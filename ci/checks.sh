#!/usr/bin/env bash
# The checks of CI, run inside the dev container by ci/run.sh. Given
# names, only those run: ci/run.sh check clippy vale
set -euo pipefail
cd /src

crates=(-p berth-api -p berth-pty -p berth-core -p berth-tui -p berth -p berthd -p box -p berth-web)

# Each runs as the condition of an if, where set -e does not stop it: the
# steps are joined with && instead.
rust-tests() {
  cargo test --locked &&
    cargo test --locked -p berth --features web
}
python-tests() { python3 -m unittest discover -s tests -v; }
shell-syntax() {
  local script bad=0
  for script in install.sh bin/*.sh bootstrap/*.sh container/*.sh ci/*.sh ansible/roles/devbox/files/*.sh \
    ansible/roles/devbox/files/tasks/*.sh plugins/*/files/*.sh; do
    bash -n "$script" || bad=1
  done
  return "$bad"
}
ansible-syntax() { ansible-playbook -i localhost, ansible/site.yml --syntax-check; }
fmt() { cargo fmt --check "${crates[@]}"; }
clippy() {
  cargo clippy --locked --all-targets -- -D warnings &&
    cargo clippy --locked --all-targets -p berth --features web -- -D warnings &&
    cargo clippy --locked -p berth-web --target wasm32-unknown-unknown -- -D warnings
}
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
