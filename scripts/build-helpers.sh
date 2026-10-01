#!/bin/sh
# Build archive-helper for the servers it runs on, into dist/helpers/:
# FreeBSD x86_64 (TrueNAS CORE) and static Linux x86_64.
#
# FreeBSD (TrueNAS CORE archive servers) is cross-compiled against the
# FreeBSD 13.1 base system (runs on 13.x and later), using Apple/LLVM clang to
# compile C code and Rust's bundled lld to link. First run downloads and
# verifies the FreeBSD base (~190 MB) into .toolchain/.
set -eu
here="$(cd "$(dirname "$0")/.." && pwd)"
cd "$here"
out="$here/dist/helpers"
mkdir -p "$out" .toolchain

# --- FreeBSD 13.1 sysroot -------------------------------------------------
sysroot="$here/.toolchain/freebsd-13.1"
if [ ! -f "$sysroot/usr/lib/crt1.o" ]; then
  url=http://ftp-archive.freebsd.org/pub/FreeBSD-Archive/old-releases/amd64/13.1-RELEASE
  sha=565baf7cf520cedfa01c5260f6a614b71c5e2b37ba3ee22e1342906548aa24ad
  [ -f .toolchain/base-13.1.txz ] || curl -sf -o .toolchain/base-13.1.txz "$url/base.txz"
  echo "$sha  .toolchain/base-13.1.txz" | shasum -a 256 -c -
  mkdir -p "$sysroot"
  tar -xf .toolchain/base-13.1.txz -C "$sysroot" ./usr/include ./usr/lib ./lib
fi

# --- musl headers for static Linux builds (Alpine's musl-dev) ---------------
musl="$here/.toolchain/musl-1.2.5"
if [ ! -f "$musl/usr/include/stdio.h" ]; then
  apk=.toolchain/musl-dev-1.2.5-r3.apk
  [ -f "$apk" ] || curl -sf -o "$apk" https://dl-cdn.alpinelinux.org/alpine/v3.20/main/x86_64/musl-dev-1.2.5-r3.apk
  echo "36abcf8a199826080b9b2a45f86782afae4ae5c8b8331909e5113911e2bdcad1  $apk" | shasum -a 256 -c -
  mkdir -p "$musl"
  tar -xzf "$apk" -C "$musl" usr/include 2>/dev/null || true
fi

rustup target add x86_64-unknown-freebsd x86_64-unknown-linux-musl >/dev/null
rustup component add llvm-tools >/dev/null
llvm_ar="$(find "$(rustc --print sysroot)" -name llvm-ar -type f | head -1)"

export FREEBSD_SYSROOT="$sysroot"
CC_x86_64_unknown_freebsd="$here/scripts/freebsd-cc" \
AR_x86_64_unknown_freebsd="$llvm_ar" \
CARGO_TARGET_X86_64_UNKNOWN_FREEBSD_LINKER="$here/scripts/freebsd-cc" \
  cargo build --release --target x86_64-unknown-freebsd -p archive-helper
cp target/x86_64-unknown-freebsd/release/archive-helper "$out/archive-helper-freebsd-x86_64"

# Linux: fully static (musl), linked by rustc with its own musl libc and lld.
lld="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin/rust-lld"
CC_x86_64_unknown_linux_musl="$here/scripts/musl-cc" \
AR_x86_64_unknown_linux_musl="$llvm_ar" \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$lld" \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C linker-flavor=ld.lld -C link-self-contained=yes" \
  cargo build --release --target x86_64-unknown-linux-musl -p archive-helper
cp target/x86_64-unknown-linux-musl/release/archive-helper "$out/archive-helper-linux-x86_64"

( cd "$out" && shasum -a 256 archive-helper-* > SHA256SUMS && cat SHA256SUMS )
