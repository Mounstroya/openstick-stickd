#!/bin/sh
# Regenerates the README screenshots with simulated data (nothing touches the stick).
# Needs: cargo, Chromium, node with puppeteer-core (npm i puppeteer-core), and optionally
# pngquant/oxipng/optipng or python3-pil to shrink the PNGs.
#   docs/screenshots/capture.sh
set -eu
cd "$(dirname "$0")/../.."
OUT=docs/screenshots
PORT=${CAPTURAS_PUERTO:-8740}
# A whole-hour time zone where it is now late afternoon, so the chat/mailbox times and the
# 24 h foot-traffic chart look like a busy day (Etc/GMT-N means UTC+N).
H=$(date -u +%H | sed 's/^0//')
OFF=$(( (18 - H + 24) % 24 )); [ "$OFF" -gt 12 ] && OFF=$((OFF - 24))
if [ "$OFF" -ge 0 ]; then TZ="Etc/GMT-$OFF"; else TZ="Etc/GMT+$((-OFF))"; fi
export TZ
DATA=$(mktemp -d)
CAPTURAS_PUERTO=$PORT CAPTURAS_DIR=$DATA cargo test --quiet capturas_servidor -- --ignored --nocapture &
SRV=$!
trap 'kill $SRV 2>/dev/null; pkill -f "stickd-.*capturas_servidor" 2>/dev/null; rm -rf "$DATA"' EXIT
for _ in $(seq 1 120); do curl -sf -o /dev/null "http://127.0.0.1:$PORT/" && break; sleep 1; done
node "$OUT/capture.js" "$OUT" "$PORT"
for f in "$OUT"/*.png; do
  if command -v pngquant >/dev/null; then pngquant --force --skip-if-larger --quality 70-95 --output "$f" "$f" || true
  elif python3 -c 'import PIL' 2>/dev/null; then
    python3 -c 'import sys; from PIL import Image; i=Image.open(sys.argv[1]).convert("RGB"); i.quantize(256, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.NONE).save(sys.argv[1], optimize=True)' "$f"
  fi
  if command -v oxipng >/dev/null; then oxipng -q -o 4 "$f"; elif command -v optipng >/dev/null; then optipng -quiet -o2 "$f"; fi
done
du -ch "$OUT"/*.png | tail -1
