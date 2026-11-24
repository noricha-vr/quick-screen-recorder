#!/usr/bin/env bash
set -euo pipefail

# Trust model: Local developer installer intended to run in a trusted user session
# (not a privilege boundary). System tools (codesign, tccutil, ps) resolve via PATH
# so scripts/test-install-local.sh can test behavior with stubs without altering real TCC.
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
APP_NAME="AirRec.app"
BIN_NAME="airrec"
LEGACY_BIN_NAME="quick-screen-recorder"
EXPECTED_BUNDLE_ID="io.github.noricha-vr.airrec"
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

Safely installs AirRec.app to /Applications and, when installed with an
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
  --skip-running-check     Skip checking whether airrec is running
  -h, --help               Show this help
EOF
}

sanitize_print() {
  printf '%s' "$1" | LC_ALL=C tr '\000-\037\177' '?'
}

reject_newlines() {
  local label="$1"
  local val="$2"
  if [[ "$val" == *$'\n'* || "$val" == *$'\r'* ]]; then
    echo "ERROR: $label must not contain newline characters" >&2
    exit 2
  fi
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

reject_newlines "--target-dir" "$TARGET_DIR"
reject_newlines "--source-app" "$SOURCE_APP"
reject_newlines "--sign-identity" "$SIGN_IDENTITY"

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
reject_newlines "resolved target directory" "$REAL_TARGET_DIR"

DIST_DIR="$ROOT_DIR/dist"
is_inside_dist_dir() {
  /usr/bin/python3 -I - "$1" "$2" << 'PY'
import os, sys

target = os.path.realpath(sys.argv[1])
dist = os.path.realpath(sys.argv[2])

# 1. Device/inode ancestor check catches case-insensitive APFS aliases (e.g. DIST vs dist) and symlinks.
if os.path.exists(dist):
    cur = target
    while True:
        try:
            if os.path.samefile(cur, dist):
                sys.exit(0)
        except OSError:
            pass
        parent = os.path.dirname(cur)
        if parent == cur:
            break
        cur = parent

# 2. Case-folded fallback when dist/ does not exist yet.
t_low = os.path.normcase(target).lower()
d_low = os.path.normcase(dist).lower()
if t_low == d_low or t_low.startswith(d_low.rstrip(os.sep) + os.sep):
    sys.exit(0)
sys.exit(1)
PY
}
if is_inside_dist_dir "$REAL_TARGET_DIR" "$DIST_DIR"; then
  echo "ERROR: Refusing to install into dist/ (public bundle directory must remain untouched and ad-hoc)" >&2
  exit 1
fi
TARGET_APP="$REAL_TARGET_DIR/$APP_NAME"

# Fail closed if the app is running OR if `ps` fails.
# Uses `ps -axo comm=` (full executable path) rather than `pgrep -x`, because macOS `pgrep -x`
# truncates process names to 15 chars (`MAXCOMLEN-1`).
assert_app_not_running() {
  [[ "$SKIP_RUNNING_CHECK" -eq 1 ]] && return 0
  local ps_out
  if ! ps_out="$(ps -axo comm= 2>/dev/null)"; then
    echo "ERROR: Failed to verify running process state (ps failed); aborting safely." >&2
    exit 1
  fi
  # Drain all input without exiting early inside awk so large process tables never
  # trigger SIGPIPE on the upstream printf under `set -o pipefail`.
  # Checks both `airrec` and legacy `quick-screen-recorder` (21 chars) so an older running
  # instance cannot keep holding the global ⇧⌘6 hotkey after installing AirRec.app.
  if printf '%s\n' "$ps_out" | awk -F/ -v bin="$BIN_NAME" -v legacy="$LEGACY_BIN_NAME" '{
      sub(/[[:space:]]+$/, "", $0);
      if ($NF == bin || $NF == legacy) { found = 1 }
    } END { exit(found ? 0 : 1) }'; then
    echo "ERROR: $APP_NAME ($BIN_NAME / $LEGACY_BIN_NAME) is currently running." >&2
    echo "Finish/save any active recording and quit the app cleanly before running make install." >&2
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
  cs_id="$(printf '%s\n' "$cs_out" | awk -F= '/^Identifier=/ && !seen {print $2; seen=1}')"
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
  cs_id="$(printf '%s\n' "$cs_out" | awk -F= '/^Identifier=/ && !seen {print $2; seen=1}')"
  [[ "$cs_id" == "$EXPECTED_BUNDLE_ID" ]] || return 1

  # Check `Authority=` first (certificate-signed bundles carry Authority and never Signature=adhoc).
  if printf '%s\n' "$cs_out" | awk '/^Authority=/{found=1} END{exit(found?0:1)}'; then
    is_adhoc=0
  elif printf '%s\n' "$cs_out" | awk '/^Signature=adhoc$/{found=1} END{exit(found?0:1)}'; then
    is_adhoc=1
  else
    return 1
  fi

  cdhashes="$(printf '%s\n' "$cs_out" | grep -E '^(CDHash=|CandidateCDHash)' | LC_ALL=C sort -u | tr '\n' ';')"
  dr_out="$(codesign -d -r - "$app_path" 2>&1 | grep -E '^#?[[:space:]]*designated' | LC_ALL=C sort -u | tr '\n' ';' || true)"
  fingerprint="${cdhashes}|${dr_out}"
  [[ -n "$cdhashes" || -n "$dr_out" ]] || return 1
  printf '%s|%s\n' "$is_adhoc" "$fingerprint"
}

# Atomic macOS kernel rename/exchange (`renamex_np`) that NEVER follows symlinks and NEVER nests
# into an existing directory.
# - mode "excl": atomically moves `src` -> `dst` ONLY IF `dst` does not exist (`RENAME_EXCL`).
# - mode "swap": atomically swaps `src` <-> `dst` (`RENAME_SWAP`), leaving the old bundle intact
#   at `src` as a rollback backup until post-install verification succeeds.
atomic_renamex() {
  local mode="$1"
  local src="$2"
  local dst="$3"
  /usr/bin/python3 -I - "$mode" "$src" "$dst" << 'PY'
import ctypes, errno, os, stat, sys

mode, src, dst = sys.argv[1], sys.argv[2], sys.argv[3]
libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
libc.renamex_np.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint]
libc.renamex_np.restype = ctypes.c_int

RENAME_SWAP = 0x00000002
RENAME_EXCL = 0x00000004

if mode == "swap":
    st = os.lstat(dst)
    if stat.S_ISLNK(st.st_mode) or not stat.S_ISDIR(st.st_mode):
        sys.stderr.write("ERROR: Destination is a symlink or non-directory during swap\n")
        sys.exit(1)
    flags = RENAME_SWAP
elif mode == "excl":
    flags = RENAME_EXCL
else:
    sys.exit(2)

rc = libc.renamex_np(os.fsencode(src), os.fsencode(dst), flags)
if rc != 0:
    err = ctypes.get_errno()
    sys.stderr.write(f"ERROR: renamex_np({mode}) failed: {os.strerror(err)} (errno {err})\n")
    sys.exit(1)
PY
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
reject_newlines "resolved source bundle" "$REAL_SOURCE_APP"
if [[ "$REAL_SOURCE_APP" == "$TARGET_APP" ]]; then
  echo "ERROR: Refusing to install bundle onto itself: $(sanitize_print "$TARGET_APP")" >&2
  exit 1
fi

# 4. Acquire exclusive lock in target directory so concurrent installs cannot race.
LOCK_DIR="$REAL_TARGET_DIR/.qsr-install.lock"
if ! mkdir "$LOCK_DIR" 2>/dev/null; then
  echo "ERROR: Another install-local.sh appears to be running (lock exists: $(sanitize_print "$LOCK_DIR"))" >&2
  exit 1
fi

STAGE_PARENT=""
STAGE_APP=""
SWAPPED_OLD_IN_STAGE=0
INSTALL_VERIFIED=0

cleanup_install() {
  if [[ "$SWAPPED_OLD_IN_STAGE" -eq 1 && "$INSTALL_VERIFIED" -eq 0 && -d "$STAGE_APP" ]]; then
    # Roll back atomically if post-swap verification failed; NEVER delete the old backup if rollback fails!
    if atomic_renamex swap "$STAGE_APP" "$TARGET_APP" 2>/dev/null; then
      SWAPPED_OLD_IN_STAGE=0
      rm -rf "$STAGE_PARENT"
    else
      echo "WARNING: Automatic rollback failed; previous app backup preserved untouched at $(sanitize_print "$STAGE_APP")" >&2
    fi
  elif [[ -n "$STAGE_PARENT" ]]; then
    rm -rf "$STAGE_PARENT"
  fi
  rmdir "$LOCK_DIR" 2>/dev/null || true
}
trap cleanup_install EXIT

# 5. Inspect existing installed bundle (if present) under lock and fail closed on mismatch.
OLD_EXISTED=0
OLD_IS_ADHOC=0
OLD_FINGERPRINT=""
OLD_DEV_INO=""
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
  OLD_DEV_INO="$(stat -f '%d:%i' "$TARGET_APP")"
  if old_sig="$(inspect_signature_fingerprint "$TARGET_APP" 2>/dev/null)"; then
    OLD_IS_ADHOC="${old_sig%%|*}"
    OLD_FINGERPRINT="${old_sig#*|}"
  else
    OLD_IS_ADHOC=0
    OLD_FINGERPRINT="__unparseable__"
  fi
fi

# 6. Copy into an exclusively created 0700 staging directory (immune to predictable symlink races).
STAGE_PARENT="$(mktemp -d "$REAL_TARGET_DIR/.qsr-install-stage.XXXXXX")"
chmod 0700 "$STAGE_PARENT"
STAGE_APP="$STAGE_PARENT/$APP_NAME"

if ! ditto_out="$(ditto "$REAL_SOURCE_APP" "$STAGE_APP" 2>&1)"; then
  echo "ERROR: ditto copy failed: $(sanitize_print "$ditto_out")" >&2
  exit 1
fi

if [[ -n "$SIGN_IDENTITY" && "$SIGN_IDENTITY" != "-" ]]; then
  echo "==> Re-signing staged local copy with identity: $(sanitize_print "$SIGN_IDENTITY") (dist/ remains ad-hoc)"
  if ! cs_resign_out="$(codesign --force --deep --sign "$SIGN_IDENTITY" "$STAGE_APP" 2>&1)"; then
    echo "ERROR: Local re-signing failed: $(sanitize_print "$cs_resign_out")" >&2
    exit 1
  fi
fi

verify_app_bundle "$STAGE_APP" "staged bundle"

# 7. Re-check running state right before atomic swap (catches launches during build/staging).
assert_app_not_running

if [[ "$OLD_EXISTED" -eq 1 ]]; then
  atomic_renamex swap "$STAGE_APP" "$TARGET_APP"
  SWAPPED_OLD_IN_STAGE=1
  # Verify the swapped-out directory is the exact inode we inspected before swap; if another
  # process replaced TARGET_APP mid-copy, roll back immediately without deleting it!
  swapped_dev_ino="$(stat -f '%d:%i' "$STAGE_APP" 2>/dev/null || true)"
  if [[ -z "$OLD_DEV_INO" || "$swapped_dev_ino" != "$OLD_DEV_INO" ]]; then
    echo "ERROR: Destination changed concurrently during install; rolling back safely." >&2
    exit 1
  fi
else
  atomic_renamex excl "$STAGE_APP" "$TARGET_APP"
fi

# 8. Re-verify installed bundle BEFORE removing the swapped-out backup (rollback on failure).
verify_app_bundle "$TARGET_APP" "installed bundle"
if ! new_sig="$(inspect_signature_fingerprint "$TARGET_APP")"; then
  echo "ERROR: Failed to parse signature/DR of installed bundle ($(sanitize_print "$TARGET_APP")); rolling back." >&2
  exit 1
fi
INSTALL_VERIFIED=1
NEW_IS_ADHOC="${new_sig%%|*}"
NEW_FINGERPRINT="${new_sig#*|}"

echo "==> Installed $(sanitize_print "$TARGET_APP") (bundle ID: $EXPECTED_BUNDLE_ID, ad-hoc: $NEW_IS_ADHOC)"

# 9. Target-scoped TCC auto-reset (strictly ad-hoc only, allowlisted services only, exact bundle ID).
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
