#!/bin/sh
# scripts/fetch-or-build.sh — herdr [[build]] step for jonasbaeumer.file-annotator.
#
# `herdr plugin install` clones the repo and runs this with cwd = the plugin root (the
# clone) and HERDR_PLUGIN_ROOT set; `herdr plugin link` (local dev) never runs [[build]]
# steps at all, so this only ever runs for real installs.
#
# Fast path: download the release tarball matching this plugin's declared VERSION (the
# `version` field in herdr-plugin.toml) and this platform's Rust target triple, verify its
# SHA-256, extract it, and install the binary to bin/herdr-annotator under the plugin root.
#
# Fallback: on ANY miss — unmapped platform, missing curl, network/download error, missing
# release asset, checksum mismatch, no sha-256 tool — print a one-line reason to stderr and
# build from source with `cargo build --release --locked` instead, installing
# target/release/herdr-annotator to bin/. If cargo is also unavailable, exit 1 naming both
# failures. A binary only ever lands in bin/ after it has been verified (fast path) or built
# (fallback) — never partially.
set -eu

repo="JonasBaeumer/herdr-file-annotator"
bin_name="herdr-annotator"

plugin_root="${HERDR_PLUGIN_ROOT:-$(pwd)}"
cd "$plugin_root"

manifest="$plugin_root/herdr-plugin.toml"
out_dir="$plugin_root/bin"
out="$out_dir/$bin_name"

have() { command -v "$1" >/dev/null 2>&1; }

# Build from source. $1, if non-empty, is the fast-path miss reason that led here, so a
# missing-cargo failure can name both causes.
build_from_source() {
  fast_path_reason="${1:-}"
  [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
  if ! have cargo; then
    if [ -n "$fast_path_reason" ]; then
      echo "$bin_name: $fast_path_reason; and cargo was not found for the source-build fallback. Install Rust from https://rustup.rs then re-run: herdr plugin install $repo" >&2
    else
      echo "$bin_name: cargo was not found. Install Rust from https://rustup.rs then re-run: herdr plugin install $repo" >&2
    fi
    exit 1
  fi
  cargo build --release --locked
  mkdir -p "$out_dir"
  rm -f "$out"
  install -m 0755 "$plugin_root/target/release/$bin_name" "$out"
  echo "$bin_name: built from source, installed to bin/$bin_name"
  exit 0
}

fallback() {
  echo "$bin_name: $1 — falling back to building from source." >&2
  build_from_source "$1"
}

download() { # download <url> <dest>
  have curl && curl -fsSL -o "$2" "$1"
}

verify_checksum() { # verify_checksum <dir> <sumsfile>
  if have shasum; then
    (cd "$1" && shasum -a 256 -c "$2")
  elif have sha256sum; then
    (cd "$1" && sha256sum -c "$2")
  else
    return 127
  fi
}

# --- resolve the target triple for this platform -------------------------------------------
os=$(uname -s 2>/dev/null || echo unknown)
arch=$(uname -m 2>/dev/null || echo unknown)
target=""
case "$os" in
  Darwin)
    case "$arch" in
      arm64|aarch64) target="aarch64-apple-darwin" ;;
      x86_64|amd64) target="x86_64-apple-darwin" ;;
    esac
    ;;
  Linux)
    case "$arch" in
      x86_64|amd64) target="x86_64-unknown-linux-musl" ;;
      aarch64|arm64) target="aarch64-unknown-linux-musl" ;;
    esac
    ;;
esac
[ -n "$target" ] || fallback "no prebuilt binary for $os/$arch"

# --- read the version this plugin declares --------------------------------------------------
version=$(grep -E '^version = "' "$manifest" 2>/dev/null | head -n 1 | sed -E 's/^version = "([^"]+)".*/\1/')
[ -n "$version" ] || fallback "could not read version from herdr-plugin.toml"

have curl || fallback "curl not found"

asset="$bin_name-v$version-$target.tar.gz"
base_url="https://github.com/$repo/releases/download/v$version"

tmpdir=$(mktemp -d 2>/dev/null) || fallback "could not create a temp dir"
trap 'rm -rf "$tmpdir"' EXIT

download "$base_url/$asset" "$tmpdir/$asset" \
  || fallback "could not download $asset (release v$version may not exist yet)"
download "$base_url/$asset.sha256" "$tmpdir/$asset.sha256" \
  || fallback "could not download $asset.sha256"

verify_checksum "$tmpdir" "$asset.sha256" >/dev/null 2>&1 \
  || fallback "checksum verification failed for $asset"

(cd "$tmpdir" && tar -xzf "$asset") || fallback "could not extract $asset"
[ -f "$tmpdir/$bin_name" ] || fallback "$asset did not contain $bin_name"

mkdir -p "$out_dir"
rm -f "$out"
install -m 0755 "$tmpdir/$bin_name" "$out"
echo "$bin_name: installed prebuilt v$version ($target), verified SHA-256, to bin/$bin_name"
exit 0
