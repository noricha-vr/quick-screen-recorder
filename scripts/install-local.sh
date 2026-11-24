#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
APP_NAME="QuickScreenRecorder.app"
BIN_NAME="quick-screen-recorder"
EXPECTED_BUNDLE_ID="io.github.noricha-vr.quick-screen-recorder"
TCC_SERVICES=("ScreenCapture" "Microphone")

TARGET_DIR="/Applications"
SOURCE_APP=""
NO_BUILD=0
SIGN_IDENTITY=""
FORCE_RESET_TCC=0
NO_RESET_TCC=0
SKIP_RUNNING_CHECK=0

usage() {
  cat << 'EOF'
Usage: scripts/install-local.sh [options]

Safely installs QuickScreenRecorder.app to /Applications and, when installed with an
ad-hoc signature whose Designated Requirement / CDHash changed, automatically resets
stale macOS TCC entries for this app's explicit services (ScreenCapture, Microphone).

Options:
  --target-dir <dir>       Destination directory (default: /Applications)
  --source-app <path>      Install from an existing .app bundle (implies --no-build)
  --no-build               Skip running scripts/bundle-app.sh
  --sign-identity <id>     Re-sign ONLY the installed copy with a local dev identity
                           (never modifies dist/; omits TCC reset when cert-signed)
  --force-reset-tcc        Reset allowlisted TCC services for ad-hoc installs even if
                           DR/CDHash is unchanged (still refused for cert-signed apps)
  --no-reset-tcc           Skip automatic TCC reset
  --skip-running-check     Skip checking whether quick-screen-recorder is running
  -h, --help               Show this help
EOF
}

sanitize_print() {
  printf '%s' "$1" | LC_ALL=C tr '\000-\037\177' '?'
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --target-dir)
      [[ $# -ge 2 ]] || { echo "ERROR: --target-dir requires a path" >&2; exit 2; }
      TARGET_DIR="$2"
      shift 2
      ;;
    --source-app)
      [[ $# -ge 2 ]] || { echo "ERROR: --source-app requires a path" >&2; exit 2; }
      SOURCE_APP="$2"
      NO_BUILD=1
      shift 2
      ;;
    --no-build)
      NO_BUILD=1
      shift
      ;;
    --sign-identity)
      [[ $# -ge 2 ]] || { echo "ERROR: --sign-identity requires an identity" >&2; exit 2; }
      SIGN_IDENTITY="$2"
      shift 2
      ;;
    --force-reset-tcc)
      FORCE_RESET_TCC=1
      shift
      ;;
    --no-reset-tcc)
      NO_RESET_TCC=1
      shift
      ;;
    --skip-running-check)
      SKIP_RUNNING_CHECK=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "ERROR: Unknown option '$(sanitize_print "$1")'" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ "$FORCE_RESET_TCC" -eq 1 && "$NO_RESET_TCC" -eq 1 ]]; then
  echo "ERROR: --force-reset-tcc and --no-reset-tcc cannot be combined" >&2
  exit 2
fi

if [[ -z "$SOURCE_APP" ]]; then
  SOURCE_APP="$ROOT_DIR/dist/$APP_NAME"
fi

if [[ ! -d "$TARGET_DIR" || -L "$TARGET_DIR" ]]; then
  echo "ERROR: Target directory is missing or a symlink: $(sanitize_print "$TARGET_DIR")" >&2
  exit 1
fi
REAL_TARGET_DIR="$(cd "$TARGET_DIR" && pwd -P)"
DIST_DIR="$ROOT_DIR/dist"
if [[ -e "$DIST_DIR" ]]; then
  REAL_DIST_DIR="$(cd "$DIST_DIR" && pwd -P)"
else
  REAL_DIST_DIR="$DIST_DIR"
fi
if [[ "$REAL_TARGET_DIR" == "$REAL_DIST_DIR" || "$REAL_TARGET_DIR" == "$REAL_DIST_DIR/"* ]]; then
  echo "ERROR: Refusing to install into dist/ (public bundle directory must remain untouched and ad-hoc)" >&2
  exit 1
fi
TARGET_APP="$REAL_TARGET_DIR/$APP_NAME"

# Fail closed if the app is running OR if `pgrep` itself fails (only exit code 1 means "not running").
assert_app_not_running() {
  [[ "$SKIP_RUNNING_CHECK" -eq 1 ]] && return 0
  local rc=0
  pgrep -x "$BIN_NAME" >/dev/null 2>&1 || rc=$?
  if [[ "$rc" -eq 0 ]]; then
    echo "ERROR: $APP_NAME ($BIN_NAME) is currently running." >&2
    echo "Finish/save any active recording and quit the app cleanly before running make install." >&2
    exit 1
  elif [[ "$rc" -ne 1 ]]; then
    echo "ERROR: Failed to verify running process state (pgrep exited $rc); aborting safely." >&2
    exit 1
  fi
}

read_plist_bundle_id() {
  local app_path="$1"
  local plist="$app_path/Contents/Info.plist"
  [[ -f "$plist" && ! -L "$plist" ]] || return 1
  /usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$plist" 2>/dev/null
}

verify_app_bundle() {
  local app_path="$1"
  local label="$2"
  local safe_path
  safe_path="$(sanitize_print "$app_path")"
  if [[ ! -d "$app_path" || -L "$app_path" ]]; then
    echo "ERROR: $label is missing or a symlink: $safe_path" >&2
    return 1
  fi
  local plist_id
  if ! plist_id="$(read_plist_bundle_id "$app_path")"; then
    echo "ERROR: Failed to read CFBundleIdentifier from $label ($safe_path)" >&2
    return 1
  fi
  if [[ "$plist_id" != "$EXPECTED_BUNDLE_ID" ]]; then
    echo "ERROR: $label CFBundleIdentifier mismatch: got '$(sanitize_print "$plist_id")', expected '$EXPECTED_BUNDLE_ID'" >&2
    return 1
  fi
  if ! codesign --verify --deep --strict "$app_path" >/dev/null 2>&1; then
    echo "ERROR: codesign --verify --deep --strict failed for $label ($safe_path)" >&2
    return 1
  fi
  local cs_out cs_id
  if ! cs_out="$(codesign -dvvv "$app_path" 2>&1)"; then
    echo "ERROR: Failed to inspect codesign metadata for $label ($safe_path)" >&2
    return 1
  fi
  cs_id="$(printf '%s\n' "$cs_out" | awk -F= '/^Identifier=/{print $2; exit}')"
  if [[ "$cs_id" != "$EXPECTED_BUNDLE_ID" ]]; then
    echo "ERROR: $label codesign Identifier mismatch: got '$(sanitize_print "$cs_id")', expected '$EXPECTED_BUNDLE_ID'" >&2
    return 1
  fi
  return 0
}

# Prints "<is_adhoc:0|1>|<fingerprint>" or fails closed when signature metadata cannot be parsed.
inspect_signature_fingerprint() {
  local app_path="$1"
  local cs_out dr_out is_adhoc cdhashes fingerprint cs_id
  cs_out="$(codesign -dvvv "$app_path" 2>&1)" || return 1
  cs_id="$(printf '%s\n' "$cs_out" | awk -F= '/^Identifier=/{print $2; exit}')"
  [[ "$cs_id" == "$EXPECTED_BUNDLE_ID" ]] || return 1

  if printf '%s\n' "$cs_out" | grep -q '^Signature=adhoc$'; then
    is_adhoc=1
  elif printf '%s\n' "$cs_out" | grep -q '^Authority='; then
    is_adhoc=0
  else
    return 1
  fi

  cdhashes="$(printf '%s\n' "$cs_out" | grep -E '^(CDHash=|CandidateCDHash)' | LC_ALL=C sort -u | tr '\n' ';')"
  dr_out="$(codesign -d -r - "$app_path" 2>&1 | grep -E '^#?[[:space:]]*designated' | LC_ALL=C sort -u | tr '\n' ';' || true)"
  fingerprint="${cdhashes}|${dr_out}"
  [[ -n "$cdhashes" || -n "$dr_out" ]] || return 1
  printf '%s|%s\n' "$is_adhoc" "$fingerprint"
}

# 1. Refuse to build/overwrite while the app is running.
assert_app_not_running

# 2. Build public ad-hoc dist bundle unless --no-build / --source-app was specified.
if [[ "$NO_BUILD" -eq 0 ]]; then
  bash "$ROOT_DIR/scripts/bundle-app.sh"
fi

# 3. Verify source bundle before touching destination (and ensure source != destination).
verify_app_bundle "$SOURCE_APP" "source bundle"
REAL_SOURCE_APP="$(cd "$SOURCE_APP" && pwd -P)"
if [[ "$REAL_SOURCE_APP" == "$TARGET_APP" ]]; then
  echo "ERROR: Refusing to install bundle onto itself: $(sanitize_print "$TARGET_APP")" >&2
  exit 1
fi

# 4. Inspect existing installed bundle (if present) and fail closed if another app sits there.
OLD_EXISTED=0
OLD_IS_ADHOC=0
OLD_FINGERPRINT=""
if [[ -e "$TARGET_APP" || -L "$TARGET_APP" ]]; then
  if [[ -L "$TARGET_APP" || ! -d "$TARGET_APP" ]]; then
    echo "ERROR: Refusing to replace non-directory or symlink at $(sanitize_print "$TARGET_APP")" >&2
    exit 1
  fi
  existing_id="$(read_plist_bundle_id "$TARGET_APP" || true)"
  if [[ "$existing_id" != "$EXPECTED_BUNDLE_ID" ]]; then
    echo "ERROR: Existing bundle at $(sanitize_print "$TARGET_APP") has unexpected CFBundleIdentifier '$(sanitize_print "$existing_id")'" >&2
    exit 1
  fi
  OLD_EXISTED=1
  if old_sig="$(inspect_signature_fingerprint "$TARGET_APP" 2>/dev/null)"; then
    OLD_IS_ADHOC="${old_sig%%|*}"
    OLD_FINGERPRINT="${old_sig#*|}"
  else
    OLD_IS_ADHOC=0
    OLD_FINGERPRINT="__unparseable__"
  fi
fi

# 5. Copy into an exclusively created 0700 staging directory (immune to predictable symlink races).
STAGE_PARENT="$(mktemp -d "$REAL_TARGET_DIR/.qsr-install-stage.XXXXXX")"
chmod 0700 "$STAGE_PARENT"
BACKUP_PARENT="$(mktemp -d "$REAL_TARGET_DIR/.qsr-install-backup.XXXXXX")"
chmod 0700 "$BACKUP_PARENT"
STAGE_APP="$STAGE_PARENT/$APP_NAME"
BACKUP_APP="$BACKUP_PARENT/$APP_NAME"

cleanup_staging() {
  if [[ -d "$BACKUP_APP" && ! -e "$TARGET_APP" ]]; then
    mv "$BACKUP_APP" "$TARGET_APP" 2>/dev/null || true
  fi
  rm -rf "$STAGE_PARENT" "$BACKUP_PARENT"
}
trap cleanup_staging EXIT

ditto "$REAL_SOURCE_APP" "$STAGE_APP"

if [[ -n "$SIGN_IDENTITY" && "$SIGN_IDENTITY" != "-" ]]; then
  echo "==> Re-signing staged local copy with identity: $(sanitize_print "$SIGN_IDENTITY") (dist/ remains ad-hoc)"
  codesign --force --deep --sign "$SIGN_IDENTITY" "$STAGE_APP"
fi

verify_app_bundle "$STAGE_APP" "staged bundle"

# 6. Re-check running state right before atomic-style directory swap (catches launches during build).
assert_app_not_running

if [[ "$OLD_EXISTED" -eq 1 ]]; then
  mv "$TARGET_APP" "$BACKUP_APP"
fi
mv "$STAGE_APP" "$TARGET_APP"
if [[ -d "$BACKUP_APP" ]]; then
  rm -rf "$BACKUP_APP"
fi

# 7. Re-verify installed bundle and parse its Designated Requirement / CDHash.
verify_app_bundle "$TARGET_APP" "installed bundle"
if ! new_sig="$(inspect_signature_fingerprint "$TARGET_APP")"; then
  echo "ERROR: Failed to parse signature/DR of installed bundle ($(sanitize_print "$TARGET_APP")); aborting before TCC reset." >&2
  exit 1
fi
NEW_IS_ADHOC="${new_sig%%|*}"
NEW_FINGERPRINT="${new_sig#*|}"

echo "==> Installed $(sanitize_print "$TARGET_APP") (bundle ID: $EXPECTED_BUNDLE_ID, ad-hoc: $NEW_IS_ADHOC)"

# 8. Target-scoped TCC auto-reset (strictly ad-hoc only, allowlisted services only, exact bundle ID).
if [[ "$NO_RESET_TCC" -eq 1 ]]; then
  echo "==> Skipping TCC reset (--no-reset-tcc)"
  exit 0
fi

if [[ "$NEW_IS_ADHOC" -ne 1 ]]; then
  if [[ "$FORCE_RESET_TCC" -eq 1 ]]; then
    echo "==> Refusing --force-reset-tcc on certificate-signed bundle (preserving existing TCC grants)"
  else
    echo "==> Certificate-signed bundle detected; preserving existing macOS TCC permissions"
  fi
  exit 0
fi

SHOULD_RESET=0
RESET_REASON=""
if [[ "$FORCE_RESET_TCC" -eq 1 ]]; then
  SHOULD_RESET=1
  RESET_REASON="--force-reset-tcc requested"
elif [[ "$OLD_EXISTED" -eq 0 ]]; then
  SHOULD_RESET=1
  RESET_REASON="fresh ad-hoc install"
elif [[ "$OLD_IS_ADHOC" -ne 1 ]]; then
  SHOULD_RESET=1
  RESET_REASON="signature type changed to ad-hoc"
elif [[ "$OLD_FINGERPRINT" != "$NEW_FINGERPRINT" ]]; then
  SHOULD_RESET=1
  RESET_REASON="ad-hoc DR/CDHash changed"
fi

if [[ "$SHOULD_RESET" -eq 0 ]]; then
  echo "==> Ad-hoc DR/CDHash unchanged; skipping TCC reset (pass --force-reset-tcc to retry reset)"
  exit 0
fi

if [[ -z "$EXPECTED_BUNDLE_ID" || ! "$EXPECTED_BUNDLE_ID" =~ ^[A-Za-z0-9._-]+$ ]]; then
  echo "ERROR: Invalid bundle identifier; refusing to invoke tccutil" >&2
  exit 1
fi

# Re-check running state right before invoking TCC reset.
assert_app_not_running

echo "==> Resetting macOS TCC entries ($RESET_REASON) for $EXPECTED_BUNDLE_ID"
FAILED_SERVICES=()
for svc in "${TCC_SERVICES[@]}"; do
  case "$svc" in
    ScreenCapture|Microphone) ;;
    *)
      echo "ERROR: Disallowed TCC service '$svc'" >&2
      exit 1
      ;;
  esac
  if ! tccutil reset "$svc" "$EXPECTED_BUNDLE_ID"; then
    FAILED_SERVICES+=("$svc")
  fi
done

if [[ "${#FAILED_SERVICES[@]}" -gt 0 ]]; then
  echo "ERROR: tccutil reset failed for service(s): ${FAILED_SERVICES[*]}" >&2
  echo "Retry after checking macOS Privacy & Security: bash scripts/install-local.sh --no-build --force-reset-tcc" >&2
  exit 1
fi

echo "==> TCC reset succeeded for: ${TCC_SERVICES[*]} ($EXPECTED_BUNDLE_ID)"
echo "==> Note: Re-grant Screen Recording (and Microphone if used) once on next launch before live capture tests."
