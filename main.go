// Launcher for the Rust binary on Laravel Cloud (see go.mod for why this is a Go program).
//
// Cloud's Go runtime builds `./app` and starts it. `app` is this launcher: it execs the prebuilt
// campfire binary from ./bundle, which the build command unpacks from a GitHub release. The bundle
// carries its own glibc, libvips and ffmpeg, so the binary is run through the bundle's dynamic
// loader rather than the runtime image's, which has none of them:
//
//	bundle/lib/<triple>/ld-linux-*.so.* --library-path <bundle lib dirs> bundle/usr/local/bin/campfire server
//
// Without a bundle it serves a diagnostics page instead of failing, so that a first deploy can
// report what the runtime image actually is (architecture, loader, libc, injected variables).
package main

import (
	"fmt"
	"net/http"
	"os"
	"path/filepath"
	"runtime"
	"sort"
	"strings"
	"syscall"
)

// The Debian multiarch directory and loader name for the architecture we were built for.
var loaders = map[string][2]string{
	"amd64": {"x86_64-linux-gnu", "ld-linux-x86-64.so.2"},
	"arm64": {"aarch64-linux-gnu", "ld-linux-aarch64.so.1"},
}

const bundle = "bundle"

func main() {
	if _, err := os.Stat(filepath.Join(bundle, "usr/local/bin/campfire")); err == nil {
		if err := launch(os.Args[1:]); err != nil {
			fmt.Fprintf(os.Stderr, "launcher: %v\n", err)
			os.Exit(1)
		}
	}
	fmt.Fprint(os.Stderr, diagnostics())
	serveDiagnostics()
}

// launch replaces this process with the campfire binary, run through the bundle's loader.
func launch(args []string) error {
	triple, ok := loaders[runtime.GOARCH]
	if !ok {
		return fmt.Errorf("no bundle loader for GOARCH %q", runtime.GOARCH)
	}
	root, err := filepath.Abs(bundle)
	if err != nil {
		return err
	}
	loader := filepath.Join(root, "lib", triple[0], triple[1])
	if _, err := os.Stat(loader); err != nil {
		return fmt.Errorf("bundle loader %s: %w", loader, err)
	}
	libs := []string{
		filepath.Join(root, "lib", triple[0]),
		filepath.Join(root, "usr/lib", triple[0]),
		filepath.Join(root, "usr/local/lib"),
	}

	if len(args) == 0 {
		args = []string{"server"}
	}
	argv := append([]string{loader, "--library-path", strings.Join(libs, ":"), filepath.Join(root, "usr/local/bin/campfire")}, args...)

	// ffmpeg and ffprobe are run as subprocesses and need the same loader, so the build command
	// leaves wrappers in bundle/wrap and they go first on PATH.
	env := environment(root, libs)
	return syscall.Exec(loader, argv, env)
}

// environment is the campfire process's environment: Cloud's injected variables, plus what the
// bundle and Cloud's edge imply. Anything already set in the environment wins, so every one of
// these stays overridable from the Cloud dashboard.
func environment(root string, libs []string) []string {
	env := os.Environ()
	set := func(key, value string) {
		if os.Getenv(key) == "" {
			env = append(env, key+"="+value)
		}
	}
	// Cloud injects PORT and terminates TLS at its proxy, so the binary serves plain HTTP there and
	// leaves its own TLS/ACME off (no TLS_DOMAIN). DISABLE_SSL stays unset so that assume_ssl keeps
	// forgery protection seeing https.
	if port := os.Getenv("PORT"); port != "" {
		set("HTTP_PORT", port)
	}
	set("PATH", filepath.Join(root, "wrap")+":"+os.Getenv("PATH"))
	set("SSL_CERT_FILE", filepath.Join(root, "etc/ssl/certs/ca-certificates.crt"))
	set("SSL_CERT_DIR", filepath.Join(root, "etc/ssl/certs"))
	set("LD_LIBRARY_PATH", strings.Join(libs, ":"))
	return env
}

// serveDiagnostics answers Cloud's health checks while reporting what the runtime image is. It
// prints no variable values: only the keys, so that nothing injected can leak through a public URL.
func serveDiagnostics() {
	port := os.Getenv("PORT")
	if port == "" {
		port = "8080"
	}
	report := diagnostics()
	http.HandleFunc("/up", func(w http.ResponseWriter, r *http.Request) { fmt.Fprintln(w, "ok") })
	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/plain; charset=utf-8")
		fmt.Fprint(w, report)
	})
	// Cloud's network is IPv6-only, so bind the wildcard rather than 0.0.0.0.
	if err := http.ListenAndServe("[::]:"+port, nil); err != nil {
		fmt.Fprintf(os.Stderr, "launcher: %v\n", err)
		os.Exit(1)
	}
}

func diagnostics() string {
	var b strings.Builder
	fmt.Fprintf(&b, "campfire launcher: no bundle found, serving diagnostics\n")
	fmt.Fprintf(&b, "go=%s %s/%s\n", runtime.Version(), runtime.GOOS, runtime.GOARCH)
	if wd, err := os.Getwd(); err == nil {
		fmt.Fprintf(&b, "cwd=%s\n", wd)
	}
	for _, f := range []string{"/proc/version", "/etc/os-release"} {
		if c, err := os.ReadFile(f); err == nil {
			fmt.Fprintf(&b, "--- %s\n%s", f, c)
		}
	}
	for _, d := range []string{".", "/", "/lib", "/usr/lib", "/lib/x86_64-linux-gnu", "/lib/aarch64-linux-gnu", "/usr/bin", "/tmp"} {
		fmt.Fprintf(&b, "--- ls %s\n%s\n", d, listing(d))
	}
	keys := make([]string, 0, len(os.Environ()))
	for _, kv := range os.Environ() {
		keys = append(keys, strings.SplitN(kv, "=", 2)[0])
	}
	sort.Strings(keys)
	fmt.Fprintf(&b, "--- env keys\n%s\n", strings.Join(keys, " "))
	return b.String()
}

func listing(dir string) string {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return "error: " + err.Error()
	}
	names := make([]string, 0, len(entries))
	for i, e := range entries {
		if i == 200 {
			names = append(names, "...")
			break
		}
		names = append(names, e.Name())
	}
	return strings.Join(names, " ")
}
