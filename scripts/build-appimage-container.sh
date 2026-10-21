#!/usr/bin/env bash
# Build the standalone executable inside a pinned ubuntu:22.04 container so
# the launcher's glibc floor is GLIBC_2.35 instead of the build host's glibc.
# The container runs in a build image that holds the build packages from a
# dated Ubuntu snapshot and the Rust toolchain of rust-toolchain.toml, which
# the pinned rustup-init installs. A build clones the committed HEAD into a
# fixed path and runs scripts/build-binary.sh and its audits unchanged; it
# installs no packages and no toolchain.
#
# usage: build-appimage-container.sh [OUTPUT_DIRECTORY] [x86_64|aarch64] [binary]
#        build-appimage-container.sh --image-key [x86_64|aarch64]
#        build-appimage-container.sh --image [x86_64|aarch64]
# The standalone executable (binary) is the only format. The architecture
# defaults to the host's. The container always runs the host's architecture;
# a foreign target is cross-compiled against that architecture's libraries
# from the same snapshot. Only the audit's short run of the built launcher
# uses a qemu-user binfmt_misc handler with the F flag.
#
# The build image is made once per image key, a hash of everything that goes
# into it (base image, snapshot, packages, rustup-init, toolchain file,
# architectures), as the local image localhost/pd2-build-ubuntu-22.04:<key>.
# A build uses that image when it exists (the release workflow loads it from
# the build-images release of the public repository) and makes it first
# otherwise, unless PD2_REQUIRE_BUILD_IMAGE is set (the release workflow sets
# it): then it stops. The image's label must name the same key. --image-key
# prints the key; --image makes the image if it does not exist and prints its
# name.
#
# Builds reuse their work by default: a persistent checkout and the `device`
# Cargo profile. PD2_RELEASE_BUILD=1 builds a release instead: a fresh
# checkout and the release profile.
set -euo pipefail

mode=build
case "${1:-}" in
  --image-key) mode=image-key && shift ;;
  --image) mode=image && shift ;;
esac

if [[ -n "${PD2_RELEASE_BUILD:-}" ]]; then
  unset PD2_FAST_BUILD
else
  PD2_FAST_BUILD=1
fi

repository_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
if [[ $mode == build ]]; then
  output_directory=${1:-"$repository_root/dist"}
  architecture=${2:-$(uname -m)}
  if [[ ${3:-binary} != binary ]]; then
    echo "the container build makes only the standalone executable (binary), not $3" >&2
    exit 1
  fi
else
  architecture=${1:-$(uname -m)}
fi
host_architecture=$(uname -m)
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
if [[ "$host_architecture" == "$architecture" ]]; then
  cross_gnu_triple=
else
  cross_gnu_triple=$architecture-linux-gnu
fi
# The toolchain the committed HEAD pins, which the image installs.
rust_toolchain_file=$(git -C "$repository_root" show HEAD:rust-toolchain.toml)

# shellcheck disable=SC2016 # expanded inside the container
setup_script='
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
cat > /etc/apt/sources.list <<SOURCES
deb https://snapshot.ubuntu.com/ubuntu/$APT_SNAPSHOT jammy main universe
deb https://snapshot.ubuntu.com/ubuntu/$APT_SNAPSHOT jammy-updates main universe
deb https://snapshot.ubuntu.com/ubuntu/$APT_SNAPSHOT jammy-security main universe
SOURCES
# The snapshot service can answer 502 or 503 for many minutes: apt tries each
# download again, and each download step runs up to ten times, with pauses
# of 1 to 5 minutes (up to 35 minutes for each step) for the service to
# recover. With its own delay between tries, apt wrote the same warning again
# and again while it waited (170,382 lines in one short outage), so the
# pauses come from the steps instead.
apt_options=(-o Acquire::https::CaInfo=/run/host-ca.crt -o Acquire::Check-Valid-Until=false
  -o Acquire::Retries=5 -o Acquire::Retries::Delay=false)
retry() {
  local attempt pause
  for attempt in 1 2 3 4 5 6 7 8 9; do
    "$@" && return
    pause=$((attempt < 5 ? attempt * 60 : 300))
    echo "$1 failed (attempt $attempt of 10); next attempt in $pause s" >&2
    sleep "$pause"
  done
  "$@"
}
target_libraries=(libfontconfig-dev libx11-xcb-dev libxkbcommon-x11-dev)
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
# Each try reads the package lists again: a list that failed to download
# leaves its packages unknown, so the install alone would fail on every try.
install_packages() {
  apt-get "${apt_options[@]}" update -qq --error-on=any &&
    apt-get "${apt_options[@]}" install -y -qq --no-install-recommends \
      binutils build-essential ca-certificates curl file git pkg-config util-linux \
      "${cross_packages[@]}" "${target_libraries[@]}" >/dev/null
}
retry install_packages
rm -rf /var/lib/apt/lists/*

export RUSTUP_HOME=/rustup CARGO_HOME=/cargo PATH=/cargo/bin:$PATH
retry curl --fail --silent --show-error --proto =https --tlsv1.2 --connect-timeout 30 --max-time 600 \
  --output /tmp/rustup-init \
  "https://static.rust-lang.org/rustup/archive/$RUSTUP_VERSION/$(uname -m)-unknown-linux-gnu/rustup-init"
echo "$RUSTUP_INIT_SHA256  /tmp/rustup-init" | sha256sum --check --status
chmod 0755 /tmp/rustup-init
/tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain none >/dev/null
rm /tmp/rustup-init
mkdir /tmp/toolchain
printf "%s\n" "$RUST_TOOLCHAIN_FILE" > /tmp/toolchain/rust-toolchain.toml
cd /tmp/toolchain
retry rustup toolchain install --no-self-update >/dev/null
if [[ -n "$CROSS_GNU_TRIPLE" ]]; then
  retry rustup target add "$PD2_TARGET_ARCHITECTURE-unknown-linux-gnu" >/dev/null
fi
cd /
rm -rf /tmp/toolchain /rustup/downloads /rustup/tmp
'

# shellcheck disable=SC2016 # expanded inside the container
build_part='
set -euo pipefail
export RUSTUP_HOME=/rustup CARGO_HOME=/cargo PATH=/cargo/bin:$PATH
# The image holds the toolchain; a build never installs another.
export RUSTUP_AUTO_INSTALL=0
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
else
  git clone --quiet --no-checkout /source.git /build/pd2launcher
  git -C /build/pd2launcher checkout --quiet --detach "$SOURCE_COMMIT"
fi
cd /build/pd2launcher
if [[ -n "${PD2_FAST_BUILD:-}" ]]; then
  # Without LTO the default linker keeps libraries that nothing uses; lld, which
  # the Rust toolchain ships, drops them and links much faster.
  lld_directory=$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n "s/^host: //p")/bin/gcc-ld
  [[ -x $lld_directory/ld.lld ]] || { echo "the Rust toolchain has no ld.lld for the fast build" >&2; exit 1; }
  export RUSTFLAGS="-Clink-arg=-fuse-ld=lld -Clink-arg=-B$lld_directory"
fi
if [[ -n "$CROSS_GNU_TRIPLE" ]]; then
  rust_target=$PD2_TARGET_ARCHITECTURE-unknown-linux-gnu
  target_variable=${rust_target//-/_}
  linker_variable=CARGO_TARGET_${target_variable^^}_LINKER
  export "$linker_variable=$CROSS_GNU_TRIPLE-gcc"
  export "CC_$target_variable=$CROSS_GNU_TRIPLE-gcc" "CXX_$target_variable=$CROSS_GNU_TRIPLE-g++"
  export "AR_$target_variable=$CROSS_GNU_TRIPLE-ar"
  export "PKG_CONFIG_ALLOW_CROSS_$target_variable=1"
  export "PKG_CONFIG_LIBDIR_$target_variable=/usr/lib/$CROSS_GNU_TRIPLE/pkgconfig:/usr/share/pkgconfig"
fi
echo "container glibc: $(ldd --version | head -n 1)"
XDG_CACHE_HOME=/xdg scripts/build-binary.sh /out
'

image_key=$(printf '%s\n' "$base_image" "$apt_snapshot" "$architecture" "$cross_gnu_triple" \
  "$rustup_version" "$rustup_init_sha256" "$rust_toolchain_file" "$setup_script" |
  sha256sum | cut -c1-16)
image_key_label=pd2-launcher.build-image-key
build_image=localhost/pd2-build-ubuntu-22.04:$image_key
if [[ $mode == image-key ]]; then
  echo "$image_key"
  exit 0
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
# Label separation is disabled instead of relabeling the checkout's Git
# directory; every mount except the caches and output is read-only.
# Podman selects the image platform with --arch; Docker has only --platform.
if [[ $(basename "$engine") == docker ]]; then
  platform_arguments=(--platform "linux/$container_architecture")
else
  platform_arguments=(--arch "$container_architecture")
fi
mkdir -p "$container_cache"

# Makes the build image unless it exists, and checks that an existing one
# was made for this key. Only a copy of the host trust roots is mounted; it
# bootstraps HTTPS to the snapshot archive, which then installs the image's
# own ca-certificates.
make_image() {
  (
    flock 8
    if "$engine" image inspect "$build_image" >/dev/null 2>&1; then
      built_for=$("$engine" image inspect --format "{{ index .Config.Labels \"$image_key_label\" }}" "$build_image")
      [[ $built_for == "$image_key" ]] || {
        echo "$build_image has the image key $built_for in its label, not $image_key" >&2
        exit 1
      }
      exit 0
    fi
    # The release workflow loads the image first and never makes it here.
    if [[ -n "${PD2_REQUIRE_BUILD_IMAGE:-}" ]]; then
      echo "the build image $build_image is missing, and PD2_REQUIRE_BUILD_IMAGE forbids making it here" >&2
      exit 1
    fi
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
    install -m 0644 "$host_ca_bundle" "$container_cache/host-ca.crt"
    echo "making the build image $build_image (once per image key)" >&2
    setup_container=pd2-build-setup-$image_key-$$
    "$engine" run --name "$setup_container" "${platform_arguments[@]}" --security-opt label=disable \
      --env APT_SNAPSHOT="$apt_snapshot" \
      --env RUSTUP_VERSION="$rustup_version" \
      --env RUSTUP_INIT_SHA256="$rustup_init_sha256" \
      --env RUST_TOOLCHAIN_FILE="$rust_toolchain_file" \
      --env PD2_TARGET_ARCHITECTURE="$architecture" \
      --env TARGET_DEBIAN_ARCHITECTURE="$target_debian_architecture" \
      --env CROSS_GNU_TRIPLE="$cross_gnu_triple" \
      --volume "$container_cache/host-ca.crt:/run/host-ca.crt:ro" \
      "$base_image" bash -c "$setup_script" >&2 || {
      "$engine" rm --force "$setup_container" >/dev/null 2>&1 || true
      exit 1
    }
    "$engine" commit --change "LABEL $image_key_label=$image_key" \
      "$setup_container" "$build_image" >/dev/null
    "$engine" rm --force "$setup_container" >/dev/null
  ) 8>"$container_cache/image.lock"
}

if [[ $mode == image ]]; then
  make_image
  echo "$build_image"
  exit 0
fi

# The audit runs the cross-compiled launcher under the host's qemu-user
# binfmt_misc handler, which the F flag makes usable inside the container.
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
fi

if [[ -n $(git -C "$repository_root" status --porcelain --untracked-files=normal) ]]; then
  echo "refusing to package a dirty worktree; the container builds the committed HEAD" >&2
  exit 1
fi
commit=$(git -C "$repository_root" rev-parse HEAD)
git_common_directory=$(cd "$repository_root" && cd "$(git rev-parse --git-common-dir)" && pwd -P)

make_image
# Docker writes the image ID with its sha256: prefix, Podman without.
image_id=sha256:$("$engine" image inspect --format '{{.Id}}' "$build_image" | sed 's/^sha256://')

mkdir -p "$output_directory" "$container_cache/cargo/registry" "$container_cache/xdg"
output_directory=$(cd "$output_directory" && pwd -P)
run_arguments=(
  run --rm "${platform_arguments[@]}" --security-opt label=disable
  --env SOURCE_COMMIT="$commit"
  --env PD2_TARGET_ARCHITECTURE="$architecture"
  --env CROSS_GNU_TRIPLE="$cross_gnu_triple"
  --env PD2_BUILD_IMAGE="$build_image"
  --env PD2_BUILD_IMAGE_ID="$image_id"
  --volume "$git_common_directory:/source.git:ro"
  --volume "$container_cache/cargo/registry:/cargo/registry"
  --volume "$container_cache/xdg:/xdg"
  --volume "$output_directory:/out"
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
if [[ -n "${PD2_FAST_BUILD:-}" ]]; then
  # The default, reusing build: a checkout and the faster device Cargo profile
  # that persist between runs. The build script, its audits, and the artifact
  # are otherwise the same.
  mkdir -p "$container_cache/build"
  run_arguments+=(--env PD2_FAST_BUILD=1 --volume "$container_cache/build:/build")
fi

"$engine" "${run_arguments[@]}" "$build_image" bash -c "$build_part"
