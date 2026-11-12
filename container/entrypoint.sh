#!/usr/bin/env bash
set -euo pipefail
umask 077
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
pids=()
cleanup() { trap - EXIT TERM INT; for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done; wait || true; }
trap cleanup EXIT TERM INT
export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
dbus-daemon --session --nofork --address="$DBUS_SESSION_BUS_ADDRESS" &
pids+=("$!")
for ((attempt=0; attempt<100; attempt++)); do
  [[ -S "$XDG_RUNTIME_DIR/bus" ]] && break
  sleep 0.1
done
[[ -S "$XDG_RUNTIME_DIR/bus" ]] || { echo 'Session bus did not start' >&2; exit 1; }
# berthd in single-user mode: sessions, tasks and the channel for this account,
# reached from outside with `docker exec -i CONTAINER berthd connect`.
if command -v berthd >/dev/null && [[ -w /run/berth ]]; then
  berthd --single /run/berth &
  pids+=("$!")
fi
devbox-desktop-session &
pids+=("$!")
devbox-wait-display
websockify --web=/usr/share/novnc "$DEVBOX_BIND:6080" 127.0.0.1:5901 &
pids+=("$!")
if command -v t3 >/dev/null; then
  T3CODE_HOST="$DEVBOX_BIND" T3CODE_PORT=3773 t3 serve &
  pids+=("$!")
fi
wait -n "${pids[@]}"
exit 1
