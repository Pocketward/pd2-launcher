#!/usr/bin/env bash
# Build the standalone launcher executable for this host's architecture:
# the same binary as the AppImage and the Debian package, compiled with the
# standalone identity, which it attests by itself when run as is. Run it
# from a clean, committed checkout, normally inside the pinned ubuntu:22.04
# container (scripts/build-appimage-container.sh <dir> <arch> binary).
set -euo pipefail

repository_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
output_directory=${1:-"$repository_root/dist"}
package_version=0.1.0
architecture=${PD2_TARGET_ARCHITECTURE:-$(uname -m)}
case "$architecture" in
  aarch64) cargo_features= ;;
  # The packaged x86-64 artifact admits its own host row.
  x86_64) cargo_features=x86_64-host-row ;;
  *)
    echo "Packaging supports only aarch64 or x86_64 targets, not $architecture" >&2
    exit 1
    ;;
esac
# PD2_RELEASE_BUILD=1 builds without a dev build's testing additions
# (src/dev_build.rs).
if [[ -n "${PD2_RELEASE_BUILD:-}" ]]; then
  cargo_features=${cargo_features:+$cargo_features,}release-build
fi
package_identity="native-${architecture}-binary"
artifact_name="pd2-launcher-${package_version}-${architecture}"
launcher_cache_root=${XDG_CACHE_HOME:-${HOME:?HOME is required}/.cache}
build_lock="$launcher_cache_root/pd2launcher-build.lock"
target_directory="$launcher_cache_root/pd2launcher-target"
# PD2_TARGET_ARCHITECTURE (the container build sets it) cross-compiles for
# another architecture; the cross linker and target libraries come from the
# environment.
cargo_target_arguments=()
# PD2_CARGO_PROFILE (the container's reusing build sets it) names another
# Cargo profile; releases always use the release profile.
cargo_profile=${PD2_CARGO_PROFILE:-release}
cargo_profile_arguments=(--release)
if [[ "$cargo_profile" != release ]]; then
  cargo_profile_arguments=(--profile "$cargo_profile")
fi
release_directory="$target_directory/$cargo_profile"
if [[ "$architecture" != "$(uname -m)" ]]; then
  cargo_target_arguments=(--target "$architecture-unknown-linux-gnu")
  release_directory="$target_directory/$architecture-unknown-linux-gnu/$cargo_profile"
fi

for command_name in cargo file flock git install readelf sha256sum strings touch; do
  command -v "$command_name" >/dev/null || {
    echo "required build command is unavailable: $command_name" >&2
    exit 1
  }
done

if [[ -n $(git -C "$repository_root" status --porcelain --untracked-files=normal) ]]; then
  echo "refusing to package a dirty worktree" >&2
  exit 1
fi

source_date_epoch=${SOURCE_DATE_EPOCH:-$(git -C "$repository_root" show -s --format=%ct HEAD)}
if [[ ! "$source_date_epoch" =~ ^[1-9][0-9]*$ ]]; then
  echo "SOURCE_DATE_EPOCH must be a positive integer" >&2
  exit 1
fi
export SOURCE_DATE_EPOCH="$source_date_epoch"

mkdir -p "$output_directory" "$target_directory"
output_directory=$(cd "$output_directory" && pwd -P)
# Named in saved problem reports (docs/DIAGNOSTICS.md).
launcher_commit=$(git -C "$repository_root" rev-parse --short=12 HEAD 2>/dev/null || echo unknown)
flock "$build_lock" env \
  CARGO_TARGET_DIR="$target_directory" \
  PD2_LAUNCHER_ARTIFACT_IDENTITY="$package_identity" \
  PD2_LAUNCHER_COMMIT="$launcher_commit" \
  cargo build --manifest-path "$repository_root/Cargo.toml" --locked "${cargo_profile_arguments[@]}" \
  ${cargo_features:+--features "$cargo_features"} "${cargo_target_arguments[@]}"

stage_root=$(mktemp -d "${TMPDIR:-/tmp}/pd2launcher-binary.XXXXXX")
artifact_partial=$(mktemp "$output_directory/.${artifact_name}.partial.XXXXXX")
trap 'rm -rf "$stage_root"; rm -f "$artifact_partial"' EXIT
staged="$stage_root/$artifact_name"
install -m 0755 "$release_directory/pd2-launcher" "$staged"
touch -d "@$SOURCE_DATE_EPOCH" "$staged"

required_glibc=$("$repository_root/scripts/audit-launcher-elf.sh" "$staged" "$architecture")
"$repository_root/scripts/audit-package-contents.sh" "$stage_root"
# Run as is, with no marker in the environment, it attests itself.
identity_output=$(env -u PD2_LAUNCHER_APPIMAGE_IDENTITY "$staged" --verify-package-identity) || {
  echo "the standalone launcher rejected its compiled identity" >&2
  exit 1
}
if [[ "$identity_output" != "$package_identity" ]]; then
  echo "the standalone launcher returned an unexpected package identity: $identity_output" >&2
  exit 1
fi

install -m 0755 "$staged" "$artifact_partial"
touch -d "@$SOURCE_DATE_EPOCH" "$artifact_partial"
artifact_path="$output_directory/$artifact_name"
mv "$artifact_partial" "$artifact_path"
artifact_sha256=$(sha256sum "$artifact_path" | cut -d' ' -f1)
printf '%s  %s\n' "$artifact_sha256" "$artifact_name" > "$output_directory/${artifact_name}.sha256"
cat > "$output_directory/${artifact_name}.build-evidence.txt" <<EVIDENCE
artifact=$artifact_name
sha256=$artifact_sha256
git_commit=$(git -C "$repository_root" rev-parse HEAD)
source_date_epoch=$SOURCE_DATE_EPOCH
source_tree=clean
architecture=$architecture
build_host=$(uname -m)
cargo_profile=$cargo_profile
release_build=$([[ -n "${PD2_RELEASE_BUILD:-}" ]] && echo 1 || echo 0)
package_identity=$package_identity
required_glibc=$required_glibc
rustc=$(rustc --version)
EVIDENCE

echo "standalone launcher audit passed (requires GLIBC_$required_glibc)"
echo "built $artifact_path"
echo "SHA-256 $artifact_sha256"
