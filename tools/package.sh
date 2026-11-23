#!/bin/sh
# Builds a wormhole release and lays it out in dist/ exactly as it goes on the CDN:
#
#   dist/get/wormhole/v1                                                  the installer (install.sh)
#   dist/get/wormhole/latest                                              the newest version, e.g. 0.1.0
#   dist/get/wormhole/<version>/wormhole-<version>-linux-<arch>.tar.gz          arch: x86_64 or aarch64
#   dist/get/wormhole/<version>/wormhole-<version>-linux-<arch>.tar.gz.sha256
#
# Upload dist/get to the CDN root, keeping the paths (tools/upload.sh). Run from anywhere:
#   sh tools/package.sh
# Builds with cargo-zigbuild (zig as the cross linker, from `pip install ziglang`) against glibc 2.17,
# whatever the glibc of the machine that builds: install.sh promises glibc 2.17 or newer. Every binary
# is checked before it is packed; one that needs a newer glibc stops the release.
# Same layout and tools as DDBD's tools/package.sh.

set -eu
cd "$(dirname "$0")/.."
export PATH="$HOME/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin:$HOME/.cargo/bin:$HOME/.local/bin:$PATH"

# Keep the build machine's folder names out of the binary (panic messages carry source paths).
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME=/build --remap-path-prefix=$(pwd)=/wormhole"

command -v cargo-zigbuild >/dev/null 2>&1 || { echo "cargo-zigbuild is needed (cargo install --locked cargo-zigbuild; pip install ziglang)." >&2; exit 1; }

version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)"
target="${CARGO_TARGET_DIR:-target}"
out="dist/get/wormhole"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
mkdir -p "$out/$version"

python3 tools/third_party.py

# The newest glibc a release may need (install.sh checks for it)
max_glibc="2.17"

# check_glibc <binary>: stop if it needs a newer glibc than max_glibc
check_glibc() {
    need="$(objdump -T "$1" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -t. -k1,1n -k2,2n -u | tail -n 1)"
    newest="$(printf '%s\n%s\n' "$need" "$max_glibc" | sort -t. -k1,1n -k2,2n | tail -n 1)"
    if [ "$newest" != "$max_glibc" ]; then
        echo "$1 needs glibc $need, newer than the $max_glibc that install.sh promises. Not packed." >&2
        exit 1
    fi
    echo "  $1 needs glibc $need at most: fine"
}

# pack <arch> <built binary>
pack() {
    check_glibc "$2"
    name="wormhole-$version-linux-$1"
    mkdir -p "$stage/$name"
    cp "$2" "$stage/$name/wormhole"
    cp README.md LICENSE NOTICE THIRD-PARTY-NOTICES "$stage/$name/"
    # reproducible: fixed order, owner and dates, and no name or time in the gzip header, so the same
    # inputs always give the same file and checksum
    tar -cf - -C "$stage" --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 "$name" | gzip -9 -n > "$out/$version/$name.tar.gz"
    (cd "$out/$version" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
}

for arch in x86_64 aarch64; do
    echo "Building wormhole $version for linux-$arch..."
    # stripped by rustc while linking (Cargo.toml strip = true): the host strip cannot read arm64 files
    cargo zigbuild --release --target "$arch-unknown-linux-gnu.$max_glibc"
    pack "$arch" "$target/$arch-unknown-linux-gnu/release/wormhole"
done

printf '%s\n' "$version" > "$out/latest"
cp install.sh "$out/v1"

echo ""
echo "Release files in dist/:"
find dist -type f | sort | while read -r f; do printf '  %-64s %s\n' "$f" "$(du -h "$f" | cut -f1)"; done
