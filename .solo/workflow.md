# Workflow: campfire-rust-experiment

**THROWAWAY EXPERIMENT.** This is a fork of basecamp/once-campfire-rust (git remote `upstream`). The question it
answers: *can the Rust port of Campfire run on Laravel Cloud, and does its cost and performance edge
over the Rails version hold there?* We harvest the learnings into brain at
`~/Herd/brain/projects/campfire-rust-experiment/` and in the matching `ideas/` note. This code is never
promoted to production and is unlikely to be reused.

## Phase & mode
- phase: experiment
- default mode: A-autonomous. **Commit directly to `main`** in small commits with clear messages, and push.
  There are no PRs. This is Ed's call, 2026-10-01.
- Deploy target: the Artisan Build Laravel Cloud org (`org-9e4a1722-9442-404e-abdb-1ca55f845597`).
  Ed has pre-authorised deploys to this experiment's own Cloud app and resources. Nothing else in the
  org may be touched.

## Hard gate
- `cargo test` + `cargo clippy` for the crates you touched. Run them in the repo's Dockerfile `toolchain` stage
  (`/usr/local/bin/docker`), not on the host toolchain, so that libvips and ffmpeg match.
- Behaviour that matters is LIVE behaviour on Cloud. Tests are secondary in an experiment.

## Agent-role constraints
- none. Fleet bindings come from `~/Herd/brain/agents.json`.

## Hard rules for this repo
- Never set a Cloud env var for a Cloud-provisioned resource. DB, cache and bucket credentials are injected.
  App secrets may be set by hand. Secrets never go on disk or into git.
- `.cloud/config.json` is committed (brain standing policy).
- Never enable any GitHub workflow that publishes images.
