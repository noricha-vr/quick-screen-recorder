#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_NAME="Quick Screen Recorder"
BUNDLE_NAME="QuickScreenRecorder.app"
BUNDLE_ID="io.github.noricha-vr.quick-screen-recorder"
BIN_NAME="quick-screen-recorder"
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

/usr/bin/python3 - "$ICONSET_DIR" << 'PY'
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

def render_icon(size):
    buf = bytearray(size * size * 4)
    cx = cy = (size - 1) * 0.5
    pad = size * 0.08
    half = size * 0.5 - pad
    corner = size * 0.22
    ring_r = size * 0.25
    dot_r = size * 0.14

    for y in range(size):
        for x in range(size):
            dx = abs(x - cx) - (half - corner)
            dy = abs(y - cy) - (half - corner)
            outside = math.hypot(max(dx, 0.0), max(dy, 0.0)) + min(max(dx, dy), 0.0) - corner
            if outside > 0.8:
                continue
            aa = max(0.0, min(1.0, 0.8 - outside))
            t = y / max(size - 1, 1)
            r = int(20 + 18 * (1.0 - t))
            g = int(24 + 22 * (1.0 - t))
            b = int(38 + 30 * (1.0 - t))

            dist = math.hypot(x - cx, y - cy)
            # Crisp white viewfinder ring
            ring_d = abs(dist - ring_r)
            if ring_d < size * 0.032:
                blend = max(0.0, min(1.0, (size * 0.032 - ring_d) / max(size * 0.012, 0.5)))
                r = int(r * (1 - blend) + 245 * blend)
                g = int(g * (1 - blend) + 248 * blend)
                b = int(b * (1 - blend) + 255 * blend)

            # Vivid red record core
            if dist < dot_r + 1.0:
                blend = max(0.0, min(1.0, dot_r + 0.6 - dist))
                r = int(r * (1 - blend) + 255 * blend)
                g = int(g * (1 - blend) + 64 * blend)
                b = int(b * (1 - blend) + 58 * blend)

            i = (y * size + x) * 4
            buf[i] = r
            buf[i + 1] = g
            buf[i + 2] = b
            buf[i + 3] = int(255 * aa)
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
  <string>Quick Screen Recorder captures your selected screen region or window locally on your Mac.</string>
  <key>NSMicrophoneUsageDescription</key>
  <string>Quick Screen Recorder records microphone audio locally only when Microphone capture is enabled.</string>
</dict>
</plist>
EOF

# Sign ad-hoc ("-") so Apple Silicon runs cleanly without embedding any personal certificate identity
codesign --force --deep --sign - "$APP_DIR"

echo "==> Built app bundle: $APP_DIR"
