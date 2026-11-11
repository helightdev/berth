#!/usr/bin/env bash
# Keep the built-in display off while the laptop lid is closed. The host keeps
# running and the agent's virtual desktop keeps its GPU; only the panel sleeps.
#   devbox-lid          watch the lid (the service)
#   devbox-lid off|on   switch the display now
set -u
switch() {
  local power=0
  [[ $1 == off ]] && power=4
  for backlight in /sys/class/backlight/*/bl_power; do
    [[ -w $backlight ]] && echo "$power" > "$backlight"
  done
  # The login screen or a physical session, through the X server's own authority.
  for authority in /var/run/lightdm/root/:*; do
    [[ -e $authority ]] || continue
    XAUTHORITY=$authority xset -display "${authority##*/}" dpms force "$1" 2>/dev/null || true
  done
}
case ${1:-} in
  on|off) switch "$1"; exit ;;
esac
last=
while sleep 2; do
  state=open
  grep -qs closed /proc/acpi/button/lid/*/state && state=closed
  [[ $state == "$last" ]] && continue
  last=$state
  if [[ $state == closed ]]; then switch off; else switch on; fi
done
