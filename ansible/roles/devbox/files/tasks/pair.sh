#!/usr/bin/env bash
# T3 asks every new browser to pair once. Its own pairing link points at this
# machine only, so the token goes into the published tailnet address.
. "$(dirname "$0")/common.sh"
[[ -n $name ]] || { echo 'Not on a tailnet. Run: sudo tailscale up' >&2; exit 1; }
tailscale serve status 2>/dev/null | grep -q 127.0.0.1:3773 || { echo 'T3 is not published on the tailnet yet (task: publish).' >&2; exit 1; }
token=$(as_agent t3 pair --ttl 10m --label "berth $(date +%F)" | sed -n 's/^Token: *//p')
[[ -n $token ]] || { echo 'T3 gave no pairing token. Is it running? (task: status)' >&2; exit 1; }
echo "Browser        https://$name:8443/pair#token=$token"
echo "App: host      https://$name:8443"
echo "App: code      $token"
echo 'Open the link in a browser, or enter host and code in the T3 app. The code works once, within 10 minutes.'
if command -v qrencode >/dev/null; then echo; qrencode -t UTF8 -m 1 "https://$name:8443/pair#token=$token"; fi
