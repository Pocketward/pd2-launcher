#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 || ! -d "$1" ]]; then
  echo "usage: $0 <staged-package-directory>" >&2
  exit 2
fi

package_root=$(realpath "$1")
failure=0

while IFS= read -r -d '' entry; do
  relative=${entry#"$package_root"/}
  lowered=${relative,,}
  if [[ -L "$entry" ]]; then
    target=$(readlink "$entry")
    resolved=$(realpath -m "$(dirname "$entry")/$target")
    if [[ "$target" = /* || "$resolved" != "$package_root"/* ]]; then
      echo "unsafe package symlink: $relative -> $target" >&2
      failure=1
    fi
  fi
  case "/$lowered" in
    /.git|*/.git/*|/.env.local|*/.env.local|*/target/debug|*/target/debug/*|*/prototype|*/prototype/*|*/prototypes|*/prototypes/*)
      echo "development-only path in package: $relative" >&2
      failure=1
      ;;
    *.mpq|*/game.exe|*/patch_d2.mpq|*/d2client.dll|*/projectdiablo.dll|*/projectd2/*|*/lodpatch_114d.exe|*/wine-gecko-*.msi|*/runner/lutris-ge-proton8-26-x86_64/*)
      echo "downloaded runtime or proprietary payload in package: $relative" >&2
      failure=1
      ;;
  esac
done < <(find "$package_root" -mindepth 1 -print0)

while IFS= read -r -d '' file; do
  if strings -a "$file" | grep -E -q '/var/home/[^/]+/code/|/tmp/pd2launcher[^/ ]*|target/debug/|prototype/zero-to-play'; then
    echo "development path embedded in package file: ${file#"$package_root"/}" >&2
    failure=1
  fi
done < <(find "$package_root" -type f -print0)

if (( failure != 0 )); then
  exit 1
fi

echo "package content audit passed: $package_root"
