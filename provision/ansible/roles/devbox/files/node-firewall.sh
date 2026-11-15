#!/usr/bin/env bash
# The firewall of a node: incoming traffic is refused except what the rules on
# standard input allow, one line of ufw arguments each, such as
# `allow in on incusbr0`. berth's rules carry the comment berth: one that is no
# longer asked for is deleted, and rules added by hand stay. Rules are added
# before the firewall is switched on, so SSH is never closed in between. It
# says on standard output what it changed, and nothing when nothing did.
set -euo pipefail
ufw=${DEVBOX_UFW:-ufw}
mark=berth

mapfile -t wanted
rules=()
for rule in "${wanted[@]}"; do
  rule=$(xargs <<<"$rule")
  [[ -n $rule ]] && rules+=("$rule")
done

# `ufw show added` lists each rule as the command that added it, with its comment.
added=$("$ufw" show added 2>/dev/null || true)
has() { grep -qxF -- "ufw $1 comment '$mark'" <<<"$added"; }

for rule in "${rules[@]}"; do
  if ! has "$rule"; then
    # shellcheck disable=SC2086 # a rule is words for ufw
    "$ufw" $rule comment "$mark" >/dev/null
    echo "added: $rule"
  fi
done

while IFS= read -r line; do
  [[ $line == "ufw "*" comment '$mark'" ]] || continue
  rule=${line#ufw }
  rule=${rule% comment \'$mark\'}
  keep=0
  for wanted_rule in "${rules[@]}"; do
    [[ $wanted_rule == "$rule" ]] && keep=1
  done
  if [[ $keep == 0 ]]; then
    # A routed rule is deleted as `ufw route delete RULE`.
    # shellcheck disable=SC2086 # a rule is words for ufw
    case $rule in
      "route "*) "$ufw" route delete ${rule#route } >/dev/null ;;
      *) "$ufw" delete $rule >/dev/null ;;
    esac
    echo "removed: $rule"
  fi
done <<<"$added"

# Should ufw have written a rule otherwise than it was given, deleting the old
# form took the rule away: it goes back in, so what is asked for always stays.
added=$("$ufw" show added 2>/dev/null || true)
for rule in "${rules[@]}"; do
  if ! has "$rule"; then
    # shellcheck disable=SC2086 # a rule is words for ufw
    "$ufw" $rule comment "$mark" >/dev/null
  fi
done

status=$("$ufw" status 2>/dev/null || true)
if ! grep -q '^Status: active' <<<"$status"; then
  "$ufw" default deny incoming >/dev/null
  "$ufw" default allow outgoing >/dev/null
  "$ufw" --force enable >/dev/null
  echo 'switched the firewall on: incoming traffic is refused except the rules above'
elif ! "$ufw" status verbose 2>/dev/null | grep -q 'deny (incoming)'; then
  "$ufw" default deny incoming >/dev/null
  echo 'refused incoming traffic by default'
fi
