#!/usr/bin/env bash
# RailSpotting receiver (Rust): install or update on Debian/Ubuntu. Run as root from the workspace
# after "cargo build --release -p railspotting-receiver --features speech":
#   sudo bash packaging/linux/install.sh
# Settings, identity, credentials, history and recordings in /opt/railspotting-receiver are kept,
# so running it again updates a receiver in place.
set -euo pipefail

APP=/opt/railspotting-receiver
USER_NAME=railspotting
SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
MODEL_URL=https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.en-q8_0.bin
[[ $EUID -eq 0 ]] || { echo "run as root" >&2; exit 1; }
BIN="$SRC/target/release/railspotting-receiver"
[[ -x "$BIN" ]] || { echo "no $BIN: build it first (cargo build --release -p railspotting-receiver --features speech)" >&2; exit 1; }

echo "== 1. Packages"
export DEBIAN_FRONTEND=noninteractive
LIBRTLSDR=librtlsdr0
apt-cache show librtlsdr0 >/dev/null 2>&1 || LIBRTLSDR=librtlsdr2
apt-get install -y -q rtl-sdr "$LIBRTLSDR" curl ca-certificates >/dev/null

echo "== 2. Service user"
id "$USER_NAME" >/dev/null 2>&1 || useradd --system --home-dir "$APP" --shell /usr/sbin/nologin "$USER_NAME"
for g in dialout plugdev; do getent group "$g" >/dev/null && usermod -aG "$g" "$USER_NAME"; done

echo "== 3. Program"
mkdir -p "$APP/bin" "$APP/state" "$APP/models"
install -m 0755 "$BIN" "$APP/bin/railspotting-receiver"
[[ -f "$APP/receiver.json" ]] || install -m 0644 "$SRC/receiver.json.example" "$APP/receiver.json"

echo "== 4. Speech model"
if ! ls "$APP"/models/ggml-small.en*.bin >/dev/null 2>&1; then
    curl -fL --retry 3 -o "$APP/models/ggml-small.en-q8_0.bin.part" "$MODEL_URL"
    mv "$APP/models/ggml-small.en-q8_0.bin.part" "$APP/models/ggml-small.en-q8_0.bin"
fi
chown -R "$USER_NAME:$USER_NAME" "$APP"
chmod 0750 "$APP"
[[ -f "$APP/credentials.json" ]] && chmod 0600 "$APP/credentials.json"

echo "== 5. Service"
install -m 0644 "$SRC/packaging/linux/railspotting-receiver.service" /etc/systemd/system/railspotting-receiver.service
systemctl daemon-reload
systemctl enable railspotting-receiver
systemctl restart railspotting-receiver
sleep 8
runuser -u "$USER_NAME" -- "$APP/bin/railspotting-receiver" --config "$APP/receiver.json" status || true
