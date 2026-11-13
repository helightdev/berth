#!/usr/bin/env bash
set -euo pipefail
# Preserve a route before enabling UFW. Docker ports must still be bound to loopback.
ufw default deny incoming >/dev/null
ufw default allow outgoing >/dev/null
for port in 22 443 8443; do ufw allow in on tailscale0 to any port "$port" proto tcp >/dev/null; done
if [[ ${DEVBOX_COCKPIT:-1} == 1 ]]; then
  ufw allow in on tailscale0 to any port 9090 proto tcp >/dev/null
else
  ufw delete allow in on tailscale0 to any port 9090 proto tcp >/dev/null 2>&1 || true
fi
ufw allow 41641/udp >/dev/null
cidr=${DEVBOX_BOOTSTRAP_SSH_CIDR:-}
if [[ -z "$cidr" && -n ${DEVBOX_SSH_CONNECTION:-} ]]; then
  read -r cidr _ <<< "$DEVBOX_SSH_CONNECTION"
fi
if [[ -n "$cidr" ]]; then
  python3 -c 'import ipaddress,sys; ipaddress.ip_network(sys.argv[1], strict=False)' "$cidr"
  ufw allow from "$cidr" to any port 22 proto tcp >/dev/null
elif [[ ${DEVBOX_PROFILE:-} == cloud ]]; then
  # No network was named for a host that is only reachable from outside:
  # SSH stays open to any address rather than locking the owner out.
  ufw allow 22/tcp >/dev/null
fi
ufw --force enable >/dev/null
