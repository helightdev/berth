#!/usr/bin/env bash
set -euo pipefail
umask 077
devbox-init-vnc
mkdir -p "$XDG_RUNTIME_DIR"
touch "$XAUTHORITY"
xauth -f "$XAUTHORITY" add "$DISPLAY" . "$(python3 -c 'import secrets; print(secrets.token_hex(16))')"
pids=()
cleanup() { trap - EXIT TERM INT; for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done; wait || true; }
trap cleanup EXIT TERM INT
# With CPU rendering switched off, let the virtual display render on the host's
# GPU (DRI3). Without a usable GPU it stays an unaccelerated display.
gpu=()
if [[ ${DEVBOX_SOFTWARE_RENDERING:-0} != 1 ]]; then
  for node in /dev/dri/renderD*; do
    if [[ -r $node && -w $node ]]; then gpu=(-rendernode "$node"); break; fi
  done
fi
Xtigervnc "$DISPLAY" -geometry "$DEVBOX_GEOMETRY" -depth 24 -dpi 96 "${gpu[@]}" \
  -localhost=1 -rfbport 5901 -SecurityTypes VncAuth \
  -PasswordFile "$HOME/.config/tigervnc/passwd" -auth "$XAUTHORITY" \
  -AlwaysShared -AcceptSetDesktopSize=0 -MaxDisconnectionTime=0 -MaxIdleTime=0 \
  -nolisten tcp &
pids+=("$!")
devbox-wait-display
xset s off
xset -dpms || true
dbus-update-activation-environment DISPLAY XAUTHORITY XDG_RUNTIME_DIR
if [[ ${DEVBOX_SOFTWARE_RENDERING:-0} == 1 ]]; then
  dbus-update-activation-environment LIBGL_ALWAYS_SOFTWARE GALLIUM_DRIVER __GLX_VENDOR_LIBRARY_NAME VK_DRIVER_FILES
fi
xfconf-query -c xfwm4 -p /general/use_compositing -n -t bool -s false
xfconf-query -c xfce4-session -p /general/SaveOnExit -n -t bool -s false
xfce4-session &
pids+=("$!")
wait -n "${pids[@]}"
# Exit unexpectedly if either the display or session terminates; systemd restarts it.
exit 1
