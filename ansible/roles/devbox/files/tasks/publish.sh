#!/usr/bin/env bash
. "$(dirname "$0")/common.sh"
[[ -n $address ]] || { echo 'Not on a tailnet. Run: sudo tailscale up' >&2; exit 1; }
# Needs HTTPS certificates enabled for the tailnet; tailscale says so if not.
timeout 60 tailscale serve --bg --https=443 http://127.0.0.1:6080
timeout 60 tailscale serve --bg --https=8443 http://127.0.0.1:3773
# Cockpit gets the same trusted certificate instead of its self-signed one.
[[ -x /usr/local/sbin/devbox-cockpit-cert ]] && /usr/local/sbin/devbox-cockpit-cert || true
echo
access
