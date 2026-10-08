#!/bin/bash
# Submit a signed disk image to Apple and staple the ticket onto it.
#
# Gatekeeper checks the image a browser saved. A notarized app inside an
# image that has no ticket of its own is rejected, and the app never opens.
# tauri-action notarizes the app. This does the image.
#
#   staple-dmg.sh --built '*aarch64*.dmg'     # the image this job just built
#   staple-dmg.sh --tag v0.1.25               # images already on that release
#
# Uses bash 3.2, which is /bin/bash on the macOS runner.

set -euo pipefail

if [ -z "${APPLE_ID:-}" ] || [ -z "${APPLE_PASSWORD:-}" ] || [ -z "${APPLE_TEAM_ID:-}" ]; then
  echo "APPLE_ID, APPLE_PASSWORD and APPLE_TEAM_ID are required. An unstapled image would be published and then refused by Gatekeeper." >&2
  exit 1
fi

staple_one() {
  src="$1"
  echo "submitting ${src}"
  xcrun notarytool submit "$src" \
    --apple-id "$APPLE_ID" \
    --team-id "$APPLE_TEAM_ID" \
    --password "$APPLE_PASSWORD" \
    --wait

  # The status flips to Accepted a few seconds before the ticket can be fetched.
  n=0
  while [ "$n" -lt 8 ]; do
    n=$((n + 1))
    if xcrun stapler staple "$src"; then
      break
    fi
    if [ "$n" -eq 8 ]; then
      echo "no ticket could be stapled to ${src}" >&2
      exit 1
    fi
    sleep 15
  done

  out=$(xcrun stapler validate "$src" 2>&1) || true
  echo "$out"
  echo "$out" | grep -q "The validate action worked"
}

upload_one() {
  src="$1"
  tag="$2"
  gh release upload "$tag" "$src" --clobber
  echo "uploaded $(basename "$src") to ${tag}"
}

case "${1:-}" in
  --built)
    glob="${2:-*.dmg}"
    files=$(find target -type f -path '*/bundle/*' -name "$glob")
    if [ -z "$files" ]; then
      echo "no disk image matching ${glob} under target/" >&2
      find target -type d -name bundle >&2 || true
      exit 1
    fi
    # A pipe would run the loop in a subshell, and a failed notarization
    # would not fail the job. Split on newlines; these names have none.
    IFS='
'
    for src in $files; do
      [ -n "$src" ] || continue
      staple_one "$src"
      # tauri-action already published this filename, unstapled. Replace it.
      upload_one "$src" "${GITHUB_REF_NAME}"
    done
    ;;
  --tag)
    tag="${2:-}"
    if [ -z "$tag" ]; then
      echo "--tag needs a release tag" >&2
      exit 1
    fi
    dir=$(mktemp -d)
    gh release download "$tag" --pattern "*.dmg" --dir "$dir" --clobber
    files=$(find "$dir" -type f -name "*.dmg")
    if [ -z "$files" ]; then
      echo "no disk image on ${tag}" >&2
      exit 1
    fi
    IFS='
'
    for src in $files; do
      [ -n "$src" ] || continue
      staple_one "$src"
      upload_one "$src" "$tag"
    done
    ;;
  *)
    echo "usage: staple-dmg.sh --built <glob> | --tag <tag>" >&2
    exit 2
    ;;
esac
