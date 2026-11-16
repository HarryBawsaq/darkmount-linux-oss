#!/usr/bin/env bash
# Builds proton-presence and installs it as a systemd user service.
set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")"

unit=proton-presence.service
bin_dir="$HOME/.local/bin"
unit_dir="$HOME/.config/systemd/user"
config_dir="$HOME/.config/proton-presence"

if [ "$(id -u)" -eq 0 ]; then
    echo "Run this as your normal user, not with sudo: it installs a user service." >&2
    exit 1
fi
if ! command -v cargo >/dev/null; then
    echo "cargo not found. Install Rust first: sudo pacman -S --needed rust" >&2
    exit 1
fi
if ! systemctl --user show-environment >/dev/null 2>&1; then
    echo "Cannot reach the systemd user manager (systemctl --user)." >&2
    echo "Run this from your normal desktop login session." >&2
    exit 1
fi

echo "==> Building (release)"
cargo build --release --locked

echo "==> Installing $bin_dir/proton-presence"
install -Dm755 target/release/proton-presence "$bin_dir/proton-presence"

echo "==> Installing $unit_dir/$unit"
install -Dm644 "$unit" "$unit_dir/$unit"

if [ -e "$config_dir/config.toml" ]; then
    echo "==> Keeping existing $config_dir/config.toml"
else
    echo "==> Creating $config_dir/config.toml"
    install -Dm644 config.example.toml "$config_dir/config.toml"
fi

echo "==> Enabling and starting the service"
systemctl --user daemon-reload
systemctl --user enable "$unit"
# restart rather than start, so a reinstall picks up the new binary
systemctl --user restart "$unit"

echo
"$bin_dir/proton-presence" status
echo
echo "Installed. The service starts at every login."
echo "Next: put your Discord application ID in $config_dir/config.toml"
echo "Logs: journalctl --user -u proton-presence -f"
