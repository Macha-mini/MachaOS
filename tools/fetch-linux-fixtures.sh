#!/bin/sh
# Fetches the dynamically-linked Linux test fixtures used by the Phase 6
# selftest (real glibc hello/true/cat on MachaOS). These are Debian 12
# (bookworm) amd64 binaries — glibc 2.36, coreutils 9.1, hello 2.10 —
# which is the exact toolchain the kernel's dynamic-loader work is
# validated against.
#
# Skips anything already present in target/, so re-runs are no-ops and
# offline builds that already have the fixtures keep working. Requires
# curl (macOS ships it) plus `ar` and `tar` with xz support (macOS's
# bsdtar handles .tar.xz natively; on Linux the GNU binutils ar + GNU
# tar are standard).
set -e
cd "$(dirname "$0")/.."

BASE="http://deb.debian.org/debian/pool/main"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# fetch_deb <pool-path> <member> <dest>  -- downloads the .deb, extracts
# <member> from its data.tar.xz, copies it to target/<dest> if missing.
fetch_deb() {
    local pool="$1" member="$2" dest="$3" deb url
    [ -f "target/$dest" ] && { echo "fixtures: target/$dest already present, skipping"; return 0; }
    deb="$(basename "$pool")"
    url="$BASE/$pool"
    echo "fixtures: fetching $deb ..."
    curl -sfL -o "$WORK/$deb" "$url"
    (cd "$WORK" && ar x "$deb" data.tar.xz)
    tar -xf "$WORK/data.tar.xz" -C "$WORK" "$member"
    cp "$WORK/$member" "target/$dest"
    echo "fixtures: -> target/$dest"
}

mkdir -p target

# ld.so + libc.so.6 from libc6 (glibc 2.36-9+deb12u14)
fetch_deb "g/glibc/libc6_2.36-9+deb12u14_amd64.deb" \
    "lib/x86_64-linux-gnu/ld-linux-x86-64.so.2" "ld-linux-x86-64.so.2"
fetch_deb "g/glibc/libc6_2.36-9+deb12u14_amd64.deb" \
    "lib/x86_64-linux-gnu/libc.so.6" "libc.so.6"

# true + cat from coreutils 9.1-1 (Debian ships them under /bin, which
# is a symlink to /usr/bin on bookworm — the package stores bin/true)
fetch_deb "c/coreutils/coreutils_9.1-1_amd64.deb" \
    "bin/true" "coreutils-true"
fetch_deb "c/coreutils/coreutils_9.1-1_amd64.deb" \
    "bin/cat" "coreutils-cat"

# GNU hello 2.10-2
fetch_deb "h/hello/hello_2.10-2_amd64.deb" \
    "usr/bin/hello" "hello"

echo "fixtures: all present in target/"
