#!/usr/bin/env bash
# audit-launcher-elf.sh <binary> <aarch64|x86_64>: the packaged launcher binary
# is a native PIE for the architecture, with the expected program interpreter,
# no RPATH/RUNPATH, exactly the host libraries the packages declare, and no
# GLIBC symbol version above the architecture's ceiling. Prints the highest
# GLIBC version it requires. Shared by every package build's audit.
set -euo pipefail

if [[ $# -ne 2 || ! -f "$1" ]]; then
  echo "usage: $0 <binary> <aarch64|x86_64>" >&2
  exit 2
fi
binary=$1
architecture=$2
case "$architecture" in
  aarch64)
    architecture_label=AArch64
    file_machine='ARM aarch64'
    elf_machine='AArch64'
    program_interpreter=/lib/ld-linux-aarch64.so.1
    # A Fedora 44 AArch64 build's libm float functions carry GLIBC_2.43
    # versions.
    glibc_ceiling=2.43
    ;;
  x86_64)
    architecture_label=x86-64
    file_machine='x86-64'
    elf_machine='Advanced Micro Devices X86-64'
    program_interpreter=/lib64/ld-linux-x86-64.so.2
    # The ubuntu:22.04 container build's glibc; the supported families are
    # Debian-based (Debian 12+, Ubuntu 22.04+) and Arch-based.
    glibc_ceiling=2.35
    ;;
  *)
    echo "unsupported package architecture: $architecture" >&2
    exit 2
    ;;
esac

if ! file "$binary" | grep -Fq "ELF 64-bit LSB pie executable, $file_machine,"; then
  echo "launcher binary is not a native $architecture_label PIE" >&2
  exit 1
fi

if ! readelf -h "$binary" | grep -Eq "Machine:[[:space:]]+$elf_machine\$"; then
  echo "launcher ELF machine is not $architecture_label" >&2
  exit 1
fi

if [[ $(readelf -l "$binary" | sed -n 's/.*Requesting program interpreter: \([^]]*\)\]$/\1/p') != "$program_interpreter" ]]; then
  echo "launcher uses an unexpected program interpreter" >&2
  exit 1
fi

if readelf -d "$binary" | grep -Eq '\((RPATH|RUNPATH)\)'; then
  echo "launcher must not contain RPATH or RUNPATH" >&2
  exit 1
fi

actual_needed=$(readelf -d "$binary" | sed -n 's/.*Shared library: \[\([^]]*\)\].*/\1/p' | LC_ALL=C sort)
expected_needed=$(cat <<LIBRARIES | LC_ALL=C sort
${program_interpreter##*/}
libc.so.6
libgcc_s.so.1
libm.so.6
libxcb.so.1
libxkbcommon-x11.so.0
libxkbcommon.so.0
LIBRARIES
)

if [[ "$actual_needed" != "$expected_needed" ]]; then
  echo "launcher has an unexpected direct shared-library dependency" >&2
  diff -u <(printf '%s\n' "$expected_needed") <(printf '%s\n' "$actual_needed") >&2 || true
  exit 1
fi

# Every GLIBC symbol version the launcher requires must be at or below the
# architecture's floor, or the package silently drops older distributions.
required_glibc=$(readelf -V --wide "$binary" |
  sed -n 's/.*Name: GLIBC_\([0-9][0-9.]*\)[[:space:]].*/\1/p' | sort -u -V | tail -n 1)
if [[ -z "$required_glibc" ]]; then
  echo "launcher declares no GLIBC symbol versions" >&2
  exit 1
fi
if [[ $(printf '%s\n' "$glibc_ceiling" "$required_glibc" | sort -V | tail -n 1) != "$glibc_ceiling" ]]; then
  echo "launcher requires GLIBC_$required_glibc, above the $architecture_label ceiling GLIBC_$glibc_ceiling" >&2
  echo "build packages with scripts/build-appimage-container.sh" >&2
  exit 1
fi

echo "$required_glibc"
