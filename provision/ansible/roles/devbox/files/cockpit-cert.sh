#!/usr/bin/env bash
# Give Cockpit the host's tailnet certificate, so browsers and phones accept
# https://NAME.TAILNET.ts.net:9090 without a warning. Tailscale issues it (Let's
# Encrypt, 90 days); the timer runs this daily and it only changes anything
# when the certificate was renewed. Does nothing until the host is enrolled.
set -euo pipefail
name=$(tailscale status --json 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["Self"]["DNSName"].rstrip("."))' 2>/dev/null || true)
[[ -n $name ]] || { echo 'Not on a tailnet yet.'; exit 0; }
[[ -d /etc/cockpit ]] || exit 0
# Only Tailscale's own control server issues certificates; Headscale does not.
control=$(tailscale debug prefs 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin).get("ControlURL", ""))' 2>/dev/null || true)
if [[ -n $control && ! $control =~ ^https://[a-z0-9.-]*tailscale\.com/?$ ]]; then
  echo "$control issues no certificates; Cockpit keeps its own."
  exit 0
fi
target=/etc/cockpit/ws-certs.d
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# Needs HTTPS certificates enabled for the tailnet; tailscale says so if not.
timeout 120 tailscale cert --cert-file "$work/cert" --key-file "$work/key" "$name"
install -d -m 0755 "$target"
if ! cmp -s "$work/cert" "$target/50-tailnet.cert"; then
  install -m 0644 "$work/cert" "$target/50-tailnet.cert"
  install -m 0600 "$work/key" "$target/50-tailnet.key"
  systemctl try-restart cockpit.service
  echo "Cockpit now uses the certificate for $name."
fi
