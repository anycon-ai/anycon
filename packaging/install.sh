#!/bin/sh
# Install the anycon CLI (daemon + client in one binary) from GitHub Releases.
#
#   curl -fsSL https://github.com/anycon-ai/anycon/releases/latest/download/install.sh | sh
#
# Environment (set it on the `sh` side of the pipe, e.g. `... | ANYCON_VERSION=0.1.0 sh`):
#   ANYCON_VERSION       version to install, e.g. 0.1.0 (default: the release this script came from)
#   ANYCON_INSTALL_DIR   where to put the binary (default: ~/.local/bin)
#
# Supported: macOS (Apple silicon, Intel) and Linux with glibc 2.35 or newer (x86_64, aarch64).
# The script downloads anycon-<target>.tar.gz and its .sha256 from the release, verifies the
# checksum, installs the binary, and puts LICENSE and THIRD-PARTY-LICENSES.txt under
# <install dir>/../share/doc/anycon. It never uses sudo.
set -eu

REPO="anycon-ai/anycon"
MIN_GLIBC="2.35"
# The copy attached to a GitHub Release has its version filled in by the release workflow;
# this copy from the repository installs the latest release.
DEFAULT_VERSION=""

say() { printf 'anycon-install: %s\n' "$*"; }
err() { printf 'anycon-install: error: %s\n' "$*" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || err "this script needs '$1'"; }

download() {
  if command -v curl >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget --https-only -q -O "$2" "$1"
  else
    err "this script needs curl or wget"
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d ' ' -f 1
  else
    err "this script needs sha256sum or shasum"
  fi
}

# Exit status 0 when version $1 >= version $2 (dotted numbers only).
version_ge() {
  [ "$(printf '%s\n%s\n' "$2" "$1" | sort -t . -k 1,1n -k 2,2n -k 3,3n | head -n 1)" = "$2" ]
}

detect_target() {
  os=$(uname -s)
  arch=$(uname -m)
  case "$os" in
    Darwin)
      # A shell running under Rosetta reports x86_64 on Apple silicon; install the native build.
      if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
        arch=arm64
      fi
      case "$arch" in
        arm64 | aarch64) echo aarch64-apple-darwin ;;
        x86_64) echo x86_64-apple-darwin ;;
        *) err "unsupported macOS architecture: $arch" ;;
      esac
      ;;
    Linux)
      if ldd --version 2>&1 | grep -qi musl; then
        err "musl-based Linux (e.g. Alpine) is not supported by the prebuilt binaries; build from source: cargo install --git https://github.com/$REPO anycon-cli --locked"
      fi
      glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}')
      if [ -n "$glibc" ] && ! version_ge "$glibc" "$MIN_GLIBC"; then
        err "glibc $glibc is too old (the prebuilt binaries need $MIN_GLIBC or newer); build from source: cargo install --git https://github.com/$REPO anycon-cli --locked"
      fi
      case "$arch" in
        x86_64 | amd64) echo x86_64-unknown-linux-gnu ;;
        aarch64 | arm64) echo aarch64-unknown-linux-gnu ;;
        *) err "unsupported Linux architecture: $arch" ;;
      esac
      ;;
    *) err "unsupported OS: $os (prebuilt binaries cover macOS and Linux)" ;;
  esac
}

main() {
  need uname
  need tar
  need mktemp

  target=$(detect_target)
  version=${ANYCON_VERSION:-$DEFAULT_VERSION}
  version=${version#v}
  if [ -n "$version" ]; then
    base="https://github.com/$REPO/releases/download/v$version"
  else
    base="https://github.com/$REPO/releases/latest/download"
  fi
  install_dir=${ANYCON_INSTALL_DIR:-"$HOME/.local/bin"}
  archive="anycon-$target.tar.gz"

  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT INT TERM

  say "downloading $archive (${version:-latest})"
  download "$base/$archive" "$tmp/$archive"
  download "$base/$archive.sha256" "$tmp/$archive.sha256"

  expected=$(cut -d ' ' -f 1 < "$tmp/$archive.sha256")
  actual=$(sha256_of "$tmp/$archive")
  [ -n "$expected" ] && [ "$expected" = "$actual" ] || err "checksum mismatch for $archive (expected $expected, got $actual)"

  tar -xzf "$tmp/$archive" -C "$tmp"
  src="$tmp/anycon-$target"
  [ -f "$src/anycon" ] || err "$archive does not contain anycon-$target/anycon"

  mkdir -p "$install_dir"
  # Write next to the target and rename, so a running daemon keeps its old inode.
  cp "$src/anycon" "$install_dir/.anycon.new"
  chmod 755 "$install_dir/.anycon.new"
  mv -f "$install_dir/.anycon.new" "$install_dir/anycon"

  doc_dir="$(dirname "$install_dir")/share/doc/anycon"
  if mkdir -p "$doc_dir" 2>/dev/null; then
    for f in LICENSE THIRD-PARTY-LICENSES.txt README.md; do
      if [ -f "$src/$f" ]; then cp "$src/$f" "$doc_dir/$f"; fi
    done
  fi

  say "installed $("$install_dir/anycon" --version 2>/dev/null || echo anycon) to $install_dir/anycon"
  case ":$PATH:" in
    *":$install_dir:"*) ;;
    *) say "$install_dir is not on your PATH; add it, e.g.: export PATH=\"$install_dir:\$PATH\"" ;;
  esac
  say "next: anycon init --hub https://<your hub> --workspace ~/anycon"
}

main "$@"
