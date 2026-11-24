#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="AirRec"
BUNDLE_NAME="AirRec.app"
BUNDLE_ID="io.github.noricha-vr.airrec"
BIN_NAME="airrec"
VERSION="$(awk -F'"' '/^version = / { print $2; exit }' "$ROOT_DIR/Cargo.toml")"

OUT_DIR="$ROOT_DIR/dist"
APP_DIR="$OUT_DIR/$BUNDLE_NAME"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"

echo "==> Building release binary ($BIN_NAME v$VERSION)"
CARGO_HOME_DIR="${CARGO_HOME:-$HOME/.cargo}"
RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$CARGO_HOME_DIR=/cargo --remap-path-prefix=$HOME=~ --remap-path-prefix=$ROOT_DIR=." \
  cargo build --release --locked --manifest-path "$ROOT_DIR/Cargo.toml"

rm -rf "$APP_DIR"
mkdir -p "$MACOS_DIR" "$RESOURCES_DIR"

cp "$ROOT_DIR/target/release/$BIN_NAME" "$MACOS_DIR/$BIN_NAME"
chmod 0755 "$MACOS_DIR/$BIN_NAME"

# Generate clean vector-style macOS .icns icon using Python + iconutil (no external dependencies)
ICONSET_TMP="$(mktemp -d "${TMPDIR:-/tmp}/qsr-icon.XXXXXX")"
ICONSET_DIR="$ICONSET_TMP/AppIcon.iconset"
mkdir -p "$ICONSET_DIR"

/usr/bin/python3 -I - "$ICONSET_DIR" << 'PY'
import math, struct, sys, zlib, os

out_dir = sys.argv[1]

def write_png(path, w, h, rgba_bytes):
    def chunk(tag, data):
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    raw = bytearray()
    stride = w * 4
    for y in range(h):
        raw.append(0)
        raw.extend(rgba_bytes[y * stride:(y + 1) * stride])
    ihdr = struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)
    png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(bytes(raw), 6)) + chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(png)

def clamp01(v):
    return 0.0 if v < 0.0 else (1.0 if v > 1.0 else v)

def render_icon(size):
    buf = bytearray(size * size * 4)
    scale = 128.0 / size
    aa_w = scale * 0.85

    # macOS squircle bounds in 128x128 design space
    sq_half = 54.0
    sq_rad = 24.5
    inner_sq = sq_half - sq_rad

    for y in range(size):
        v = (y + 0.5) * scale
        t = clamp01((v - 10.0) / 108.0)
        bg_r = 30.0 * (1.0 - t) + 15.0 * t
        bg_g = 38.0 * (1.0 - t) + 19.0 * t
        bg_b = 56.0 * (1.0 - t) + 29.0 * t
        ay = abs(v - 64.0)
        dy_sq = ay - inner_sq

        for x in range(size):
            u = (x + 0.5) * scale
            ax = abs(u - 64.0)
            dx_sq = ax - inner_sq
            sq_d = math.hypot(max(dx_sq, 0.0), max(dy_sq, 0.0)) + min(max(dx_sq, dy_sq), 0.0) - sq_rad
            if sq_d > aa_w:
                continue
            alpha = clamp01((aa_w - sq_d) / (2.0 * aa_w))

            r, g, b = bg_r, bg_g, bg_b

            # Subtle 1.2px inner rim highlight for macOS depth
            if sq_d > -1.8:
                rim = clamp01((sq_d + 1.8) / 1.8) * 0.14
                r = r * (1.0 - rim) + 255.0 * rim
                g = g * (1.0 - rim) + 255.0 * rim
                b = b * (1.0 - rim) + 255.0 * rim

            # Folded 1st-quadrant viewfinder corner bracket (exact rounded L-bracket with round caps)
            cx_h = 18.0 if ax < 18.0 else (30.0 if ax > 30.0 else ax)
            d_h = math.hypot(ax - cx_h, ay - 36.0)
            cy_v = 18.0 if ay < 18.0 else (30.0 if ay > 30.0 else ay)
            d_v = math.hypot(ax - 36.0, ay - cy_v)
            if ax >= 30.0 and ay >= 30.0:
                d_arc = abs(math.hypot(ax - 30.0, ay - 30.0) - 6.0)
            else:
                d_arc = 1e9
            d_br = min(d_h, d_v, d_arc) - 3.75
            if d_br < aa_w:
                bl = clamp01((aa_w - d_br) / (2.0 * aa_w))
                r = r * (1.0 - bl) + 238.0 * bl
                g = g * (1.0 - bl) + 244.0 * bl
                b = b * (1.0 - bl) + 255.0 * bl

            # Vivid radial-shaded red record core
            d_core = math.hypot(u - 64.0, v - 64.0) - 22.0
            if d_core < aa_w:
                bl = clamp01((aa_w - d_core) / (2.0 * aa_w))
                rt = clamp01(math.hypot(u - 57.0, v - 54.0) / 30.0)
                cr = 255.0 * (1.0 - rt) + 217.0 * rt
                cg = 94.0 * (1.0 - rt) + 38.0 * rt
                cb = 87.0 * (1.0 - rt) + 38.0 * rt
                r = r * (1.0 - bl) + cr * bl
                g = g * (1.0 - bl) + cg * bl
                b = b * (1.0 - bl) + cb * bl

            i = (y * size + x) * 4
            buf[i] = int(r + 0.5)
            buf[i + 1] = int(g + 0.5)
            buf[i + 2] = int(b + 0.5)
            buf[i + 3] = int(255.0 * alpha + 0.5)
    return buf

specs = [
    ("icon_16x16.png", 16),
    ("icon_16x16@2x.png", 32),
    ("icon_32x32.png", 32),
    ("icon_32x32@2x.png", 64),
    ("icon_128x128.png", 128),
    ("icon_128x128@2x.png", 256),
    ("icon_256x256.png", 256),
    ("icon_256x256@2x.png", 512),
    ("icon_512x512.png", 512),
    ("icon_512x512@2x.png", 1024),
]
cache = {}
for filename, px in specs:
    if px not in cache:
        cache[px] = render_icon(px)
    write_png(os.path.join(out_dir, filename), px, px, cache[px])
PY

if command -v iconutil >/dev/null 2>&1; then
  iconutil -c icns "$ICONSET_DIR" -o "$RESOURCES_DIR/AppIcon.icns" || true
fi
rm -rf "$ICONSET_TMP"

cat > "$CONTENTS_DIR/Info.plist" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleDisplayName</key>
  <string>$APP_NAME</string>
  <key>CFBundleExecutable</key>
  <string>$BIN_NAME</string>
  <key>CFBundleIconFile</key>
  <string>AppIcon</string>
  <key>CFBundleIdentifier</key>
  <string>$BUNDLE_ID</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>$APP_NAME</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>$VERSION</string>
  <key>CFBundleVersion</key>
  <string>$VERSION</string>
  <key>LSMinimumSystemVersion</key>
  <string>15.0</string>
  <key>LSUIElement</key>
  <true/>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSScreenCaptureUsageDescription</key>
  <string>AirRec captures your selected screen region or window locally on your Mac.</string>
  <key>NSMicrophoneUsageDescription</key>
  <string>AirRec records microphone audio locally only when Microphone capture is enabled.</string>
</dict>
</plist>
EOF

# Sign ad-hoc ("-") so Apple Silicon runs cleanly without embedding any personal certificate identity
codesign --force --deep --sign - "$APP_DIR"

echo "==> Built app bundle: $APP_DIR"
