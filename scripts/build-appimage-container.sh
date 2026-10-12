#!/usr/bin/env bash
# Build the AppImage, the Debian package, or the standalone executable inside
# a pinned ubuntu:22.04 container so the launcher's glibc floor is GLIBC_2.35
# instead of the build host's glibc.
# The container clones the committed HEAD into a fixed path, installs build
# packages from a dated Ubuntu snapshot and the pinned rustup-init, then runs
# the format's build script (scripts/build-binary.sh for the executable) and
# its audits unchanged.
#
# usage: build-appimage-container.sh [OUTPUT_DIRECTORY] [x86_64|aarch64] [binary|appimage|deb]
# The format defaults to the standalone executable; the AppImage and the
# Debian package wrap the same binary. The architecture defaults to the
# host's. The container always runs the host's architecture; a foreign target
# is cross-compiled against that architecture's libraries from the same
# snapshot. Only the audits' short runs of the built launcher use a qemu-user
# binfmt_misc handler with the F flag.
#
# Builds reuse their work by default: a local image holding the packages, a
# persistent checkout, and the `device` Cargo profile.
# PD2_RELEASE_BUILD=1 builds a release instead: a fresh container and the
# release profile.
set -euo pipefail

if [[ -n "${PD2_RELEASE_BUILD:-}" ]]; then
  unset PD2_FAST_BUILD
else
  PD2_FAST_BUILD=1
fi

repository_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
output_directory=${1:-"$repository_root/dist"}
architecture=${2:-$(uname -m)}
host_architecture=$(uname -m)
package_format=${3:-binary}
case "$package_format" in
  appimage) build_script=scripts/build-appimage.sh ;;
  deb) build_script=scripts/build-deb.sh ;;
  binary) build_script=scripts/build-binary.sh ;;
  *)
    echo "the package format must be appimage, deb, or binary, not $package_format" >&2
    exit 1
    ;;
esac
apt_snapshot=20260920T000000Z
rustup_version=1.29.1
case "$architecture" in
  x86_64) target_debian_architecture=amd64 ;;
  aarch64) target_debian_architecture=arm64 ;;
  *)
    echo "the ubuntu:22.04 container build supports x86_64 or aarch64, not $architecture" >&2
    exit 1
    ;;
esac
# Platform manifests of ubuntu:22.04 (2026-09-03 image) and the pinned
# rustup-init for the build host's architecture.
case "$host_architecture" in
  x86_64)
    container_architecture=amd64
    base_image=docker.io/library/ubuntu@sha256:281c5745f657873d78e5531fc5ba8575f46ab7769b94550ac99543f122679986
    rustup_init_sha256=dda7234360b7f578ca8b0ddcb80145646fa61a67c1720a5abc7051b35c9fcb71
    cache_suffix=
    ;;
  aarch64)
    container_architecture=arm64
    base_image=docker.io/library/ubuntu@sha256:1cc7bb38a74c0e126716646e47c0b3c5c139547d386d5ff7a64cf3ea316ca523
    rustup_init_sha256=15f6e4ce9f583b929c996c91562bad6d4454f3281de858b02cdfdef615fac433
    cache_suffix=-aarch64
    ;;
  *)
    echo "the ubuntu:22.04 container build runs on x86_64 or aarch64 hosts, not $host_architecture" >&2
    exit 1
    ;;
esac
container_cache=${PD2_CONTAINER_CACHE:-${XDG_CACHE_HOME:-${HOME:?HOME is required}/.cache}/pd2launcher-container-ubuntu-22.04$cache_suffix}

# The audits run the cross-compiled launcher under the host's qemu-user
# binfmt_misc handler. AppImages carry their own ELF magic that the handler
# does not match, so the AppImage audit runs the built AppImage through the
# host's static qemu-user binary, mounted into the container.
emulator_arguments=()
if [[ "$host_architecture" != "$architecture" ]]; then
  binfmt_handler=/proc/sys/fs/binfmt_misc/qemu-$architecture
  # Debian and Ubuntu register a symlink to qemu-*-static as the interpreter.
  emulator=$(sed -n 's/^interpreter //p' "$binfmt_handler" 2>/dev/null || true)
  emulator=$(readlink -f -- "$emulator" 2>/dev/null || true)
  command -v file >/dev/null || {
    echo "the file command is required to check the qemu-$architecture interpreter" >&2
    exit 1
  }
  if [[ ! -r "$binfmt_handler" ]] || ! grep -qx enabled "$binfmt_handler" ||
    ! grep -q '^flags: .*F' "$binfmt_handler" || [[ ! -x "$emulator" ]] ||
    ! file -L "$emulator" | grep -Eq 'statically linked|static-pie linked'; then
    echo "auditing a $architecture build on $host_architecture needs a static qemu-$architecture binfmt_misc handler with the F flag (qemu-user-static)" >&2
    exit 1
  fi
  emulator_arguments=(
    --volume "$emulator:/run/qemu-user:ro"
    --env PD2_APPIMAGE_EMULATOR=/run/qemu-user
  )
fi

engine=${CONTAINER_ENGINE:-}
if [[ -z "$engine" ]]; then
  for candidate in podman docker; do
    if command -v "$candidate" >/dev/null; then
      engine=$candidate
      break
    fi
  done
fi
[[ -n "$engine" ]] || {
  echo "podman or docker is required (or set CONTAINER_ENGINE)" >&2
  exit 1
}

if [[ -n $(git -C "$repository_root" status --porcelain --untracked-files=normal) ]]; then
  echo "refusing to package a dirty worktree; the container builds the committed HEAD" >&2
  exit 1
fi
commit=$(git -C "$repository_root" rev-parse HEAD)
git_common_directory=$(cd "$repository_root" && cd "$(git rev-parse --git-common-dir)" && pwd -P)

host_ca_bundle=
for candidate in /etc/ssl/certs/ca-certificates.crt /etc/pki/tls/certs/ca-bundle.crt /etc/ssl/cert.pem; do
  if [[ -f "$candidate" ]]; then
    host_ca_bundle=$candidate
    break
  fi
done
[[ -n "$host_ca_bundle" ]] || {
  echo "no host CA bundle found for the initial snapshot download" >&2
  exit 1
}

mkdir -p "$output_directory" "$container_cache/cargo" "$container_cache/rustup" "$container_cache/xdg"
output_directory=$(cd "$output_directory" && pwd -P)
# Only this copy of the host trust roots is mounted; it bootstraps HTTPS to the
# snapshot archive, which then installs the image's own ca-certificates.
install -m 0644 "$host_ca_bundle" "$container_cache/host-ca.crt"

# shellcheck disable=SC2016 # expanded inside the container
setup_script='
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
cat > /etc/apt/sources.list <<SOURCES
deb https://snapshot.ubuntu.com/ubuntu/$APT_SNAPSHOT jammy main universe
deb https://snapshot.ubuntu.com/ubuntu/$APT_SNAPSHOT jammy-updates main universe
deb https://snapshot.ubuntu.com/ubuntu/$APT_SNAPSHOT jammy-security main universe
SOURCES
apt_options=(-o Acquire::https::CaInfo=/run/host-ca.crt -o Acquire::Check-Valid-Until=false)
target_libraries=(libasound2-dev libfontconfig-dev libvulkan1 libwayland-dev libx11-xcb-dev
  libxkbcommon-x11-dev libzstd-dev)
cross_packages=()
if [[ -n "$CROSS_GNU_TRIPLE" ]]; then
  # The target libraries come from the same snapshot, for the target architecture.
  dpkg --add-architecture "$TARGET_DEBIAN_ARCHITECTURE"
  target_libraries=("${target_libraries[@]/%/:$TARGET_DEBIAN_ARCHITECTURE}")
  # Debian names the cross compiler packages with hyphens (gcc-x86-64-linux-gnu)
  # but installs the tools under the GNU triple (x86_64-linux-gnu-gcc).
  cross_package_suffix=${CROSS_GNU_TRIPLE//_/-}
  cross_packages=("gcc-$cross_package_suffix" "g++-$cross_package_suffix")
fi
apt-get "${apt_options[@]}" update -qq
apt-get "${apt_options[@]}" install -y -qq --no-install-recommends \
  binutils build-essential ca-certificates clang curl desktop-file-utils file git \
  libxml2-utils pkg-config python3 util-linux "${cross_packages[@]}" "${target_libraries[@]}" >/dev/null
'

# shellcheck disable=SC2016 # expanded inside the container
build_part='
set -euo pipefail
export RUSTUP_HOME=/rustup CARGO_HOME=/cargo PATH=/cargo/bin:$PATH
if [[ ! -x /cargo/bin/rustup ]]; then
  curl --fail --silent --show-error --proto =https --tlsv1.2 --output /tmp/rustup-init \
    "https://static.rust-lang.org/rustup/archive/$RUSTUP_VERSION/$(uname -m)-unknown-linux-gnu/rustup-init"
  echo "$RUSTUP_INIT_SHA256  /tmp/rustup-init" | sha256sum --check --status
  chmod 0755 /tmp/rustup-init
  /tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain none >/dev/null
fi

git config --global --add safe.directory "*"
if [[ -n "${PD2_FAST_BUILD:-}" ]]; then
  # Fast device build: one builder at a time, a checkout that survives between
  # runs so Git rewrites only the files that changed (Cargo compares file
  # times, and a fresh clone makes every source file new), and the faster
  # device profile.
  exec 9>/build/.fast-build.lock
  flock 9
  if [[ -d /build/pd2launcher/.git ]]; then
    git -C /build/pd2launcher fetch --quiet --no-tags \
      --upload-pack="git -c uploadpack.allowAnySHA1InWant=true upload-pack" /source.git "$SOURCE_COMMIT"
    git -C /build/pd2launcher checkout --quiet --detach --force "$SOURCE_COMMIT"
    git -C /build/pd2launcher clean --quiet -fd
  else
    git clone --quiet --no-checkout /source.git /build/pd2launcher
    git -C /build/pd2launcher checkout --quiet --detach "$SOURCE_COMMIT"
  fi
  export PD2_CARGO_PROFILE=device
  # The pinned toolchain and target are already in the cache after the first run.
  export RUSTUP_SELF_UPDATE=disable
else
  git clone --quiet --no-checkout /source.git /build/pd2launcher
  git -C /build/pd2launcher checkout --quiet --detach "$SOURCE_COMMIT"
fi
cd /build/pd2launcher
if [[ -z "${PD2_FAST_BUILD:-}" ]] || ! rustup show active-toolchain >/dev/null 2>&1; then
  rustup toolchain install >/dev/null
fi
if [[ -n "${PD2_FAST_BUILD:-}" ]]; then
  # Without LTO the default linker keeps libraries that nothing uses; lld, which
  # the Rust toolchain ships, drops them and links much faster.
  lld_directory=$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n "s/^host: //p")/bin/gcc-ld
  [[ -x $lld_directory/ld.lld ]] || { echo "the Rust toolchain has no ld.lld for the fast build" >&2; exit 1; }
  export RUSTFLAGS="-Clink-arg=-fuse-ld=lld -Clink-arg=-B$lld_directory"
fi
if [[ -n "$CROSS_GNU_TRIPLE" ]]; then
  rust_target=$PD2_TARGET_ARCHITECTURE-unknown-linux-gnu
  if [[ -z "${PD2_FAST_BUILD:-}" ]] || ! rustup target list --installed | grep -qx "$rust_target"; then
    rustup target add "$rust_target" >/dev/null
  fi
  target_variable=${rust_target//-/_}
  linker_variable=CARGO_TARGET_${target_variable^^}_LINKER
  export "$linker_variable=$CROSS_GNU_TRIPLE-gcc"
  export "CC_$target_variable=$CROSS_GNU_TRIPLE-gcc" "CXX_$target_variable=$CROSS_GNU_TRIPLE-g++"
  export "AR_$target_variable=$CROSS_GNU_TRIPLE-ar"
  export "PKG_CONFIG_ALLOW_CROSS_$target_variable=1"
  export "PKG_CONFIG_LIBDIR_$target_variable=/usr/lib/$CROSS_GNU_TRIPLE/pkgconfig:/usr/share/pkgconfig"
  export "BINDGEN_EXTRA_CLANG_ARGS_$target_variable=--target=$CROSS_GNU_TRIPLE"
fi
echo "container glibc: $(ldd --version | head -n 1)"
XDG_CACHE_HOME=/xdg APPIMAGE_EXTRACT_AND_RUN=1 "$BUILD_SCRIPT" /out
'

# Label separation is disabled instead of relabeling the checkout's Git
# directory; every mount except the caches and output is read-only.
# Podman selects the image platform with --arch; Docker has only --platform.
if [[ "$host_architecture" == "$architecture" ]]; then
  cross_gnu_triple=
else
  cross_gnu_triple=$architecture-linux-gnu
fi
if [[ $(basename "$engine") == docker ]]; then
  platform_arguments=(--platform "linux/$container_architecture")
else
  platform_arguments=(--arch "$container_architecture")
fi
run_arguments=(
  run --rm "${platform_arguments[@]}" --security-opt label=disable
  --env APT_SNAPSHOT="$apt_snapshot"
  --env RUSTUP_VERSION="$rustup_version"
  --env RUSTUP_INIT_SHA256="$rustup_init_sha256"
  --env SOURCE_COMMIT="$commit"
  --env BUILD_SCRIPT="$build_script"
  --env PD2_TARGET_ARCHITECTURE="$architecture"
  --env TARGET_DEBIAN_ARCHITECTURE="$target_debian_architecture"
  --env CROSS_GNU_TRIPLE="$cross_gnu_triple"
  --volume "$git_common_directory:/source.git:ro"
  --volume "$container_cache/host-ca.crt:/run/host-ca.crt:ro"
  --volume "$container_cache/cargo:/cargo"
  --volume "$container_cache/rustup:/rustup"
  --volume "$container_cache/xdg:/xdg"
  --volume "$output_directory:/out"
  "${emulator_arguments[@]}"
)
if [[ -n "${SOURCE_DATE_EPOCH:-}" ]]; then
  run_arguments+=(--env SOURCE_DATE_EPOCH="$SOURCE_DATE_EPOCH")
fi
# A dev build compiles in the CD keys of the build machine's .env.local
# (src/dev_build.rs), so a test install starts with them filled in. They are
# passed by name, so the values never appear in a command line, and a release
# build neither reads nor accepts them.
if [[ -n "${PD2_RELEASE_BUILD:-}" ]]; then
  if [[ -n "${PD2_DEV_D2_KEY:-}${PD2_DEV_D2_LOD_KEY:-}" ]]; then
    echo "a release build never includes dev CD keys" >&2
    exit 1
  fi
  run_arguments+=(--env PD2_RELEASE_BUILD=1)
else
  local_env="$repository_root/.env.local"
  local_key() {
    grep -m1 "^$1=" "$local_env" | cut -d= -f2- | tr -d "\"'\r "
  }
  if [[ -z "${PD2_DEV_D2_KEY:-}" && -f "$local_env" ]]; then
    PD2_DEV_D2_KEY=$(local_key D2_KEY) || PD2_DEV_D2_KEY=
    PD2_DEV_D2_LOD_KEY=$(local_key D2_LOD_KEY) || PD2_DEV_D2_LOD_KEY=
  fi
  if [[ -n "${PD2_DEV_D2_KEY:-}" && -n "${PD2_DEV_D2_LOD_KEY:-}" ]]; then
    export PD2_DEV_D2_KEY PD2_DEV_D2_LOD_KEY
    run_arguments+=(--env PD2_DEV_D2_KEY --env PD2_DEV_D2_LOD_KEY)
  fi
fi

# A release build runs the setup and the build in one fresh container.
container_script="$setup_script$build_part"
run_image=$base_image
if [[ -n "${PD2_FAST_BUILD:-}" ]]; then
  # The default, reusing build. The packages from the pinned snapshot are
  # installed once into a local image named after everything that determines
  # them; the build then runs in that image with a checkout and the faster
  # device Cargo profile that persist between runs.
  # The build scripts, their audits, and the artifact are otherwise the same.
  image_key=$(printf '%s\n' "$base_image" "$apt_snapshot" "$architecture" "$cross_gnu_triple" "$setup_script" |
    sha256sum | cut -c1-16)
  run_image=localhost/pd2-build-ubuntu-22.04:$image_key
  mkdir -p "$container_cache/build"
  (
    flock 8
    if ! "$engine" image inspect "$run_image" >/dev/null 2>&1; then
      echo "preparing the build image $run_image (once per snapshot, package, or architecture change)" >&2
      setup_container=pd2-build-setup-$image_key-$$
      "$engine" run --name "$setup_container" "${platform_arguments[@]}" --security-opt label=disable \
        --env APT_SNAPSHOT="$apt_snapshot" \
        --env TARGET_DEBIAN_ARCHITECTURE="$target_debian_architecture" \
        --env CROSS_GNU_TRIPLE="$cross_gnu_triple" \
        --volume "$container_cache/host-ca.crt:/run/host-ca.crt:ro" \
        "$base_image" bash -c "$setup_script"
      "$engine" commit --quiet "$setup_container" "$run_image" >/dev/null
      "$engine" rm --force "$setup_container" >/dev/null
    fi
  ) 8>"$container_cache/image.lock"
  container_script=$build_part
  run_arguments+=(--env PD2_FAST_BUILD=1 --volume "$container_cache/build:/build")
fi

"$engine" "${run_arguments[@]}" "$run_image" bash -c "$container_script"
