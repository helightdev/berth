#!/usr/bin/env bash
# Checks and end-to-end tests in containers, the same here and in CI. Needs
# Docker or Podman with compose, and nothing else.
#
#   ci/run.sh check [CHECK…]    the checks of CI: fmt, shell-syntax, ansible-syntax,
#                               vale, python-tests, rust-tests, clippy
#   ci/run.sh e2e [TEST…]       build berth, start a test host and run the
#                               end-to-end tests against it (names as in
#                               unittest: e2e_serve, e2e_serve.Roles)
#   ci/run.sh all               check, then e2e
#   ci/run.sh boot [cloud|iso]  boot real installs in QEMU and test them; needs
#                               KVM and takes a while (see ci/boot.sh)
#   ci/run.sh shell             a shell in the dev container
#   ci/run.sh run COMMAND…      a command in the dev container
#   ci/run.sh build             build the images
#   ci/run.sh clean             remove the images and the cache volumes
#
# BERTH_CI_ENGINE    docker or podman (default: whichever is found, Docker first)
# BERTH_CI_CACHE     a directory for cargo's downloads and the target directory
#                    instead of volumes, for a CI cache to keep
# BERTH_CI_DOCKER    1 or 0: a Docker engine (docker:dind, privileged) on the test
#                    network for the Docker plugin's tests. Default: on with
#                    Docker when no TEST is named or one is e2e_provisioners;
#                    off with Podman, where the case is skipped
# BERTH_CI_DIND_IMAGES  images of this machine's engine to load into that
#                    engine, separated by spaces (it has no way out to pull)
# BERTH_CI_NO_BUILD  1: use the images as they are
# BERTH_CI_PROJECT   the compose project (default: berth-ci)
# BERTH_CI_INCREMENTAL  1 or 0: cargo's incremental builds (default: 1, and 0 in CI)
set -euo pipefail
cd "$(dirname "$0")/.."
root=$PWD

engine=${BERTH_CI_ENGINE:-}
if [[ -z $engine ]]; then
  if command -v docker >/dev/null; then engine=docker
  elif command -v podman >/dev/null; then engine=podman
  else echo "Docker or Podman is needed" >&2; exit 1
  fi
fi
export BERTH_CI_PROJECT=${BERTH_CI_PROJECT:-berth-ci}
# They make a build after a small change quicker, for a larger target
# directory. CI starts from a cache and builds once.
if [[ -n ${CI:-} ]]; then
  export BERTH_CI_INCREMENTAL=${BERTH_CI_INCREMENTAL:-0}
else
  export BERTH_CI_INCREMENTAL=${BERTH_CI_INCREMENTAL:-1}
fi
project=$BERTH_CI_PROJECT

# The dev container writes to the checkout as its owner. Rootless Podman maps
# root in the container to that user already.
if [[ $engine == podman && $(id -u) != 0 ]]; then
  export BERTH_CI_USER=0:0
else
  export BERTH_CI_USER="$(id -u):$(id -g)"
fi
if [[ -n ${BERTH_CI_CACHE:-} ]]; then
  mkdir -p "$BERTH_CI_CACHE/target" "$BERTH_CI_CACHE/cargo"
  BERTH_CI_CACHE=$(cd "$BERTH_CI_CACHE" && pwd)
  export BERTH_CI_TARGET=$BERTH_CI_CACHE/target BERTH_CI_CARGO=$BERTH_CI_CACHE/cargo
fi

# The target volume is mounted over target/: made here first, it belongs to
# the user rather than to the engine's root.
mkdir -p target build/ci

compose() { "$engine" compose -f ci/compose.yml "$@"; }
# A terminal only when there is one.
tty=()
[[ -t 0 && -t 1 ]] || tty=(-T)
dev() { compose --profile dev run --rm "${tty[@]}" dev "$@"; }

# The checks run the store's tests on PostgreSQL too, against the postgres
# service while it is up.
postgres_down() { compose --profile dev rm --stop --force postgres >/dev/null 2>&1 || true; }
postgres_up() {
  postgres_down
  trap postgres_down EXIT
  compose --profile dev up -d postgres >/dev/null
  local tries=0
  until compose --profile dev exec -T postgres pg_isready -q -h 127.0.0.1 -U berth -d berth; do
    if ((++tries > 60)); then
      echo "PostgreSQL for the tests did not start" >&2
      return 1
    fi
    sleep 1
  done
  export BERTH_CI_POSTGRES=postgres://berth:berth-ci@postgres:5432/berth
}
# Only rust-tests needs it: all the checks, or a list that names it.
checks() {
  if (($# == 0)) || [[ " $* " == *" rust-tests "* ]]; then
    postgres_up || return 1
  fi
  local status=0
  dev ci/checks.sh "$@" || status=$?
  postgres_down
  return "$status"
}

build() {
  [[ ${BERTH_CI_NO_BUILD:-} == 1 ]] && return
  compose --profile dev --profile e2e build --quiet "$@"
}

logs=$root/build/ci/logs
e2e_down() {
  compose --profile e2e --profile docker down --remove-orphans --timeout 5 >/dev/null 2>&1 || true
  "$engine" volume rm -f "${project}_seed" >/dev/null 2>&1 || true
}

# The engine for the Docker plugin's tests, when the run wants it: Docker
# only, since a privileged engine inside rootless Podman is not dependable.
# The test case skips when it is left out.
want_docker() {
  case ${BERTH_CI_DOCKER:-} in
    1) return 0 ;;
    0) return 1 ;;
  esac
  [[ $engine == docker ]] || return 1
  (($# == 0)) && return 0
  [[ " $* " == *" e2e_provisioners"* ]]
}
docker_up() {
  compose --profile e2e --profile docker up -d --wait docker >/dev/null || return 1
  local image
  for image in ${BERTH_CI_DIND_IMAGES:-}; do
    "$engine" save "$image" | compose --profile docker exec -T docker docker load >/dev/null || return 1
  done
  export BERTH_CI_DOCKER_HOST=tcp://docker:2375
}

e2e() {
  # The test host and the runner have no way out, so berth is built first.
  dev cargo build --locked -p berth --features web
  e2e_down
  trap e2e_down EXIT
  compose --profile e2e up -d studio dock
  if want_docker "$@"; then
    docker_up || { echo "The Docker engine for the tests did not start" >&2; return 1; }
  fi
  local status=0
  compose --profile e2e run --rm "${tty[@]}" runner python3 tests/e2e/run.py "$@" || status=$?
  rm -rf "$logs"
  mkdir -p "$logs"
  local host
  for host in studio dock; do
    mkdir -p "$logs/$host"
    compose --profile e2e logs --no-color "$host" >"$logs/$host/init.log" 2>&1 || true
    compose --profile e2e cp "$host:/var/log/berth-ci/." "$logs/$host/" >/dev/null 2>&1 || true
  done
  if ((status)); then
    echo "The end-to-end tests failed; the test hosts' logs are in ${logs#"$root"/}" >&2
  fi
  return "$status"
}

command=${1:-}
shift || true
case $command in
  build) build ;;
  check) build dev && checks "$@" ;;
  e2e) build && e2e "$@" ;;
  all) build && checks && e2e ;;
  shell) build dev && dev bash ;;
  run) build dev && dev "$@" ;;
  boot)
    # shellcheck source=ci/boot.sh
    source ci/boot.sh
    boot "$@"
    ;;
  clean)
    e2e_down
    compose --profile dev down --remove-orphans >/dev/null 2>&1 || true
    "$engine" volume rm -f berth-ci-target berth-ci-cargo >/dev/null
    "$engine" image rm -f berth-ci-dev berth-ci-host berth-ci-boot >/dev/null
    ;;
  *) sed -n '2,23p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2 ;;
esac
