#!/usr/bin/env bash
. "$(dirname "$0")/common.sh"
echo "Host        $(hostname), up since $(uptime -s)"
echo "Provision   $(cat /var/lib/devbox/provision-state 2>/dev/null || echo unknown)"
echo "Tailscale   ${address:-not enrolled} ${name}"
for unit in ssh tailscaled berthd cockpit.socket docker lightdm; do
  systemctl cat "$unit" >/dev/null 2>&1 && printf '%-11s %s\n' "$unit" "$(systemctl is-active "$unit" || true)"
done
for port in 6080:desktop 3773:T3; do
  code=$(curl -s -o /dev/null -m 5 -w '%{http_code}' "http://127.0.0.1:${port%%:*}/" || true)
  printf '%-11s %s\n' "${port##*:}" "$([[ $code == 200 ]] && echo running || echo "not answering ($code)")"
done
echo "Graphics    $(as_agent glxinfo -B 2>/dev/null | sed -n 's/^OpenGL renderer string: //p' || true)"
echo "Disk        $(df -h --output=avail,pcent / | tail -1 | xargs) free/used on /"
[[ -f /var/run/reboot-required ]] && echo 'Reboot      required by installed updates'
# Without systemd, as in a container, there are no failed units to list.
{ systemctl --failed --no-legend 2>/dev/null || true; } | sed 's/^/Failed      /'
echo
access
