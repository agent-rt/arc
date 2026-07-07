set shell := ["bash", "-euo", "pipefail", "-c"]

# Host-buildable crates (arc-runner is Windows-only: xcap / windows-rs).
CRATES := "-p arc-proto -p arc-net -p arc-relay -p arc-cli -p arc-runner-core -p arc-runner-android"

# Default: show available recipes.
default:
    @just --list

# Fast compile check (host crates).
check:
    cargo check {{CRATES}} --all-targets

# Run tests (host crates).
test:
    cargo test {{CRATES}}

# Lint with clippy, deny warnings (host crates).
lint:
    cargo clippy {{CRATES}} --all-targets -- -D warnings

# Format sources.
fmt:
    cargo fmt --all

# Check formatting (CI-friendly).
fmt-check:
    cargo fmt --all -- --check

# Build the controller release binary (arc; `arc --mcp` is the MCP server).
build:
    cargo build --release -p arc-cli

# Run arc with arbitrary args, e.g. `just run -t win shell --cmd ver`.
run *ARGS:
    cargo run --quiet -p arc-cli -- {{ARGS}}

# Full pre-commit gate (host).
ci: fmt-check lint test

# Cut a release: bumps version across the workspace, commits, tags, pushes.
# Requires cargo-release. Usage: just release 0.1.1
release VERSION:
    @command -v cargo-release >/dev/null 2>&1 || { echo "install cargo-release first: cargo install cargo-release"; exit 1; }
    cargo release {{VERSION}} --execute

# Cross-compile the APK's Rust (arc-adb) to android/arm64 (see scripts/build-android.sh).
android-build ABI="arm64-v8a":
    scripts/build-android.sh {{ABI}}

# Build the debug APK: cross-compile the .so + runner, stage them, gradle assemble.
android-apk:
    # Release-stripped (profile.release: lto + strip) — the runner drops from
    # ~87 MB to a few MB, so the on-device push is near-instant.
    scripts/build-android.sh arm64-v8a 24 -- --release
    cargo ndk -t arm64-v8a build --release -p arc-runner-android
    mkdir -p android/app/src/main/jniLibs/arm64-v8a android/app/src/main/assets
    cp target/aarch64-linux-android/release/libarc_adb.so android/app/src/main/jniLibs/arm64-v8a/
    cp target/aarch64-linux-android/release/arc-runner-android android/app/src/main/assets/
    cd android && ./gradlew :app:assembleDebug
    @echo "APK: android/app/build/outputs/apk/debug/app-debug.apk"
