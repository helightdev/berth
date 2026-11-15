#!/usr/bin/env bash
# The checks of CI, run inside the dev container by ci/run.sh, or as they
# are on a machine with the tools, as CI does. Given names, only those run:
# ci/run.sh check clippy vale
set -euo pipefail
cd "$(dirname "$0")/.."

crates=(-p berth-api -p berth-pty -p berth-core -p berth-tui -p berth -p berthd -p box -p berth-page)

# Each runs as the condition of an if, where set -e does not stop it: the
# steps are joined with && instead.
# nextest runs the test programs side by side, which pays on a machine with
# many cores. On CI's four it is slower than cargo test.
rust-tests() {
  if cargo nextest --version >/dev/null 2>&1; then
    cargo nextest run --locked --no-fail-fast
  else
    cargo test --locked
  fi
}
# Each file in a process of its own, side by side, its output when it is done.
python-tests() {
  local file out=build/python-tests pids=() bad=0
  rm -rf "$out" && mkdir -p "$out"
  for file in tests/test_*.py; do
    file=$(basename "$file")
    python3 -m unittest discover -s tests -p "$file" -v >"$out/$file.log" 2>&1 &
    pids+=("$!:$file")
  done
  for entry in "${pids[@]}"; do
    if wait "${entry%%:*}"; then
      grep -E '^(Ran|OK)' "$out/${entry#*:}.log" | paste -sd ' ' | sed "s|^|${entry#*:}: |"
    else
      cat "$out/${entry#*:}.log"
      echo "${entry#*:} failed" >&2
      bad=1
    fi
  done
  return "$bad"
}
shell-syntax() {
  local script bad=0
  for script in install.sh provision/bin/*.sh provision/bootstrap/*.sh container/*.sh ci/*.sh \
    provision/ansible/roles/devbox/files/*.sh provision/ansible/roles/devbox/files/tasks/*.sh plugins/*/files/*.sh \
    recipes/*/*.sh; do
    bash -n "$script" || bad=1
  done
  return "$bad"
}
ansible-syntax() {
  (cd provision && ansible-playbook -i localhost, ansible/site.yml --syntax-check) &&
    (cd provision && ansible-playbook -i localhost, ansible/controller.yml --syntax-check) &&
    (cd recipes/berth-server && ansible-playbook -i localhost, site.yml --syntax-check)
}
fmt() { cargo fmt --check "${crates[@]}"; }
clippy() { cargo clippy --locked --all-targets -- -D warnings; }
vale() { command vale --minAlertLevel=warning README.md AGENTS.md docs/content/docs plugins/*/guide.md; }

all=(fmt shell-syntax ansible-syntax vale python-tests rust-tests clippy)
if (($#)); then steps=("$@"); else steps=("${all[@]}"); fi
for step in "${steps[@]}"; do
  if [[ " ${all[*]} " != *" $step "* ]]; then
    echo "No check $step. The checks: ${all[*]}" >&2
    exit 2
  fi
done

# GitHub folds the output of each check.
group() { [[ -n ${GITHUB_ACTIONS:-} ]] && echo "::group::$1"; true; }
endgroup() { [[ -n ${GITHUB_ACTIONS:-} ]] && echo "::endgroup::"; true; }

failed=()
for step in "${steps[@]}"; do
  group "$step"
  start=$SECONDS
  if "$step"; then
    endgroup
    echo "== $step passed ($((SECONDS - start))s)"
  else
    endgroup
    echo "== $step failed ($((SECONDS - start))s)"
    failed+=("$step")
  fi
done
if ((${#failed[@]})); then
  echo "Failed: ${failed[*]}" >&2
  exit 1
fi
