#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALLER="$ROOT_DIR/scripts/install-local.sh"
BUNDLE_ID="io.github.noricha-vr.quick-screen-recorder"

TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/qsr-install-test.XXXXXX")"
trap 'rm -rf "$TMP_ROOT"' EXIT

STUB_BIN="$TMP_ROOT/bin"
mkdir -p "$STUB_BIN"
TCC_LOG="$TMP_ROOT/tcc.log"
PGREP_COUNT_FILE="$TMP_ROOT/pgrep.count"

cat > "$STUB_BIN/codesign" << 'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "$1" == "--verify" ]]; then
  app="${@: -1}"
  [[ -f "$app/.stub-verify-fail" ]] && exit 1
  exit 0
fi
if [[ "$1" == "--force" ]]; then
  ident=""
  prev=""
  for arg in "$@"; do
    if [[ "$prev" == "--sign" ]]; then ident="$arg"; fi
    prev="$arg"
  done
  app="${@: -1}"
  if [[ "$ident" == "-" ]]; then
    printf 'Identifier=%s\nSignature=adhoc\nCDHash=resigned-adhoc\n' "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app/Contents/Info.plist")" > "$app/.stub-dvvv"
    printf 'designated => cdhash H"resigned-adhoc"\n' > "$app/.stub-dr"
  else
    printf 'Identifier=%s\nAuthority=%s\nCDHash=cert-signed-hash\n' "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app/Contents/Info.plist")" "$ident" > "$app/.stub-dvvv"
    printf 'designated => identifier "%s" and anchor apple generic\n' "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$app/Contents/Info.plist")" > "$app/.stub-dr"
  fi
  exit 0
fi
if [[ "$1" == "-dvvv" ]]; then
  app="$2"
  [[ -f "$app/.stub-dvvv" ]] || exit 1
  cat "$app/.stub-dvvv" >&2
  exit 0
fi
if [[ "$1" == "-d" && "$2" == "-r" && "$3" == "-" ]]; then
  app="$4"
  [[ -f "$app/.stub-dr" ]] && cat "$app/.stub-dr" >&2
  exit 0
fi
exit 1
EOF
chmod +x "$STUB_BIN/codesign"

cat > "$STUB_BIN/tccutil" << EOF
#!/usr/bin/env bash
set -euo pipefail
echo "ARGC=\$# ARGS=\$*" >> "$TCC_LOG"
if [[ "\${STUB_TCC_FAIL_SVC:-}" == "\${2:-}" ]]; then
  exit 1
fi
exit 0
EOF
chmod +x "$STUB_BIN/tccutil"

cat > "$STUB_BIN/pgrep" << EOF
#!/usr/bin/env bash
set -euo pipefail
count=0
[[ -f "$PGREP_COUNT_FILE" ]] && count="\$(cat "$PGREP_COUNT_FILE")"
count=\$((count + 1))
echo "\$count" > "$PGREP_COUNT_FILE"
case "\${STUB_PGREP_MODE:-not_running}" in
  not_running) exit 1 ;;
  running) exit 0 ;;
  error) exit 3 ;;
  running_on_second)
    if [[ "\$count" -ge 2 ]]; then exit 0; else exit 1; fi
    ;;
esac
exit 1
EOF
chmod +x "$STUB_BIN/pgrep"

make_fake_app() {
  local dir="$1"
  local bid="$2"
  local sig_mode="$3"
  local cdhash="$4"
  local cs_id="${5:-$bid}"
  rm -rf "$dir"
  mkdir -p "$dir/Contents/MacOS"
  cat > "$dir/Contents/Info.plist" << PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key>
  <string>$bid</string>
</dict>
</plist>
PLIST
  case "$sig_mode" in
    adhoc)
      printf 'Identifier=%s\nSignature=adhoc\nCDHash=%s\n' "$cs_id" "$cdhash" > "$dir/.stub-dvvv"
      printf 'designated => cdhash H"%s"\n' "$cdhash" > "$dir/.stub-dr"
      ;;
    cert)
      printf 'Identifier=%s\nAuthority=Developer ID Application: Test\nCDHash=%s\n' "$cs_id" "$cdhash" > "$dir/.stub-dvvv"
      printf 'designated => identifier "%s" and anchor apple generic\n' "$cs_id" > "$dir/.stub-dr"
      ;;
    corrupt)
      printf 'Identifier=%s\nUnknownField=wat\n' "$cs_id" > "$dir/.stub-dvvv"
      ;;
  esac
}

run_inst() {
  rm -f "$PGREP_COUNT_FILE"
  PATH="$STUB_BIN:$PATH" bash "$INSTALLER" "$@"
}

assert_log_lines() {
  local expected="$1"
  local actual=0
  [[ -f "$TCC_LOG" ]] && actual="$(wc -l < "$TCC_LOG" | tr -d ' ')"
  if [[ "$actual" -ne "$expected" ]]; then
    echo "FAIL: expected $expected tccutil calls, got $actual" >&2
    [[ -f "$TCC_LOG" ]] && cat "$TCC_LOG" >&2
    exit 1
  fi
}

APPS_DIR="$TMP_ROOT/Apps"
mkdir -p "$APPS_DIR"

# 1. Wrong CFBundleIdentifier in source app MUST be rejected with zero tccutil calls.
SRC_BAD="$TMP_ROOT/bad.app"
make_fake_app "$SRC_BAD" "com.example.other-app" "adhoc" "aaaa"
if run_inst --target-dir "$APPS_DIR" --source-app "$SRC_BAD" >/dev/null 2>&1; then
  echo "FAIL: wrong CFBundleIdentifier should have failed" >&2
  exit 1
fi
assert_log_lines 0

# 2. Mismatched codesign Identifier vs Info.plist MUST be rejected.
SRC_CS_MISMATCH="$TMP_ROOT/mismatch.app"
make_fake_app "$SRC_CS_MISMATCH" "$BUNDLE_ID" "adhoc" "aaaa" "com.example.spoofed"
if run_inst --target-dir "$APPS_DIR" --source-app "$SRC_CS_MISMATCH" >/dev/null 2>&1; then
  echo "FAIL: codesign Identifier mismatch should have failed" >&2
  exit 1
fi
assert_log_lines 0

# 3. Unparseable signature metadata MUST fail closed with zero tccutil calls.
SRC_CORRUPT="$TMP_ROOT/corrupt.app"
make_fake_app "$SRC_CORRUPT" "$BUNDLE_ID" "corrupt" "aaaa"
if run_inst --target-dir "$APPS_DIR" --source-app "$SRC_CORRUPT" >/dev/null 2>&1; then
  echo "FAIL: corrupt signature metadata should have failed" >&2
  exit 1
fi
assert_log_lines 0
rm -rf "$APPS_DIR/QuickScreenRecorder.app"

# 4. Fresh ad-hoc install resets exact 2 allowlisted services with exact 3 args each.
SRC_V1="$TMP_ROOT/v1.app"
make_fake_app "$SRC_V1" "$BUNDLE_ID" "adhoc" "hash1111"
run_inst --target-dir "$APPS_DIR" --source-app "$SRC_V1" >/dev/null
assert_log_lines 2
grep -qx "ARGC=3 ARGS=reset ScreenCapture $BUNDLE_ID" "$TCC_LOG"
grep -qx "ARGC=3 ARGS=reset Microphone $BUNDLE_ID" "$TCC_LOG"

# 5. Re-installing identical ad-hoc DR/CDHash skips tccutil reset.
: > "$TCC_LOG"
run_inst --target-dir "$APPS_DIR" --source-app "$SRC_V1" >/dev/null
assert_log_lines 0

# 6. --force-reset-tcc retries reset on identical ad-hoc DR.
: > "$TCC_LOG"
run_inst --target-dir "$APPS_DIR" --source-app "$SRC_V1" --force-reset-tcc >/dev/null
assert_log_lines 2

# 7. --no-reset-tcc skips reset even when ad-hoc DR changed.
: > "$TCC_LOG"
SRC_V2="$TMP_ROOT/v2.app"
make_fake_app "$SRC_V2" "$BUNDLE_ID" "adhoc" "hash2222"
run_inst --target-dir "$APPS_DIR" --source-app "$SRC_V2" --no-reset-tcc >/dev/null
assert_log_lines 0

# 8. Changed ad-hoc DR triggers automatic reset.
: > "$TCC_LOG"
SRC_V3="$TMP_ROOT/v3.app"
make_fake_app "$SRC_V3" "$BUNDLE_ID" "adhoc" "hash3333"
run_inst --target-dir "$APPS_DIR" --source-app "$SRC_V3" >/dev/null
assert_log_lines 2

# 9. Certificate-signed install (--sign-identity) preserves TCC even with --force-reset-tcc,
#    and leaves source bundle ad-hoc untouched.
: > "$TCC_LOG"
run_inst --target-dir "$APPS_DIR" --source-app "$SRC_V3" --sign-identity "Developer ID Application: Local" --force-reset-tcc >/dev/null
assert_log_lines 0
grep -q '^Signature=adhoc$' "$SRC_V3/.stub-dvvv"

# 10. Partial tccutil failure exits non-zero and prints retry guidance.
: > "$TCC_LOG"
SRC_V4="$TMP_ROOT/v4.app"
make_fake_app "$SRC_V4" "$BUNDLE_ID" "adhoc" "hash4444"
if OUT="$(STUB_TCC_FAIL_SVC=Microphone run_inst --target-dir "$APPS_DIR" --source-app "$SRC_V4" 2>&1)"; then
  echo "FAIL: partial tccutil failure should exit non-zero" >&2
  exit 1
fi
printf '%s\n' "$OUT" | grep -q "Microphone"
printf '%s\n' "$OUT" | grep -q -- "--force-reset-tcc"

# 11. Refuses to replace an existing app at target with a different CFBundleIdentifier.
: > "$TCC_LOG"
make_fake_app "$APPS_DIR/QuickScreenRecorder.app" "com.example.victim" "adhoc" "victimhash"
if run_inst --target-dir "$APPS_DIR" --source-app "$SRC_V4" >/dev/null 2>&1; then
  echo "FAIL: must refuse replacing an existing app with different bundle ID" >&2
  exit 1
fi
assert_log_lines 0
rm -rf "$APPS_DIR/QuickScreenRecorder.app"

# 12. Refuses to install when pgrep reports the app is running (exit 0).
: > "$TCC_LOG"
if STUB_PGREP_MODE=running run_inst --target-dir "$APPS_DIR" --source-app "$SRC_V4" >/dev/null 2>&1; then
  echo "FAIL: must refuse install while app is running" >&2
  exit 1
fi
assert_log_lines 0

# 13. Fails closed when pgrep encounters an internal error (exit 3 != 1).
: > "$TCC_LOG"
if STUB_PGREP_MODE=error run_inst --target-dir "$APPS_DIR" --source-app "$SRC_V4" >/dev/null 2>&1; then
  echo "FAIL: must fail closed when pgrep exits with error 3" >&2
  exit 1
fi
assert_log_lines 0

# 14. Re-checks running status right before swap (catches app launched mid-build/staging).
: > "$TCC_LOG"
if STUB_PGREP_MODE=running_on_second run_inst --target-dir "$APPS_DIR" --source-app "$SRC_V4" >/dev/null 2>&1; then
  echo "FAIL: must re-check running app before swap" >&2
  exit 1
fi
assert_log_lines 0
[[ ! -e "$APPS_DIR/QuickScreenRecorder.app" ]]

# 15. Refuses installing into dist/ (including dist/., dist/../dist, and case-insensitive DIST aliases).
mkdir -p "$ROOT_DIR/dist"
for bad_target in "$ROOT_DIR/dist" "$ROOT_DIR/dist/." "$ROOT_DIR/dist/../dist" "$ROOT_DIR/DIST"; do
  if [[ -d "$bad_target" ]]; then
    if run_inst --target-dir "$bad_target" --source-app "$SRC_V4" --sign-identity "Developer ID Application: Local" >/dev/null 2>&1; then
      echo "FAIL: must refuse installing into $bad_target" >&2
      exit 1
    fi
  fi
done

# 16. Sanitizes control characters (e.g. ANSI escape) from untrusted plist values in error output.
SRC_ESC="$TMP_ROOT/esc.app"
make_fake_app "$SRC_ESC" "$(printf 'com.evil.\033[31mred')" "adhoc" "eeee"
OUT_ESC="$(run_inst --target-dir "$APPS_DIR" --source-app "$SRC_ESC" 2>&1 || true)"
if printf '%s' "$OUT_ESC" | LC_ALL=C grep -q $'\033'; then
  echo "FAIL: raw ESC control byte leaked to terminal output" >&2
  exit 1
fi

# 17. Rejects newline-containing paths (prevents codesign line injection).
NL_DIR="$TMP_ROOT/$(printf 'bad\nSignature=adhoc')"
mkdir -p "$NL_DIR"
if run_inst --target-dir "$NL_DIR" --source-app "$SRC_V4" >/dev/null 2>&1; then
  echo "FAIL: must reject newline-containing target directory" >&2
  exit 1
fi

# 18. Refuses concurrent install when lock directory is held.
mkdir -p "$APPS_DIR/.qsr-install.lock"
if run_inst --target-dir "$APPS_DIR" --source-app "$SRC_V4" >/dev/null 2>&1; then
  rmdir "$APPS_DIR/.qsr-install.lock"
  echo "FAIL: must refuse concurrent install while lock exists" >&2
  exit 1
fi
rmdir "$APPS_DIR/.qsr-install.lock"

# 19. Post-swap verification failure rolls back cleanly to the previous app!
: > "$TCC_LOG"
run_inst --target-dir "$APPS_DIR" --source-app "$SRC_V3" >/dev/null
# Make target fail verification ONLY when installed at target path (`--verify` checks sentinel):
SRC_FAIL_POST="$TMP_ROOT/fail-post.app"
make_fake_app "$SRC_FAIL_POST" "$BUNDLE_ID" "adhoc" "badpost"
# Simulate post-swap verify failure by making codesign --verify fail ONLY for resolved $APPS_DIR/QuickScreenRecorder.app
REAL_APPS_DIR="$(cd "$APPS_DIR" && pwd -P)"
cat > "$STUB_BIN/codesign" << EOF
#!/usr/bin/env bash
set -euo pipefail
if [[ "\$1" == "--verify" ]]; then
  app="\${@: -1}"
  if [[ "\$app" == "$REAL_APPS_DIR/QuickScreenRecorder.app" ]] && grep -q 'CDHash=badpost' "\$app/.stub-dvvv" 2>/dev/null; then
    exit 1
  fi
  exit 0
fi
if [[ "\$1" == "-dvvv" ]]; then
  cat "\$2/.stub-dvvv" >&2
  exit 0
fi
if [[ "\$1" == "-d" && "\$2" == "-r" && "\$3" == "-" ]]; then
  [[ -f "\$4/.stub-dr" ]] && cat "\$4/.stub-dr" >&2
  exit 0
fi
exit 1
EOF
chmod +x "$STUB_BIN/codesign"
: > "$TCC_LOG"
if run_inst --target-dir "$APPS_DIR" --source-app "$SRC_FAIL_POST" >/dev/null 2>&1; then
  echo "FAIL: post-swap verify failure should have failed" >&2
  exit 1
fi
assert_log_lines 0
grep -q 'CDHash=hash3333' "$APPS_DIR/QuickScreenRecorder.app/.stub-dvvv"

echo "INSTALL_LOCAL_STUB_TESTS_OK (19/19 cases passed)"
