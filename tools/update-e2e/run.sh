#!/bin/bash
# End-to-end test of the in-app update on macOS: builds release 1 and
# release 2 as signed debug bundles, serves fake releases on 127.0.0.1:8765,
# installs release 1, updates it to release 2 through the app's own commands
# (driven over WebDriver on :4445), restarts into it, then checks that every
# tampered release is refused and leaves the app untouched.
#
# Needs the private release key (default: lavender-release.key next to the
# main checkout, see RELEASING.md). Usage: tools/update-e2e/run.sh
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
export REPO="$(cd "$HERE/../.." && pwd)"
# Resolved to its real path: Tauri refuses to run from a path with a symlink
# in it (and /tmp is one on macOS), which the updater reports as a write error.
WORK="${WORK:-/private/tmp/lavender-update-e2e}"
mkdir -p "$WORK"
export WORK="$(cd -P "$WORK" && pwd)"
export KEY="${KEY:-$(cd "$(git -C "$REPO" rev-parse --git-common-dir)/.." && pwd)/lavender-release.key}"
# The app version the builds carry (release N is $VERSION-local.N).
export VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$REPO/src-tauri/Cargo.toml" | head -n 1)"
[ -n "$VERSION" ] || { echo "no version in src-tauri/Cargo.toml"; exit 1; }
export PATH="$(dirname "$(rustup which cargo)"):$PATH"
APP="$WORK/install/Lavender.app/Contents/MacOS/Lavender"
WD="python3 $HERE/wd.py"
FAILED=0

check() { # $1 = description, $2 = expected substring, $3 = actual
  if [[ "$3" == *"$2"* ]]; then echo "PASS $1"; else echo "FAIL $1: got $3"; FAILED=1; fi
}

launch() {
  pkill -f "$APP" 2>/dev/null; sleep 2
  rm -rf "$WORK/install" && mkdir -p "$WORK/install" && cp -R "$WORK/old/Lavender.app" "$WORK/install/"
  (nohup "$APP" > "$WORK/app.log" 2>&1 &)
  wait_app
}

wait_app() {
  for _ in $(seq 1 30); do curl -sf http://127.0.0.1:4445/status >/dev/null && break; sleep 2; done
  sleep 3
  $WD new >/dev/null
  $WD listen app-update-progress >/dev/null
}

# Runs install_app_update and prints the last phase and message.
install_outcome() {
  $WD js "window.__events=[]; return 1" >/dev/null
  $WD invoke install_app_update >/dev/null
  for _ in $(seq 1 60); do
    sleep 1
    last=$($WD js "const e=window.__events||[]; const l=e[e.length-1]; return l ? l.phase + ' | ' + (l.message||'') : ''")
    case "$last" in *ready*|*failed*|*cancelled*) break ;; esac
  done
  echo "$last"
}

test -f "$KEY" || { echo "missing release key: $KEY"; exit 1; }
mkdir -p "$WORK"
echo "building release 1 and release 2 (debug bundles)..."
bash "$HERE/build.sh" 1 "$WORK/old" > "$WORK/build1.log" 2>&1 || { echo "build 1 failed, see $WORK/build1.log"; exit 1; }
bash "$HERE/build.sh" 2 "$WORK/new" > "$WORK/build2.log" 2>&1 || { echo "build 2 failed, see $WORK/build2.log"; exit 1; }
python3 "$HERE/prepare.py" || exit 1

pkill -f "$HERE/server.py" 2>/dev/null
echo normal > "$WORK/mode.txt"
(nohup python3 "$HERE/server.py" > "$WORK/server.log" 2>&1 &)
sleep 1

launch
check "release 1 runs" "\"currentVersion\": \"$VERSION-local.1\"" "$($WD invoke get_app_update_info)"
check "release 2 is offered" '"canInstall": true' "$($WD invoke check_app_update)"
: > "$WORK/requests.log"
check "install verifies and installs" "ready | Lavender $VERSION-local.2 is installed" "$(install_outcome)"
# latest.json is fetched twice: once by the app, to hash it against the signed
# manifest, then by the updater, whose pick is checked against that copy.
check "manifest is fetched before the package" \
  "$(printf 'normal /api/latest\nnormal /dl/normal/SHA256SUMS\nnormal /dl/normal/SHA256SUMS.sig\nnormal /dl/normal/latest.json\nnormal /dl/normal/latest.json\nnormal /dl/normal/Lavender_aarch64.app.tar.gz')" \
  "$(cat "$WORK/requests.log")"
cmp -s "$APP" "$WORK/new/Lavender.app/Contents/MacOS/Lavender" && r=replaced || r=unchanged
check "the bundle on disk is release 2" replaced "$r"
# The app exits while restarting, so the call is fired and not awaited.
$WD js "setTimeout(() => window.__TAURI_INTERNALS__.invoke('restart_for_app_update'), 100); return 1" >/dev/null
sleep 4
wait_app
check "the restarted app is release 2" "\"currentVersion\": \"$VERSION-local.2\"" "$($WD invoke get_app_update_info)"
check "release 2 is up to date" '"status": "up_to_date"' "$($WD invoke check_app_update)"

launch
echo badsig > "$WORK/mode.txt"; : > "$WORK/requests.log"
check "a tampered manifest is refused" "the manifest signature does not match" "$(install_outcome)"
grep -q "Lavender_aarch64.app.tar.gz" "$WORK/requests.log" && r=downloaded || r=not-downloaded
check "nothing is downloaded after a bad signature" not-downloaded "$r"
echo badhash > "$WORK/mode.txt"
check "a wrong package hash is refused" "checksum mismatch" "$(install_outcome)"
echo mismatch > "$WORK/mode.txt"
check "a manifest of another release is refused" "belongs to a different release" "$(install_outcome)"
echo nomanifest > "$WORK/mode.txt"
check "a release without a manifest is not installable" "This release has no signed update manifest." "$($WD invoke check_app_update)"
check "and installing it is refused" "the release has no signed manifest" "$(install_outcome)"
echo older > "$WORK/mode.txt"
check "an older release is not offered" '"status": "up_to_date"' "$($WD invoke check_app_update)"
echo none > "$WORK/mode.txt"
check "no published release is reported" "No Lavender release has been published yet." "$($WD invoke check_app_update)"
cmp -s "$APP" "$WORK/old/Lavender.app/Contents/MacOS/Lavender" && r=unchanged || r=replaced
check "refused updates leave release 1 in place" unchanged "$r"

pkill -f "$APP"; pkill -f "$HERE/server.py"
echo "test app data: ~/Library/Application Support/io.github.iamprivacy.lavender.e2etest"
[ "$FAILED" = 0 ] && echo "ALL PASSED" || { echo "SOME FAILED"; exit 1; }
