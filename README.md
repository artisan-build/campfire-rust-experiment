# Campfire in Rust, deployable on Laravel Cloud

**An independent, experimental fork of [`basecamp/once-campfire-rust`](https://github.com/basecamp/once-campfire-rust).**
It is not affiliated with, supported by, or endorsed by 37signals, and nothing here comes from them.
"Campfire" and "ONCE" are 37signals' product names; the MIT licence covers the code, not the names.
Treat this fork as an experiment, not a product: if you want the real thing, buy
[ONCE Campfire](https://once.com/campfire).

What this fork adds to upstream is one thing — **it runs on [Laravel Cloud](https://cloud.laravel.com)**,
built from source by Cloud's own Rust runtime, with the SQLite database and every uploaded file kept
in an object storage bucket so a deploy doesn't lose them. Two branches, two ways of getting there:

| Branch | How it gets onto Cloud | You need |
|---|---|---|
| **`main`** (this one) | Cloud compiles the repository with `cargo build --release` on its **Rust runtime** | early access to the Rust runtime |
| [`using-go-runtime`](#without-the-rust-runtime-the-using-go-runtime-branch) | a `go.mod` makes Cloud pick its Go runtime, which builds a small launcher around a **prebuilt** binary you publish yourself | nothing special, plus a machine that can build the binary |

The port itself is upstream's work, and so is everything in this README from
[`# Campfire in Rust`](#campfire-in-rust) down: what was ported, how parity was proven, the
benchmarks, and running it outside Cloud.

## Deploying to Laravel Cloud

This is the whole procedure, from an empty Cloud account to a working chat server. It takes about
fifteen minutes, most of it waiting for one build. Every step is the `cloud` CLI or the REST API;
only attaching the bucket needs either the dashboard or a raw API call.

### What you need

- **A Laravel Cloud account with early access to the Rust runtime.** Say that plainly: at the time
  of writing the Rust runtime is **not generally available, not documented, and not selectable**.
  There is no runtime option on `cloud application:create` and no runtime field on an environment.
  Cloud *detects* it, from a repository whose root looks like a Cargo workspace — which this branch
  is. If your organization doesn't have it, your environment comes up as PHP or Node and nothing
  here will build; use [`using-go-runtime`](#without-the-rust-runtime-the-using-go-runtime-branch)
  instead. Ask Laravel for access; there is no self-serve way on.
- **The `cloud` CLI**, authenticated: `composer global require laravel/cloud-cli` then `cloud auth`.
- **A GitHub, GitLab or Bitbucket account** connected to Cloud, holding your fork.
- About **$7/month**. That is one `flex-512mb` app instance; the bucket costs effectively nothing at
  chat-sized volumes. No database, no cache, no queue and no WebSocket cluster are needed — the app
  is one process with SQLite inside it.

### 1. Fork this repository

```sh
gh repo fork artisan-build/campfire-rust-experiment --clone
```

Fork rather than point Cloud at this repository directly, so that nobody else's push changes what
your server runs.

Two things about the fork are worth knowing before you deploy:

- **The `reference/` submodule is required to build.** `crates/assets` digests and embeds the
  upstream Rails app's CSS and JavaScript at compile time. `.cloud/build` checks the submodule out
  itself, so you don't have to do anything — but a fork with submodules stripped will not compile.
- **`.cloud/build` downloads three native dependencies from pinned URLs, each verified by sha256**:
  a libvips 8.16.1 build for Cloud's exact platform (a release asset on *this* repository, because
  GitHub does not copy release assets into forks), a static ffmpeg from
  [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds), and
  [Litestream](https://litestream.io). Cloud's build image has none of them and cannot install
  anything (`apt-get` isn't root there). If you would rather not depend on a release of ours, build
  your own libvips bundle with `.cloud/publish-runtime-bundle` and change the tag and checksum at
  the top of `.cloud/build`.

### 2. Create the application

```sh
cloud application:create \
  --name=campfire \
  --repository=<you>/campfire-rust-experiment \
  --source-provider=github \
  --region=us-east-2
```

Note the application id and the environment id it prints; everything else needs them. `cloud env:list <app>`
prints them again later.

Check that Cloud detected Rust, because nothing else will tell you:

```sh
cloud env:get <env> --json --fields=buildCommand
```

`cargo build --release` means **Rust**. Anything mentioning `composer`, `npm` or `go build` means
your organization does not have the Rust runtime and you should stop here.

### 3. Point the build at `.cloud/build`

```sh
cloud env:update <env> --build-command='sh .cloud/build' --force
```

The default `cargo build --release` would build every workspace member and install none of the
native dependencies. `.cloud/build` (88 lines, committed, so changing it is a commit rather than a
dashboard edit) fetches and checksums libvips, ffmpeg and Litestream into `runtime/`, checks out
`reference/`, builds `-p campfire` with the linker flags libvips needs, and installs a launcher
called `boot` into `/var/www/bin`.

Why a launcher: the Rust runtime's start command is fixed and undocumented — it runs the
first executable it finds in `/var/www/bin`, alphabetically — and the app needs the bundled libvips
on the loader's path, the bundled ffmpeg on `PATH`, and Litestream wrapped around it.
`.cloud/boot` does that, and `boot` sorts before `campfire`.

### 4. Set the environment variables

```sh
cloud env:variables <env> --action=append --key=SECRET_KEY_BASE          --value="$(openssl rand -hex 64)"
cloud env:variables <env> --action=append --key=RECOVER_UPGRADE_HEADERS  --value=1
cloud env:variables <env> --action=append --key=CAMPFIRE_STORAGE_PATH    --value=/tmp/campfire-storage
cloud env:variables <env> --action=append --key=RAILS_ENV                --value=production
```

| Variable | Why |
|---|---|
| `SECRET_KEY_BASE` | Signs sessions and Active Storage URLs. **Required**; the app refuses to boot without it. Generate it once and keep it — changing it signs everyone out and invalidates every blob URL in flight. |
| `RECOVER_UPGRADE_HEADERS=1` | **Without this there is no realtime.** Cloud's per-instance nginx blanks `Connection` and never forwards `Upgrade`, and every HTTP server decides at parse time whether a connection may become a WebSocket from exactly those two headers — so Action Cable is refused before any application code runs. Set, the app reconstructs both headers in front of its own parser. It is a workaround for the proxy, not a feature; see [Known limits](#known-limits). |
| `CAMPFIRE_STORAGE_PATH` | Where the SQLite database lives. Cloud's filesystem is wiped by every deploy, so this only has to be writable: `/tmp/campfire-storage` is. Durability comes from Litestream replicating it to the bucket, not from this path. |
| `RAILS_ENV` | Names the database file (`db/<env>.sqlite3`), as it does under Rails. `.cloud/boot` defaults it to `production`; set it explicitly so a dashboard reader can see it. |

Leave **`DISABLE_SSL` unset.** Cloud terminates TLS at its proxy and forwards
`X-Forwarded-Proto`, so the app's `assume_ssl` is what makes forgery protection and generated URLs
see https. Setting `DISABLE_SSL` turns that off and breaks sign-in.

Leave **`TLS_DOMAIN` unset** too — the app would try to get its own Let's Encrypt certificate for a
port Cloud's proxy owns. `.cloud/boot` serves plain HTTP on `$PORT` behind the proxy.

Optional: `VAPID_PUBLIC_KEY` and `VAPID_PRIVATE_KEY` (a P-256 pair in URL-safe Base64) turn on Web
Push; without them push is off and the log says so. `RAILS_LOG_LEVEL` and `APP_VERSION` behave as
they do in the upstream image. Everything else is in `crates/campfire/src/config.rs`.

### 5. Create a bucket and attach it

The bucket is not optional. It holds **both** the SQLite replica (under `campfire/`) and every
uploaded file (under `blobs/`). Without it, every deploy starts from an empty database and loses
every attachment.

```sh
cloud bucket:create \
  --name=campfire-storage \
  --region=us-east-2 \
  --visibility=private \
  --key-name=campfire \
  --key-permission=read_write \
  --allowed-origins=https://<your-environment>.laravel.cloud
```

`--allowed-origins` is required for a private bucket even though no browser ever talks to it.

Then **attach** it to the environment. The CLI cannot do this; use the dashboard (the
environment's *Storage* section) or the API:

```sh
curl -X PATCH https://cloud.laravel.com/api/environments/<env> \
  -H "Authorization: Bearer $CLOUD_API_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"filesystem_keys":[{"id":"<bucket key id>","disk":"s3","is_default_disk":true}]}'
```

Attaching is what makes Cloud inject `AWS_BUCKET`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
`AWS_ENDPOINT_URL` and `AWS_REGION`. **Set none of those by hand.** The app switches its storage
service from local disk to the bucket purely on `AWS_BUCKET` being present, and Litestream reads the
same variables, so attaching the bucket is the entire configuration. At boot the log says which it
picked:

```
storage: s3 service `local` in bucket fls-… (fls-….r2.cloudflarestorage.com, auto) under "blobs/"
```

If it says `disk service` instead, the bucket is not attached and nothing you upload will survive.

### 6. Deploy

```sh
cloud deploy <app> <env>
```

Expect **about four and a half minutes**: roughly 3m 45s of it is `cargo build --release` with fat
LTO, from scratch, because Cloud caches nothing for Rust. The 15-minute build cap is not close.

If it fails, read the build log before changing anything — it is complete, and it includes the
wrapper Cloud puts around your build command:

```sh
cloud deployment:list <env>
curl -s https://cloud.laravel.com/api/deployments/<deployment>/logs -H "Authorization: Bearer $CLOUD_API_TOKEN"
```

### 7. Finish setup in the browser

`https://<your-environment>.laravel.cloud/up` should answer **200**. Then open the site: it
redirects to the first-run wizard, where the name, email address and password you enter become the
administrator account. From there, `/account/edit` has the join link that invites everyone else.

That's it. Post a message, drag in a picture, and check that a second browser sees it without
reloading.

### Known limits

Honest ones, all measured on this deployment rather than assumed:

- **One instance, no horizontal scale.** The database is SQLite inside the process and Action
  Cable's pub/sub is in-process, so a second replica would be a second, divergent server. Keep
  `min_replicas` and `max_replicas` at 1. The upstream benchmarks are the ceiling, and it is a high
  one: 10,000 connected clients in a fifth of Rails' memory.
- **The WebSocket header workaround is required** (`RECOVER_UPGRADE_HEADERS=1`, above). While you
  are there: Cloud's nginx uses `proxy_read_timeout 20`, so a WebSocket survives only because
  Action Cable pings every 3 seconds. Verified by holding a connection idle for 90 seconds.
- **Hibernation does not appear to fire.** The environment reports `uses_hibernation: true` with a
  5-minute timeout, and the same process answered after 8 and then 25 minutes of silence. So budget
  for the instance running continuously, and don't rely on scale-to-zero.
- **Deploys take ~4.5 minutes** because Cloud keeps no cargo registry or target-directory cache
  between Rust builds. An unchanged commit recompiles from scratch.
- **Thumbnails and video posters are not byte-identical to the upstream image's.** This branch's
  libvips is built against Debian bookworm's codecs and without its Highway SIMD backend (bookworm's
  libhwy is too old), and ffmpeg is a static BtbN build rather than the Debian trixie 7.1.5 the
  upstream `Dockerfile` compiles. Everything works and every format still loads; the bytes differ,
  so the repository's storage vectors do not hold against this build. Resize speed is also lower
  without Highway.
- **A 434 MB deploy artifact**, of which 232 MB is the vendored `runtime/` directory and 65 MB the
  `reference/` submodule. Nothing in Cloud minds, but it is not small.
- **Attachments are streamed through the app, not served from the bucket.** That is deliberate (it
  keeps Active Storage's signed-URL access model exactly as Rails has it, and no browser ever holds
  a bucket credential), but it means every attachment download is an HTTPS round trip from the app
  to the bucket. Its cost under load is unmeasured.
- **No backups beyond the replica.** Litestream gives you point-in-time recovery of the database
  from the bucket; set up whatever you would normally set up on top of that.

### Without the Rust runtime: the `using-go-runtime` branch

If your organization has no Rust early access, the [`using-go-runtime`](../../tree/using-go-runtime)
branch deploys on a runtime everyone has. It is the same application; only how it gets onto Cloud
differs.

How that route works: a seven-line `go.mod` in the repository root makes Cloud detect **Go**, and
`main.go` is not the application but a ~250-line launcher. The build command downloads a **prebuilt
bundle** — the `campfire` binary plus libvips, ffmpeg and their shared libraries and a complete
glibc with its own dynamic loader — from a GitHub release, and `go build` compiles only the
launcher, which then execs the binary through that loader. Deploys take **30–60 seconds** instead of
four and a half minutes, because nothing of substance is compiled on Cloud.

The catch is in the word *prebuilt*: **you have to build and publish that bundle yourself**, from a
machine with Docker, with `.cloud/publish-bundle`, and write the release tag into
`.cloud/bundle-release`. So a deploy ships whatever binary someone last published, and the commit and
the artifact are only conventionally related — where on `main`, the deploy is reproducible from the
commit alone. Follow that branch's own notes; the steps for variables, the bucket and the first-run
wizard are the same as above.

---

# Campfire in Rust

*From here down is the upstream project's README.*

A port of [ONCE Campfire](https://github.com/basecamp/once-campfire) from Rails to Rust. It was
built to be impossible to tell apart from the Rails app: the same screens pixel for pixel, the same
protocols, and the Rails app's existing SQLite database and storage directory. With that parity
reached, it now diverges from Rails where that makes it faster or better; each divergence is listed
under [Known differences](#known-differences). Everything moved to Rust except the frontend: the CSS,
Stimulus controllers, Turbo, Lexxy and the other vendored JavaScript ship as they are, apart from
the few files in [`crates/assets/overrides/`](crates/assets/OVERRIDES.md).

The port ships as a single `campfire` executable (plus libvips and ffmpeg). It replaces Ruby, Puma,
Redis, Resque and Thruster. Against the Rails app it replaces, it serves pages, posts and real-time
delivery 20–95× faster, and holds 10,000 connected clients in a fifth of the memory.

## What was done

The Rails app lives in `reference/` as a git submodule, pinned to the commit being matched. It is
the oracle for everything: no expected output was written by hand. Golden vectors, screenshots and
protocol recordings all come from running the real Rails app.

**The port (`crates/`)**

| Crate | What it replaces |
|---|---|
| `ruby` | The Ruby string behaviour the other crates rely on (ERB escaping, `String#to_i` and `#to_f`, `Float#to_s`, `CGI.escape`, Rack's byte ranges), checked against Ruby itself |
| `rails_compat` | Rails' signed and encrypted cookies, signed IDs, signed global IDs, Turbo stream names and bcrypt, byte-compatible with Rails so sessions carry over |
| `kit` | Rack, Action Dispatch and Thruster, on Axum: Rails-style nested params, sessions, flash, format negotiation, forgery protection by `Sec-Fetch-Site`, ETags and gzip built from a page's cached parts, plus an in-process front server with TLS and ACME, HTTP/2 and Thruster's response cache |
| `db` | Active Record over the existing schema (rusqlite), with the same callbacks, timestamps and STI values, and a Rails-compatible fixture loader |
| `richtext` | The Action Text pipeline: sanitizing, mentions, opengraph embeds and autolinking, byte-identical to Rails on a 658-case corpus apart from the deliberate differences below |
| `storage` | Active Storage: the same blob keys and disk layout, variants (libvips) and video previews (ffmpeg) with byte-identical thumbnails, and its service layer — local disk or an S3-compatible bucket |
| `cable` | The Action Cable protocol server and pub/sub, frame-for-frame with Rails, on a WebSocket implementation of its own that shares and compresses broadcasts |
| `assets` | Propshaft and importmap-rails, with identical fingerprinted filenames and tags |
| `views` | The ERB templates as Askama templates at the same paths, DOM-identical apart from the deliberate differences below |
| `routes` | Path helpers from `config/routes.rb` |
| `campfire` | Every controller, the channels, the jobs, Web Push, opengraph unfurling, bot webhooks and search |

The plan behind it, including why it uses Axum and why pixel parity is tested the way it is, is in
[`plans/rust-conversion.md`](plans/rust-conversion.md).

**How parity is proven (`parity/`)**

- A Playwright harness runs the Rails app and the Rust app side by side in pinned containers, on
  identical seed data generated by the Rails app, with frozen clocks. It compares 225 screen states
  across Chromium, Firefox and WebKit, four viewports, light and dark mode, and the CSS
  breakpoints.
- For every state it compares:
  - the server HTML
  - the live DOM
  - the accessibility tree
  - every subresource the page loads
  - the Action Cable frames
  - the screenshot, pixel for pixel with zero tolerance
- Before any Rust code existed, the harness had to show the Rails app matching itself, so that
  nondeterminism couldn't hide real differences.

Results when the port was finished, before it started to diverge:

| Check | Result |
|---|---|
| Rust vs Rails, full matrix (all engines, viewports, schemes and breakpoints, 5 seeds) | All 5,018 cells compared; every cell passes on the fixed build |
| Rust vs Rails, lean gate (the default per-change check) | 970/970, 0 flaky, nothing allowlisted |
| Response header shape, about 70 request types | 0 differences |
| Rollback | Rails boots on, reads, searches and edits a database the Rust app wrote |

The only thing masked then was the random join code on the first-run screen. Since the port began
to diverge, the harness also leaves out the CSRF tags Rails renders, masks the digests of the files
in `crates/assets/overrides/`, and ignores the session cookie writes and the manifest body that now
differ on purpose; everything else still has to match. The latest lean gate, for the release that
made the repository public, passed 873 of 874 cells in Chromium, Firefox and WebKit; the one left is
the web app manifest, allowlisted as a deliberate difference (`parity/allowlist.yml`).

## Performance

These numbers come from benchmarking the [`v0.1.2`](https://github.com/basecamp/once-campfire-rust/releases/tag/v0.1.2)
image against the Rails app: production images of both, the same seed data, the same 4 pinned
hardware threads, host networking, and 3 interleaved runs per app. The medians are below; the full
tables with spreads are in
[`bench/results/v0.1.2-20260929/report.md`](bench/results/v0.1.2-20260929/report.md). The host ran
other work on other cores during the run. The Rust app's throughput stays within a few percent
between runs and its process memory within about 10%. The container's memory (`memory.current`,
which counts the page cache) and a few latency and connect-time cells vary more; the report has
every range. Some of Rails' numbers swung widely (noted below).

### Throughput (16 concurrent clients)

| Route | Rails | Rust | Rust advantage |
|---|---|---|---|
| Room page | 212 req/s | 20,213 req/s | **95×** |
| Messages page (`?before=`) | 411 req/s | 23,092 req/s | **56×** |
| Sidebar | 529 req/s | 22,634 req/s | **43×** |
| Search | 390 req/s | 23,276 req/s | **60×** |
| Post a message | 264 req/s | 5,494 req/s | **21×** |
| `/up` | 4,062 req/s | 131,390 req/s | **32×** |

### Latency

| Measurement | Rails | Rust | Rust advantage |
|---|---|---|---|
| Room page p50, one client | 10.4 ms | 0.19 ms | **54×** |
| Room page p99, 64 clients | 461 ms | 5.2 ms | **88×** |
| Post a message p99, one client | 13.8 ms | 1.70 ms | **8×** |
| Post a message p99, 64 clients | 385 ms | 17.5 ms | **22×** |
| Upload a 505 KB JPEG until its thumbnail is served | 122 ms | 29.1 ms | **4.2×** |

### Real time (Action Cable, up to 10,000 clients in one room)

| Measurement | Rails | Rust | Rust advantage |
|---|---|---|---|
| Deliveries per second, 100 clients | 7,858 | 305,061 | **39×** |
| Deliveries per second, 1,000 clients | 12,328 | 513,332 | **42×** |
| Deliveries per second, 5,000 clients | 10,256 | 594,554 | **58×** |
| Deliveries per second, 10,000 clients | 9,485 | 656,183 | **69×** |
| Post to all 1,000 clients received, p50 | 101 ms | 6.4 ms | **16×** |
| Post to all 10,000 clients received, p50 | 3,635 ms* | 40 ms | **91×*** |
| Post to all 10,000 clients received, p99 | 5,489 ms* | 57 ms | **96×*** |
| Connect and subscribe 10,000 clients | 29.2 s | 1.6 s | **18×** |

Every Rust client subscribed in every run; Rails missed one of 10,000 in one run. \* Rails' 10,000-client delivery latency
swung between runs (p50 from 2.5 to 5.0 s); in the previous benchmark it was 1.2 s at p50 and 1.5 s
at p99, which would make these ratios about 29× and 27×.

### Startup and memory

| Measurement | Rails | Rust | Rust advantage |
|---|---|---|---|
| Cold start (`docker run` until `/up` answers) | 2,607 ms | 143 ms | **18×** |
| Idle memory (container) | 355 MB | 15 MB | **24×** |
| App process, 1,000 idle cable clients (Pss) | 656 MB | 186 MB | **3.5×** |
| App process, 10,000 idle cable clients (Pss) | 1,469 MB | 327 MB | **4.5×** |
| App process, 10,000 cable clients under load (Pss) | 2,199 MB | 324 MB | **6.8×** |
| Whole container, 10,000 cable clients under load (Pss) | 3,340 MB | 324 MB | **10×** |
| Image size, unpacked | 933 MB | 169 MB | **5.5×** |
| Image size, compressed download | 359 MB | 67 MB | **5.4×** |

Rails' whole container adds Redis and Thruster to its app processes; the Rust app is one process.

### Since the previous benchmarks

The run before these benchmarked `main` at `898653e` the same way
([`bench/results/scale-20260927`](bench/results/scale-20260927/report.md)). Since then came
[cached page parts](#gzip-and-etags-from-cached-page-parts), [the new WebSocket
layer](#100000-clients-and-a-raspberry-pi-5), opting out of transparent huge pages
([`bench/results/thp-20260928`](bench/results/thp-20260928/report.md)), and keeping every page's
compressed form ([`bench/results/whole-page-parts-20260929`](bench/results/whole-page-parts-20260929/summary.md)):

| Rust app | `898653e` | `v0.1.1` | `v0.1.2` |
|---|---|---|---|
| Room page, 16 clients | 6,002 req/s | 20,479 req/s | 20,213 req/s |
| Sidebar, 16 clients | 11,550 req/s | 12,642 req/s | **22,634 req/s** |
| Search, 16 clients | 8,970 req/s | 23,399 req/s | 23,276 req/s |
| Deliveries per second, 10,000 clients | 379,608 | 638,688 | 656,183 |
| Post to all 10,000 clients received, p50 | 42 ms | 40 ms | 40 ms |
| Idle memory (container) | 47 MB | 15 MB | 15 MB |
| App process, 10,000 idle cable clients (Pss) | 582 MB | 313 MB | 327 MB |
| App process, 10,000 cable clients under load (Pss) | 876 MB | 310 MB | 324 MB |

The `v0.1.1` numbers are from [`bench/results/v0.1.1-20260928`](bench/results/v0.1.1-20260928/report.md).

### 100,000 clients, and a Raspberry Pi 5

One Campfire holds 100,000 connected clients in 1.5 GB: every one connects in about 13 s, and
memory stays flat while messages fan out to all of them. On a Raspberry Pi 5's CPU budget
(emulated: four pinned cores capped at 1.2 cores' worth), the app delivered over a million
messages a second to those clients, the load generator's limit, using 0.71 of its 1.2 cores. What
limits a Pi is its gigabit Ethernet: about 51,000 compressed deliveries a second. That's 100,000
chatters in rooms of 100, each posting every five minutes, with a third to spare; a single room of
100,000 can't be busy on one gigabit link. Details and caveats in
[`bench/results/pi-100k-20260928/report.md`](bench/results/pi-100k-20260928/report.md).

| At 100,000 clients | Before | After |
|---|---|---|
| Memory, idle | 3.2 GB | 1.5 GB |
| Memory, while fanning out | 5.9 GB | 1.6 GB |
| A message on the wire | ~10 KB | ~2.3 KB (compressed) |
| Posting while a message fans out to everyone, p50 | 637 ms | 43 ms |

What changed: Action Cable sockets use their own small WebSocket implementation, which writes each
broadcast's shared bytes to every socket without copying them per connection, and compresses a
broadcast once for all of its subscribers (`permessage-deflate`, which browsers offer). Connections
run on threads of their own, so page loads and posts don't queue behind a fan-out. The app also
raises its own open-file limit, which in Docker would otherwise stop it at 65,536 clients.

### Where the speed came from

A straight translation was already 3–10× faster than Rails. Profiling (in
[`plans/perf-attribution.md`](plans/perf-attribution.md)) then showed where the time went, and each
change since has been measured before and after, keeping the test suite and the parity gate green.
In the order they landed:

| Change | Effect |
|---|---|
| Look up a message's cached fragment before building its view, as Rails' `cache` does | Room page 989 → 1,664 req/s; messages page 1,116 → 2,234 req/s |
| gzip on the zlib-rs backend instead of miniz_oxide | Room page 662 → 955 req/s |
| Run SQLite WAL checkpoints on their own thread, off the writer | POST p99 at one client: 12.5 → 1.7 ms |
| Cache prepared statements for every query | POSTs about 10% faster |
| Share cached fragments instead of copying them; build stylesheet tags once per process | 6–20% less CPU per page |
| Cable: 4 KiB read buffers instead of zero-filling 128 KiB per read; encode each broadcast once and share it across subscribers; batch socket writes | 4.7× less CPU per delivery; half the latency and memory under fan-out |
| Fat LTO, one codegen unit, jemalloc | A further 5–14% per route |
| Splice precompressed messages into gzipped pages ([below](#gzip-and-etags-from-cached-page-parts)) | Room page 2,527 → 5,461 req/s; messages page 3,709 → 16,523 req/s; search 2,123 → 5,526 req/s |
| Forgery protection by `Sec-Fetch-Site` instead of CSRF tokens ([Known differences](#known-differences)) | Room page +9%, messages page +6%, search +10%; pages render the same until their content changes, so revalidation gets a 304 |
| Index messages by `(room_id, created_at)`; check "more than a page" without counting the room | In a room with 236k messages: room page 95 → 6,051 req/s (64×), messages page 87 → 17,972 req/s (208×). Before, a room page sorted the room's whole history, so rooms slowed as they grew; now a long room serves as fast as a new one |
| Cache every part of a page, not just its messages, and take the ETag from the parts ([below](#gzip-and-etags-from-cached-page-parts)); send cookies only when they change | Room page 2.9×, search 2.8×, messages page 1.3× |
| Cable: own WebSocket framing with shared, once-compressed frames; connections on their own runtime ([above](#100000-clients-and-a-raspberry-pi-5)) | 100,000 clients in 1.6 GB instead of 5.9 GB while fanning out; a post during a 100,000-client fan-out 637 → 43 ms; frames 10 KB → 2.3 KB on the wire |
| Keep the compressed form of every page, not only pages with cached messages ([`bench/results/whole-page-parts-20260929`](bench/results/whole-page-parts-20260929/summary.md)) | Sidebar 10,683 → 19,400 req/s (1.8×); it spent 41% of its CPU compressing the same page again |
| No transparent huge pages for the process or jemalloc ([`bench/results/thp-20260928`](bench/results/thp-20260928/report.md)) | Idle memory 37 → 11 MB on two cores and 160 → 15 MB on 32, where the kernel's THP setting is `always`; throughput unchanged |

Against Rails, the room page went from 4.4× in the preliminary benchmark to 95× in the latest one.

### gzip and ETags from cached page parts

Every response is gzipped at level 6, as Rails' `Rack::Deflater` does, and after the passes above
that was 60–76% of the CPU on large pages. Most of a room page is cached messages, whose bytes are
the same on every request, so the app stopped compressing them per request, in two steps:

1. **Spliced gzip.** Each cached message is compressed once and kept, and pages splice the stored
   pieces into the gzip stream. Compressing each message on its own would make a room page 4.4×
   larger, because consecutive messages share most of their markup, so each piece is compressed
   against the message before it as a preset dictionary, and reused only when that same message
   (with the same text between them) comes before it again: the steady state for a room page. The
   layout around the messages was still compressed live, because every page carried a fresh CSRF
   token.
2. **Cached page parts.** Without CSRF tokens (see [Known differences](#known-differences)), a page
   renders byte for byte the same until what it shows changes, so the layout can be stored too. A
   page is now split into parts that cover it end to end: its cached messages and the text between
   them. Each part is compressed once, against the part before it, and kept under the part's
   identity (the cached fragment, or the SHA-256 of the text) and its predecessor's; a message keeps
   pieces for the few predecessors it's seen with (its room, a page of older messages, search
   results). The ETag comes from the parts' digests instead of a SHA-256 over the whole body.

For a 466 KB room page, gzip and the ETag took ~1,200 µs per request at first, ~460 µs after
splicing, and 42 µs now; the first request after a page changes pays ~2 ms, once, to compress its
new parts. The decoded body is unchanged, and the compressed page is within 1% of compressing it
whole.

| Route (16 clients) | Before | Spliced gzip | Cached page parts |
|---|---|---|---|
| Room page | 2,527 req/s | 5,461 req/s | 16,881 req/s |
| Messages page (`?before=`) | 3,709 req/s | 16,523 req/s | 22,580 req/s |
| Search | 2,123 req/s | 5,526 req/s | 16,097 req/s |

Each step was measured natively against the commit before it, in its own session, so the columns
come from different runs (the page-parts run on a busy host, which understates it). Details in
[`bench/results/splice-20260927`](bench/results/splice-20260927/report.md),
[`bench/results/header-csrf-20260927`](bench/results/header-csrf-20260927/report.md) and
[`bench/results/page-parts-20260927`](bench/results/page-parts-20260927/report.md).

## Running it

It's a drop-in replacement for the Rails image: the same environment variables, ports and storage
layout. Point it at an existing Campfire's storage and everyone stays signed in.

With [ONCE](https://github.com/basecamp/once), on any server with Docker:

```sh
once deploy ghcr.io/basecamp/once-campfire-rust --host chat.example.com
```

ONCE provides the secrets, TLS, backups and upgrades. The image is published for amd64 and arm64:
`:latest` and a version tag for each [release](https://github.com/basecamp/once-campfire-rust/releases),
and `:main` for every change to `main` (see [`.github/workflows`](.github/workflows)).

Or with Docker alone:

```sh
docker run -d -p 80:80 -p 443:443 \
  -e SECRET_KEY_BASE=... -e VAPID_PUBLIC_KEY=... -e VAPID_PRIVATE_KEY=... \
  -e TLS_DOMAIN=chat.example.com \
  -v campfire:/rails/storage \
  ghcr.io/basecamp/once-campfire-rust
```

- **TLS:** with `TLS_DOMAIN` set, the app gets and renews its own Let's Encrypt certificate. It
  keeps certificates where Thruster did, so an existing install keeps its certificate.
- **Plain HTTP:** set `DISABLE_SSL` instead, for running behind another proxy.
- **Web Push:** `VAPID_PUBLIC_KEY` and `VAPID_PRIVATE_KEY` are a P-256 key pair in URL-safe Base64
  (as the Rails image takes them). They're checked at boot; without a valid pair, push
  notifications are off and the log says why. `VAPID_SUBJECT` is the contact push services see (a
  `mailto:` or `https:` URL); it defaults to `https://` and your `TLS_DOMAIN`.
- **The app port:** as with Puma behind Thruster, the app also answers on `TARGET_PORT` (3000)
  without the front server's cache and compression, but only on loopback. Set `TARGET_BIND`
  (e.g. `0.0.0.0`) to open it further; it trusts `X-Forwarded-*` from whoever reaches it.
- **Storage:** everything lives under `/rails/storage`: the SQLite database, uploaded files and
  backups.
- **Media:** the image builds libvips 8.16.1 (thumbnails and other variants) and ffmpeg 7.1.5
  (video posters, and ffprobe for video and audio metadata) from the Debian trixie source packages
  the Rails image installs, with the same flags and libraries, so thumbnails and posters are byte
  for byte the ones Rails makes. Only what Campfire can reach goes in: libvips loads PNG, GIF,
  JPEG, TIFF, WebP, AVIF and HEIC/HEIF (with EXIF orientation and ICC profiles) and saves PNG,
  JPEG, GIF and WebP; ffmpeg keeps every built-in demuxer and decoder plus dav1d for AV1, the
  filters that pick and orient a poster frame, and only the MJPEG encoder and `image2` muxer that
  write it; other encoders and muxers, hardware, network and external codec libraries are left
  out. That took the image from 640 MB to 169 MB unpacked, and from 246 MB to 67 MB to download
  (see the [`Dockerfile`](Dockerfile)).
- **Many clients:** every connected browser is a socket, and the app raises its open-file limit to
  the hard limit at startup (Docker's default soft limit would stop it at 65,536). Past that, it's
  memory (~15 KB per client) and bandwidth; see
  [100,000 clients](#100000-clients-and-a-raspberry-pi-5).
- **ONCE hooks:** `/hooks/pre-backup` runs `campfire backup`, which uses SQLite's online backup API.
- **Other options:** see `crates/campfire/src/config.rs`.

To build the image yourself: `docker build -t campfire-rust .` (the `reference/` submodule must be
checked out). For development:

```sh
cargo test --workspace --exclude html5ever   # all crates
cargo run -p campfire -- server               # needs SECRET_KEY_BASE or SECRET_KEY_BASE_DUMMY=1
```

## Verifying changes

```sh
parity/bin/reference build && parity/bin/candidate build   # Rails and Rust images
parity/bin/seed build                                      # seed data, generated by the Rails app
parity/bin/candidate compare                               # lean parity gate, Rust vs Rails
parity/bin/compare --matrix full ...                       # full matrix, for release checks
reference-tools/http_shape/sweep.py <rails-url> <rust-url> # response header shape
bench/run                                                  # benchmark both apps
bench/results/pi-100k-20260928/run100k.sh BIN LABEL        # 100,000 cable clients (PI=1: a Pi 5's budget)
```

[`parity/SCREENS.md`](parity/SCREENS.md) documents the screen inventory, masks and the flake policy.
[`AGENTS.md`](AGENTS.md) describes the repository layout and working rules, and
[`CONTRIBUTING.md`](CONTRIBUTING.md) how to propose changes. Report security issues as
[`SECURITY.md`](SECURITY.md) describes.

## Known differences

Deliberate:

- **Compressed WebSocket frames.** The app accepts the `permessage-deflate` compression browsers
  offer (without context takeover, so each broadcast is compressed once for all of its
  subscribers); Rails' Action Cable doesn't negotiate it. The frames decode to the same messages.
- **No CSRF tokens.** Forgery protection checks the `Sec-Fetch-Site` header browsers send, as Rails
  main's `protect_from_forgery using: :header_only` does, instead of per-request tokens. Writes are
  accepted from `same-origin` and `same-site` requests; `cross-site` ones, and HTTPS requests
  without the header, get a 422. The `Origin` check still applies. On plain HTTP, where browsers
  don't send the header, a missing one is accepted, and the `SameSite=Lax` session cookie and the
  `Origin` check protect writes. Pages have no `csrf-token` meta tag or `authenticity_token` fields,
  so they render byte for byte the same until what they show changes: ETags now match on
  revalidation, and a page's markup can be cached. Browsers from before 2023 that don't send the
  header (e.g. Safari before 16.4) can't submit forms over HTTPS. Tabs opened before an upgrade keep
  working: their tokens are ignored, and the header does the job.
- **Redis and Resque are gone.** Jobs run in-process and are best-effort: a crash loses queued
  webhooks and pushes, as a Redis restart would under Rails. Each kind of job (pushes, webhooks,
  purges, ...) has its own queue and `JOB_CONCURRENCY` workers, so a slow bot's webhooks can't hold
  up push notifications.
- **Push subscriptions are kept through our own failures.** Rails destroys a push subscription on
  any OpenSSL error, which includes a bad VAPID key and any TLS failure (an empty CA store, a skewed
  clock), so a configuration mistake deleted everyone's subscriptions on the next message. The
  VAPID keys are now checked once at boot (Web Push is off, with a log line, when they're missing
  or don't form a key pair), and a subscription is destroyed only when the push service answers
  410 or 404 (RFC 8030; Rails keeps it on a 404) or its own key isn't a valid P-256 point.
- **Long messages still get push notifications.** Rails puts the whole message in the notification,
  and one over about 4 KB fails to encrypt (a Web Push message holds 4096 bytes), so nobody is
  notified. The notification's body is now cut short with an ellipsis at 3 KB, and its title at 256
  bytes.
- **The VAPID subject is configurable.** Rails identifies every install to push services as
  `mailto:support@37signals.com`; this uses `VAPID_SUBJECT`, or `https://` and the first
  `TLS_DOMAIN`, or the project's URL.
- **Cookies are only sent when they change.** Rails rewrites the session cookie, re-signs the
  `session_token` cookie and re-sets `last_room` on nearly every response. The session cookie is now
  written only when the session changed, and deleted once it's empty (it only holds the flash and a
  return-to URL); `session_token` is re-signed when the session's hourly activity refresh runs, which
  keeps its 20-year expiry rolling; `last_room` is set when it changes. An authenticated request whose
  session doesn't need that refresh also no longer passes through the database writer.
- **ETags aren't a digest of the body** on pages of 1 KB or more: they're a SHA-256 over the page's
  parts (its cached messages and the text around them, or the whole body as one part). Identical
  pages still get identical ETags, and any change gets a new one.
- **One more index.** On boot the app adds `index_messages_on_room_id_and_created_at` to the Rails
  schema if it's missing (a one-time 49 ms for 236k messages). Rails' schema pages a room's messages
  through `index_messages_on_room_id` alone, which sorts the room's whole history for every page.
  The index is additive, so the database still works with the Rails image.
- **Blobs can live in an S3-compatible bucket.** `crates/storage` had one disk service where Active
  Storage has a service layer; it now has a `Service` over the local disk and an S3 one, picked by
  `CAMPFIRE_STORAGE_SERVICE` or, when that is unset, by whether `AWS_BUCKET` and the rest of the
  `AWS_*` group are in the environment. Credentials are read from there and nowhere else. Objects
  are named `<prefix><blob key>` (`CAMPFIRE_STORAGE_S3_PREFIX`, default `blobs/`), so the keys in
  `active_storage_blobs` mean the same thing in either service. Unlike Active Storage's `:amazon`
  service, blobs are **not** served from presigned bucket URLs: both services keep the app's own
  signed `/rails/active_storage/disk/...` route and the app streams the bytes, so the access model,
  the signature and its expiry are unchanged and no browser ever touches the bucket. Direct uploads
  still `PUT` to the app behind `require_active_storage_authentication`. Uploads to a bucket are
  checksum-verified against the *source* before anything is written, rather than written and then
  undone, both because that is stricter and because Cloudflare R2 rejects a `PUT` carrying a
  `Content-MD5` alongside an `x-amz-checksum-*`. `campfire storage:list [prefix]` prints what the
  configured service holds.
- **Leaner libvips and ffmpeg.** The image builds both from the same Debian sources as the Rails
  image, leaving out what Campfire can't reach (see [Running it](#running-it)). Thumbnails, video
  posters and metadata come out byte for byte the same for every image and video format either
  image handles. libvips loses only loaders that `Vips.block_untrusted` already blocks
  (ImageMagick, SVG, PDF, JPEG XL, JPEG 2000, OpenEXR, FITS, Matlab, OpenSlide). ffmpeg loses the
  decoders and demuxers that come from external libraries with no built-in equivalent: tracker
  modules (libopenmpt), game-console music (libgme), JPEG XL and SVG frames, codec2 speech,
  teletext subtitles, and DASH/IMF manifests. Tracker modules and game-console music attached to
  a message are now stored without duration or bit rate, which Campfire never shows.
- **Limits where Rails had none, or raised.** Request bodies other than file uploads are capped at
  16 MiB (a 413), and so are Active Storage direct uploads, which Campfire's editor doesn't use:
  asking for a larger one is a 413, and for one whose byte size isn't a number a 422 (Rails made
  it 0). A QR code for more than a QR code can hold is a 422, not a 500.
  Page numbers are capped at a billion. A WebSocket connection holds up to 64 subscriptions with identifiers of up to
  4 KiB, takes messages of up to 1 MiB (Rails' websocket-driver takes 64 MiB, and both close with
  1009 past it), and a client that doesn't read what it's sent for 30 seconds is disconnected.
  Deactivating or banning a user closes their open connections once the change commits.
- **Link unfurling is bounded in time.** Rails gives each connect and read of an unfurl 60
  seconds, across up to 10 redirects and the image check. Now an unfurl gets 10 seconds in all and
  5 per connect or read, and a page that takes longer unfurls nothing. At most 16 unfurls run at
  once, and only a `meta` tag's first 256 attributes are read.
- **Bot webhooks are bounded.** A delivery gets 60 seconds in all, on top of Rails' 7 per connect
  or read; one that runs out answers "Failed to respond within 60 seconds", as a 7-second timeout
  answers with its own. A reply larger than 100 MB (after decompression) fails the delivery and
  posts nothing; Rails read replies of any size into memory.
- **Push deliveries are bounded in time.** A push service gets 10 seconds per connect or read and 30
  in all, where the web-push gem leaves `Net::HTTP`'s 60 seconds per step; a slow service would
  otherwise hold one of the few push workers for minutes.
- **The front server is stricter than Thruster.** The app's own listener on `TARGET_PORT` binds
  loopback only (Puma bound every interface) and has the front's timeouts and `MAX_REQUEST_BODY`
  (see [Running it](#running-it)). The response cache counts its keys toward `CACHE_SIZE`, skips
  URIs longer than 2 KB, keys on the raw path and query, and lets range requests through to the app
  instead of answering them with a whole cached body. Thruster decoded the path, so `/a%2Fb` and
  `/a/b` shared an entry. It also sorted and re-escaped the query and dropped any pair containing
  `;`, so `?disposition=attachment;`, which the app reads, shared the entry of no query at all.
  Between requests, an HTTP/1 keep-alive connection closes once the shorter of `HTTP_IDLE_TIMEOUT`
  and `HTTP_READ_TIMEOUT` has passed since the previous response, unless the next request's headers
  have arrived, because hyper's header timer runs while the connection waits. Thruster waited the
  idle timeout for the next request's first bytes and then gave it the whole read timeout. With the
  image's settings (60 and 300 seconds) idle connections close after 60 seconds either way; without
  them the defaults are 60 and 30, so an idle HTTP/1 connection closes after 30 seconds. HTTP/2
  connections get the idle timeout.
- **A response header line holding a DEL is left out.** Header values that come from the request,
  like `?disposition=` on a proxied blob, go out as Puma writes them: line by line, leaving out the
  lines that hold control characters. Puma writes a DEL, and Thruster turns the response into a
  502; the app leaves that line out like the others.
- **Media is processed off the database writer.** Rails saves a blob's row and then uploads its
  file after commit; here the upload is copied into storage first, straight from the request's
  tempfile, and deleted again if the save fails. Rails' disk service then reads the copy back to
  check its MD5 against the checksum it just computed from the same bytes; the app skips that
  second pass, and checks the file as before whenever it's opened for analysis, a variant or a
  poster. A direct upload is still checked against the checksum its client gave. Variants, video
  posters and analysis run on background threads (at most four at a time), and only their rows
  are written in a transaction, so a large image or video doesn't hold up other writes. A variant
  or poster is saved already analyzed, where Rails analyzes it in a job after commit; the rows end
  up the same. Two requests for the same missing variant may both transform it: the first to save
  wins and the other's file is deleted. ffmpeg is stopped after 60 seconds of drawing a poster and
  ffprobe after 30 seconds of reading a file, which Rails doesn't limit.
- **Floats in JSON have the shortest digits.** Rails writes them with the json gem's Grisu2,
  which now and then picks a longer form of the same number (`250.70174600000001` for
  `250.701746`); the app writes the shortest one, laid out the same way, in blob metadata and
  everywhere else it writes JSON. Both read back as the same number.
- **Passwords are hashed and checked outside the database.** bcrypt (about 250 ms) runs before the
  write that saves a password, and a sign-in looks the user up and then verifies the password after
  releasing the database connection. An unknown email address still costs one bcrypt, as in Rails.
- **Searches are for words.** Rails passes a search's words to SQLite's full-text `MATCH` as they
  are, so `NOT`, `AND`, `OR` or `NEAR` in the wrong place is a 500. Each word is now matched as
  itself.
- **`/rooms/directs/:id` redirects to the room** instead of answering 500.
- **An infinite q-value in `Accept` is read.** Rails answers `text/html;q=1e400, application/json`
  with a 500 (a FloatDomainError); the type with it now sorts first, or last when it's negative.
- **Edge's install instructions render.** With an EdgeHTML user agent (`Edge/`), Rails answers
  profile and room pages with a 500 because the partial names an image that isn't there
  (`install-edge.svg`); the Rust app ships it.
- **New-ping suggestions appear.** The user picker for a new ping asks for JSON; in Rails it asks
  for anything, gets HTML, and never shows a suggestion.
- **Autolinking can't break out of an attribute.** rails_autolink finds URLs and email addresses
  with regular expressions over the sanitized HTML, which Nokogiri serializes with `<` and `>` left
  raw in attribute values. A URL after a `>` in, say, a `title` was taken for text and linked, and
  the inserted `<a href="...">` closed the attribute, turning the rest of its value into live
  markup (a stored XSS; it affects the Rails app). The port escapes `<` and `>` in attribute values
  before autolinking, so URLs inside attributes stay as they were. The same DOM otherwise.
- **Cached markup doesn't carry the request's host.** A message's "Copy link" button held an
  absolute URL built from the Host header, inside a fragment cached for everyone, so one request
  with a forged Host changed the link everyone copied. The button now carries the message's path
  (`data-copy-to-clipboard-url-value`), and the copy-to-clipboard controller (an override) makes it
  absolute against the page. The bot API's cached JSON, whose URLs must be absolute, is cached per
  base URL instead.
- **Rich text drops `name` attributes.** Rails' default sanitizer allowlist keeps them, which lets
  a message clobber the page's DOM globals (`<img name="body">` shadows `document.body`). Nothing
  Campfire's composer writes has one.
- **Rich text keeps only highlight colors in `style`.** Where Rails runs `style` through Loofah's
  CSS scrubber, the sanitizer keeps only `color` and `background-color` with a plain color value
  (a keyword, hex, `rgb()`/`hsl()`, or a custom property like Lexxy's `var(--highlight-1)`), which
  is all Lexxy writes. It shows in the HTML body the bot API and webhooks send; message pages drop
  `style` altogether, as they did.
- **The web app manifest is valid JSON.** Rails HTML-escapes the account name and URLs into
  `webmanifest.json`, so a name with `\` or `"` broke the manifest and the small logo's URL read
  `?size=small&amp;v=...`. They're JSON strings now.
- **Content attachments nest at most 8 deep.** An `<action-text-attachment>` carrying HTML in its
  `content` renders that content, attachments included; each level parses and sanitizes
  everything below it again, so a 336 KB body of nested ones took 10 seconds to render. Deeper
  levels now render empty. Campfire's composer doesn't nest them at all.
- **A mention of a deleted user shows ☒.** Rails can't find a "missing" partial for users, so
  the mention raised and blanked the whole message, and editing the message raised too. The rest of
  the message now shows with ☒ in the mention's place, and the editor leaves the mention out.
- **A message whose plain text raises gets none.** Where reading a stored body's plain text raises
  in Rails (for example, an attachment whose `sgid` isn't Base64 or JSON, which Campfire's composer
  doesn't write), saving or editing the message raises after it commits: the request answers 500,
  and the message isn't indexed for search, pushed, broadcast or sent to bots. The app logs the
  error and gives the message an empty plain text (or its attachment's filename), so it goes out
  like any other; its page shows it as unrenderable, as in Rails.
- **A body past the HTML parser's limits is saved.** Rails refuses a body nested more than 400
  levels deep, or with an element carrying more than 400 attributes, as soon as it's assigned: the
  request answers 500 and nothing is saved or changed. The app stores such a body as it came, gives
  it an empty plain text as above, and its page shows it as unrenderable.
- **Not ported:** the duplicate `session_token` cookie Rails' Active Storage streaming sends; and
  legacy AES-CBC encrypted cookies, since Campfire started on GCM.
- **This fork's Cloud build has two media divergences of its own**, which the Docker image above
  does not: its libvips is compiled against Debian bookworm's codecs and without the Highway SIMD
  backend, and its ffmpeg is a static BtbN build rather than Debian trixie's 7.1.5. So thumbnails
  and video posters from a Laravel Cloud deployment are **not** byte-identical to Rails', and the
  storage vectors don't hold against it. The `Dockerfile` is unaffected and still builds both from
  the Rails image's own sources. See [Known limits](#known-limits).

Not fully covered:

- HTTP-01 ACME validation is only unit-tested. TLS-ALPN-01 was tested end to end against a local
  ACME server.
- Rich text is checked against Rails on a 658-case corpus, 400 of them fuzzed, which matches
  exactly apart from the deliberate differences above. Active Storage attachments embedded in a
  message body, which Campfire's composer can't create, render as ☒.

## How it was built

The port was built in about a day by coordinated Claude Code agents, each owning one crate or
harness component. They followed the plan in `plans/rust-conversion.md`, which Codex also reviewed.
[`plans/overnight-report.md`](plans/overnight-report.md) logs the unattended overnight run: the
parity gate going green, the Thruster replacement, the benchmarks, and each optimization with its
before and after numbers. The optimizations and divergences since then were made the same way, one
pull request each, with their measurements in `bench/results/`.

## License

MIT, like Campfire. See [`MIT-LICENSE`](MIT-LICENSE).
