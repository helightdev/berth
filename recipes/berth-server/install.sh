#!/usr/bin/env bash
# Run the recipe on this machine: install Ansible from Ubuntu's packages and
# apply site.yml to the machine itself. Arguments go to ansible-playbook:
#
#   sudo ./install.sh
#   sudo ./install.sh -e tailscale_auth_key=tskey-auth-… -e @my-vars.yml
#   sudo ./install.sh --check --diff      what a run would change
set -euo pipefail
cd "$(dirname "$0")"

if [[ $EUID -ne 0 ]]; then
  echo 'Run it as root: sudo ./install.sh' >&2
  exit 1
fi
if ! command -v ansible-playbook >/dev/null; then
  export DEBIAN_FRONTEND=noninteractive
  apt-get update
  apt-get install -y ansible-core
fi
exec ansible-playbook --connection local --inventory localhost, site.yml "$@"
