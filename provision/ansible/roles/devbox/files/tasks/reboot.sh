#!/usr/bin/env bash
set -euo pipefail
echo 'Rebooting.'
systemd-run --quiet --on-active=2 systemctl reboot
