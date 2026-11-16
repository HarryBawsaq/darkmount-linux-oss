#!/usr/bin/env bash
# Stops and removes the proton-presence service and binary.
set -euo pipefail

unit=proton-presence.service
unit_file="$HOME/.config/systemd/user/$unit"
binary="$HOME/.local/bin/proton-presence"
config_dir="$HOME/.config/proton-presence"

if systemctl --user show-environment >/dev/null 2>&1; then
    echo "==> Stopping and disabling $unit"
    systemctl --user disable --now "$unit" 2>/dev/null || true
fi

echo "==> Removing $unit_file"
rm -f "$unit_file"
echo "==> Removing $binary"
rm -f "$binary"

if systemctl --user show-environment >/dev/null 2>&1; then
    systemctl --user daemon-reload
    systemctl --user reset-failed "$unit" 2>/dev/null || true
fi

echo
echo "Uninstalled."
if [ -d "$config_dir" ]; then
    echo "Your config was kept. To delete it too: rm -r $config_dir"
fi
