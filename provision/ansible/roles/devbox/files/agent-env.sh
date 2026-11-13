#!/usr/bin/env bash
set -euo pipefail
source /etc/devbox/environment
export DBUS_SESSION_BUS_ADDRESS=${DBUS_SESSION_BUS_ADDRESS:-unix:path=$XDG_RUNTIME_DIR/bus}
if [[ $# == 0 ]]; then exec /bin/bash; fi
exec "$@"
