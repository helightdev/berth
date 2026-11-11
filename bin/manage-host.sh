#!/usr/bin/env bash
# Maintenance tasks for an installed devbox. Runs on the host as root; the UI's
# "Manage an installed host" action sends it over SSH. By hand:
#   ssh ops@HOST 'sudo bash -s -- status' < bin/manage-host.sh
# Tasks: status, access, publish, pair, smoke, update, reboot.
set -euo pipefail
task=${1:-status}
agent=$(. /etc/devbox/environment 2>/dev/null; echo "${DEVBOX_USER:-agent}")
name=$(tailscale status --json 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["Self"]["DNSName"].rstrip("."))' 2>/dev/null || true)
address=$(tailscale ip -4 2>/dev/null | head -1 || true)

access() {
  if [[ -z $address ]]; then echo 'Not on a tailnet. Run: sudo tailscale up'; return; fi
  password=$(cat "/home/$agent/.config/tigervnc/password" 2>/dev/null || true)
  if tailscale serve status 2>/dev/null | grep -q 127.0.0.1:6080; then
    # After the #, so the password stays in the browser and reaches no server or log.
    encoded=$(python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1], safe=""))' "$password")
    echo "Desktop   https://$name/vnc.html${password:+#autoconnect=1&password=$encoded}"
    echo "T3        https://$name:8443/"
  else
    echo 'Desktop and T3 are not published on the tailnet yet (task: publish).'
  fi
  if systemctl is-active -q cockpit.socket; then
    echo "Cockpit   https://$name:9090  (or https://$address:9090; log in as the operator)"
  fi
  echo "SSH       ssh ${SUDO_USER:-OPERATOR}@$address"
  echo "Desktop (VNC) password: ${password:-not created yet}"
  [[ -n $password ]] && echo 'The Desktop link signs in by itself; treat it like the password.'
  echo "The $name names need Tailscale DNS on the machine you browse from; the address always works."
}

case $task in
  status)
    echo "Host        $(hostname), up since $(uptime -s)"
    echo "Provision   $(cat /var/lib/devbox/provision-state 2>/dev/null || echo unknown)"
    echo "Tailscale   ${address:-not enrolled} ${name}"
    for unit in ssh tailscaled cockpit.socket docker lightdm; do
      systemctl cat "$unit" >/dev/null 2>&1 && printf '%-11s %s\n' "$unit" "$(systemctl is-active "$unit" || true)"
    done
    for port in 6080:desktop 3773:T3; do
      code=$(curl -s -o /dev/null -m 5 -w '%{http_code}' "http://127.0.0.1:${port%%:*}/" || true)
      printf '%-11s %s\n' "${port##*:}" "$([[ $code == 200 ]] && echo running || echo "not answering ($code)")"
    done
    echo "Graphics    $(sudo -iu "$agent" agent-env glxinfo -B 2>/dev/null | sed -n 's/^OpenGL renderer string: //p' || true)"
    echo "Disk        $(df -h --output=avail,pcent / | tail -1 | xargs) free/used on /"
    [[ -f /var/run/reboot-required ]] && echo 'Reboot      required by installed updates'
    systemctl --failed --no-legend | sed 's/^/Failed      /'
    echo; access ;;
  access) access ;;
  publish)
    [[ -n $address ]] || { echo 'Not on a tailnet. Run: sudo tailscale up' >&2; exit 1; }
    # Needs HTTPS certificates enabled for the tailnet; tailscale says so if not.
    timeout 60 tailscale serve --bg --https=443 http://127.0.0.1:6080
    timeout 60 tailscale serve --bg --https=8443 http://127.0.0.1:3773
    # Cockpit gets the same trusted certificate instead of its self-signed one.
    [[ -x /usr/local/sbin/devbox-cockpit-cert ]] && /usr/local/sbin/devbox-cockpit-cert || true
    echo; access ;;
  pair)
    # T3 asks every new browser to pair once. Its own pairing link points at
    # this machine only, so the token goes into the published tailnet address.
    [[ -n $name ]] || { echo 'Not on a tailnet. Run: sudo tailscale up' >&2; exit 1; }
    tailscale serve status 2>/dev/null | grep -q 127.0.0.1:3773 || { echo 'T3 is not published on the tailnet yet (task: publish).' >&2; exit 1; }
    token=$(sudo -iu "$agent" agent-env t3 pair --ttl 10m --label "berth $(date +%F)" | sed -n 's/^Token: *//p')
    [[ -n $token ]] || { echo 'T3 gave no pairing token. Is it running? (task: status)' >&2; exit 1; }
    echo "Browser        https://$name:8443/pair#token=$token"
    echo "App: host      https://$name:8443"
    echo "App: code      $token"
    echo 'Open the link in a browser, or enter host and code in the T3 app. The code works once, within 10 minutes.'
    if command -v qrencode >/dev/null; then echo; qrencode -t UTF8 -m 1 "https://$name:8443/pair#token=$token"; fi ;;
  smoke) sudo -iu "$agent" agent-env devbox-smoke ;;
  update)
    export DEBIAN_FRONTEND=noninteractive
    apt-get update
    apt-get -y -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold dist-upgrade
    apt-get -y autoremove
    [[ -f /var/run/reboot-required ]] && echo 'A reboot is required to finish (task: reboot).' || echo 'Up to date.' ;;
  reboot)
    echo 'Rebooting.'
    systemd-run --quiet --on-active=2 systemctl reboot ;;
  *) echo "Unknown task: $task (status, access, publish, smoke, update, reboot)" >&2; exit 2 ;;
esac
