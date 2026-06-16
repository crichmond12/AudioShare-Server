#!/bin/bash

set -e

LOG_DEVICE="audioshare_device.log"
LOG_SITE="audioshare_site.log"

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

echo "[build] Building device server (cargo)..."
cargo build --release --manifest-path "$SCRIPT_DIR/Cargo.toml"
cp "$SCRIPT_DIR/target/release/audio_share" "$SCRIPT_DIR/audioshare_device"

echo "[build] Building site server (go)..."
go build -C "$SCRIPT_DIR/site" -o "$SCRIPT_DIR/audioshare_site" .

echo "[build] Done. Starting servers..."
set +e

cleanup() {
    echo ""
    echo "Shutting down..."
    kill "$PID_DEVICE" "$PID_SITE" 2>/dev/null
    wait "$PID_DEVICE" "$PID_SITE" 2>/dev/null
    exit 0
}
trap cleanup INT TERM

"$SCRIPT_DIR/audioshare_device" 2>&1 | tee -a "$LOG_DEVICE" | sed 's/^/[device] /' &
PID_DEVICE=$!

"$SCRIPT_DIR/audioshare_site" 2>&1 | tee -a "$LOG_SITE" | sed 's/^/[site] /' &
PID_SITE=$!

wait "$PID_DEVICE" "$PID_SITE"
