#!/usr/bin/env bash
# The test host's init. It runs sshd, takes the operator's key from /seed as
# a cloud provider would hand it over, and does for berthd what systemd does
# on a real host: start it once the role has installed it, restart it when its
# program changes and have it read its files again when they change.
set -uo pipefail
logs=/var/log/berth-ci
ssh-keygen -A >/dev/null
/usr/sbin/sshd -D -e 2>>"$logs/sshd.log" &
sshd=$!

berthd=
program=
files=
stop() {
  [[ -n $berthd ]] && kill "$berthd" 2>/dev/null
  kill "$sshd" 2>/dev/null
  exit 0
}
trap stop TERM INT

while kill -0 "$sshd" 2>/dev/null; do
  for key in /seed/*.pub; do
    [[ -f $key ]] || continue
    user=$(basename "$key" .pub)
    if ! cmp -s "$key" "/etc/ssh/provided-keys/$user"; then
      install -m 0644 "$key" "/etc/ssh/provided-keys/$user"
    fi
  done
  if [[ -x /usr/local/bin/berthd && -f /etc/berth/berthd.json ]]; then
    if [[ -n $berthd ]] && ! kill -0 "$berthd" 2>/dev/null; then
      echo "berth-ci: berthd ended" >>"$logs/berthd.log"
      berthd=
    fi
    now=$(stat -c '%Y %s' /usr/local/bin/berthd)
    read_now=$(cat /etc/berth/*.json | md5sum)
    if [[ -z $berthd || $now != "$program" ]]; then
      if [[ -n $berthd ]]; then
        kill "$berthd" 2>/dev/null
        wait "$berthd" 2>/dev/null
      fi
      echo "berth-ci: starting berthd" >>"$logs/berthd.log"
      /usr/local/bin/berthd >>"$logs/berthd.log" 2>&1 &
      berthd=$!
      program=$now
      files=$read_now
    elif [[ $read_now != "$files" ]]; then
      echo "berth-ci: reloading berthd" >>"$logs/berthd.log"
      kill -HUP "$berthd"
      files=$read_now
    fi
  fi
  sleep 0.5
done
echo "berth-ci: sshd ended" >&2
exit 1
