# Shared by the scripts of berth's own actions. Sourced, not run. They run on
# the host as root, started by berthd; `berth run HOST ACTION` or the overview
# runs them.
set -euo pipefail
agent=$(. /etc/devbox/environment 2>/dev/null; echo "${DEVBOX_USER:-agent}")
operator=$(jq -r '.operator // empty' /etc/berth/berthd.json 2>/dev/null || true)
name=$(tailscale status --json 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["Self"]["DNSName"].rstrip("."))' 2>/dev/null || true)
address=$(tailscale ip -4 2>/dev/null | head -1 || true)
# The control server the host is on. Tailscale's own offers tailscale serve and
# HTTPS certificates; Headscale offers neither.
control=$(tailscale debug prefs 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin).get("ControlURL", ""))' 2>/dev/null || true)
own_control() { [[ -n $control && ! $control =~ ^https://[a-z0-9.-]*tailscale\.com/?$ ]]; }

as_agent() { sudo -iu "$agent" agent-env "$@"; }

# What a host off the tailnet says where a tailnet address would be.
off_tailnet='Not on a tailnet: berth reaches this host over SSH at its address. With the secret tailnet_api_key set, applying it from berth joins it.'

tunnels() {
  echo "Desktop   ssh -L 6080:127.0.0.1:6080 ${operator:-OPERATOR}@$1, then http://127.0.0.1:6080/vnc.html"
  echo "T3        ssh -L 3773:127.0.0.1:3773 ${operator:-OPERATOR}@$1, then http://127.0.0.1:3773/"
}

access() {
  password=$(cat "/home/$agent/.config/tigervnc/password" 2>/dev/null || true)
  if [[ -z $address ]]; then
    echo "$off_tailnet Through an SSH tunnel, with the address berth uses:"
    tunnels ADDRESS
    echo "SSH       ssh ${operator:-OPERATOR}@ADDRESS"
    echo "Desktop (VNC) password: ${password:-not created yet}"
    return
  fi
  if own_control; then
    echo "The tailnet's control server, $control, does not offer tailscale serve. Through an SSH tunnel:"
    tunnels "$address"
  elif tailscale serve status 2>/dev/null | grep -q 127.0.0.1:6080; then
    # After the #, so the password stays in the browser and reaches no server or log.
    encoded=$(python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1], safe=""))' "$password")
    echo "Desktop   https://$name/vnc.html${password:+#autoconnect=1&password=$encoded}"
    echo "T3        https://$name:8443/"
  else
    echo 'Desktop and T3 are not published on the tailnet yet (action: publish).'
  fi
  if systemctl is-active -q cockpit.socket && own_control; then
    echo "Cockpit   https://$address:9090  (its own certificate; log in as the operator)"
  elif systemctl is-active -q cockpit.socket; then
    echo "Cockpit   https://$name:9090  (or https://$address:9090; log in as the operator)"
  fi
  echo "SSH       ssh ${operator:-OPERATOR}@$address"
  echo "Desktop (VNC) password: ${password:-not created yet}"
  [[ -n $password ]] && echo 'The Desktop link signs in by itself; treat it like the password.'
  echo "The $name names need the tailnet's DNS (MagicDNS) on the machine you browse from; the address always works."
}
