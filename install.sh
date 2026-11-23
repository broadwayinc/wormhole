#!/bin/sh
# wormhole installer. Served at https://cdn.broadwayinc.com/get/wormhole/v1 and run with:
#   curl -fsSL https://cdn.broadwayinc.com/get/wormhole/v1 | sh
#
# It downloads the wormhole build for this machine, checks its SHA-256, and installs the
# `wormhole` program into /usr/local/bin, so every user on the machine can run it. As root it
# writes there directly; as a normal user it uses sudo (on EC2 Ubuntu sudo asks nothing); with no
# sudo at all it falls back to ~/.local/bin. Running it again updates to the newest version.
# The README and license files go to <prefix>/share/doc/wormhole.
#
# Optional settings, given as environment variables to `sh`:
#   WORMHOLE_VERSION=0.1.0          install this version instead of the latest one
#   WORMHOLE_INSTALL_DIR=/some/bin  install the program into this folder
#   WORMHOLE_BASE_URL=https://...   download from another place (testing a release)
#
# Everything runs inside main(), so nothing happens if the download of this script stops halfway.

set -eu

say() { printf '%s\n' "$*"; }
fail() { printf 'wormhole install failed: %s\n' "$*" >&2; exit 1; }

main() {
    base="${WORMHOLE_BASE_URL:-https://cdn.broadwayinc.com/get/wormhole}"

    # --- this machine ---
    os="$(uname -s)"
    [ "$os" = "Linux" ] || fail "wormhole runs on Linux only; this machine runs $os."
    case "$(uname -m)" in
        x86_64 | amd64) arch="x86_64" ;;
        aarch64 | arm64) arch="aarch64" ;;
        *) fail "there is no wormhole build for the processor type $(uname -m)." ;;
    esac
    glibc="$(ldd --version 2>/dev/null | head -n 1 | awk '{print $NF}')"
    case "$glibc" in
        [0-9]*.[0-9]*) ;;
        *) fail "wormhole needs a Linux with glibc 2.17 or newer (Ubuntu, Debian, RHEL/CentOS 7+, Amazon Linux). This system's C library could not be read as glibc (Alpine and other musl systems are not supported)." ;;
    esac
    major="${glibc%%.*}"
    minor="${glibc#*.}"
    minor="${minor%%[!0-9]*}"
    if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 17 ]; }; then
        fail "wormhole needs glibc 2.17 or newer; this system has glibc $glibc."
    fi

    # --- download tools ---
    if command -v curl >/dev/null 2>&1; then
        fetch() { curl -fsSL "$1" -o "$2"; }
    elif command -v wget >/dev/null 2>&1; then
        fetch() { wget -q "$1" -O "$2"; }
    else
        fail "curl or wget is needed to download wormhole."
    fi
    if command -v sha256sum >/dev/null 2>&1; then
        sha256() { sha256sum "$1" | awk '{print $1}'; }
    elif command -v shasum >/dev/null 2>&1; then
        sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
    else
        fail "sha256sum (or shasum) is needed to check the download."
    fi

    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT

    # --- which version ---
    version="${WORMHOLE_VERSION:-}"
    if [ -z "$version" ]; then
        fetch "$base/latest" "$tmp/latest" || fail "could not read the latest version from $base/latest."
        version="$(tr -d ' \r\n' < "$tmp/latest")"
    fi
    case "$version" in
        [0-9]*.[0-9]*.[0-9]*) ;;
        *) fail "'$version' is not a wormhole version." ;;
    esac

    # --- download and check ---
    name="wormhole-$version-linux-$arch"
    say "Downloading wormhole $version for Linux $arch..."
    fetch "$base/$version/$name.tar.gz" "$tmp/$name.tar.gz" || fail "could not download $base/$version/$name.tar.gz."
    fetch "$base/$version/$name.tar.gz.sha256" "$tmp/$name.tar.gz.sha256" || fail "could not download the checksum $base/$version/$name.tar.gz.sha256."
    want="$(awk '{print $1}' < "$tmp/$name.tar.gz.sha256")"
    got="$(sha256 "$tmp/$name.tar.gz")"
    [ "$want" = "$got" ] || fail "the download is damaged: its SHA-256 is $got, expected $want. Run the command again."
    tar -xzf "$tmp/$name.tar.gz" -C "$tmp" || fail "could not unpack $name.tar.gz."
    [ -f "$tmp/$name/wormhole" ] || fail "the download does not contain the wormhole program."

    # --- where to install ---
    sudo=""
    if [ -n "${WORMHOLE_INSTALL_DIR:-}" ]; then
        bin="$WORMHOLE_INSTALL_DIR"
        docs="$(dirname "$bin")/share/doc/wormhole"
    else
        bin="/usr/local/bin"
        docs="/usr/local/share/doc/wormhole"
        if [ "$(id -u)" -ne 0 ] && ! { [ -d "$bin" ] && [ -w "$bin" ]; }; then
            if command -v sudo >/dev/null 2>&1; then
                sudo="sudo"
            else
                bin="$HOME/.local/bin"
                docs="$HOME/.local/share/doc/wormhole"
            fi
        fi
    fi
    if [ -n "$sudo" ]; then
        # passwordless sudo (EC2) goes straight through; otherwise read the password from the
        # terminal, even though this script itself arrives through a pipe
        if ! sudo -n true 2>/dev/null; then
            say "Installing into $bin needs administrator rights; sudo may ask for your password."
            if [ -r /dev/tty ]; then sudo -v < /dev/tty || fail "sudo did not accept the password."; else sudo -v || fail "sudo is needed to write to $bin."; fi
        fi
    fi

    $sudo mkdir -p "$bin" "$docs"
    # install to a temp name and rename, so a running wormhole keeps working and never sees half a file
    $sudo install -m 0755 "$tmp/$name/wormhole" "$bin/.wormhole.new"
    $sudo mv -f "$bin/.wormhole.new" "$bin/wormhole"
    for f in README.md LICENSE NOTICE THIRD-PARTY-NOTICES; do
        [ -f "$tmp/$name/$f" ] && $sudo install -m 0644 "$tmp/$name/$f" "$docs/$f"
    done

    installed="$("$bin/wormhole" version 2>/dev/null)" || fail "$bin/wormhole was installed but does not run on this system."
    say ""
    say "Installed $installed at $bin/wormhole."
    say "Guide and license files: $docs"
    case ":$PATH:" in
        *":$bin:"*) ;;
        *) say ""
           say "$bin is not on your PATH yet. Add this line to ~/.profile, then open a new terminal:"
           say "  export PATH=\"$bin:\$PATH\"" ;;
    esac
    say ""
    say "Next: make a shared key on one server and copy it to the other (see $docs/README.md):"
    say "  wormhole keygen ~/wormhole.key"
    say "  wormhole --help"
    say "Running this command again later updates wormhole to the newest version."
    say "A running wormhole keeps the old version until it restarts (pm2 restart <name>)."
}

main "$@"
