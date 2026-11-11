#!/usr/bin/env bash
set -euo pipefail
for ((attempt=0; attempt<100; attempt++)); do
  if xdpyinfo -display "${DISPLAY:-:1}" >/dev/null 2>&1; then exit 0; fi
  sleep 0.2
done
echo 'Agent display did not become available' >&2
exit 1
