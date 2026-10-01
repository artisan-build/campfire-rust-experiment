// Laravel Cloud has no Rust runtime. It detects a Go application from a go.mod at the repository
// root and runs the single binary the build produces, which is the closest thing Cloud offers to
// "run this executable". This module exists only so that Cloud picks that runtime; main.go is a
// launcher for the prebuilt Rust binary, not an application.
module github.com/artisan-build/campfire-rust-experiment

go 1.25
