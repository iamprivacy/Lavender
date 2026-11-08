#!/bin/bash
# Builds a signed debug Lavender.app for the update test (macOS).
# Usage: build.sh <release number> <output dir>
# The bundle uses its own identifier, so it never touches a real Lavender's
# settings or library, and LAVENDER_UPDATE_TEST_SERVER points its update
# check at the local server instead of GitHub.
set -euo pipefail
N="$1"
OUT="$2"
: "${REPO:?}" "${KEY:?}" "${WORK:?}"
cd "$REPO"
printf '{ "identifier": "io.github.iamprivacy.lavender.e2etest" }\n' > "$WORK/e2e.conf.json"
TARGET=$(cd src-tauri && cargo metadata --format-version 1 --no-deps |
  python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
LAVENDER_UPDATE_TEST_SERVER=http://127.0.0.1:8765 LAVENDER_RELEASE="$N" \
  TAURI_SIGNING_PRIVATE_KEY="$KEY" TAURI_SIGNING_PRIVATE_KEY_PASSWORD="" \
  npx tauri build --debug --bundles app \
  --config src-tauri/tauri.updater.conf.json --config "$WORK/e2e.conf.json"
B="$TARGET/debug/bundle/macos"
rm -rf "$OUT" && mkdir -p "$OUT"
cp -R "$B/Lavender.app" "$OUT/"
cp "$B/Lavender.app.tar.gz" "$B/Lavender.app.tar.gz.sig" "$OUT/"
