#!/usr/bin/env bash
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get -y -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold dist-upgrade
apt-get -y autoremove
[[ -f /var/run/reboot-required ]] && echo 'A reboot is required to finish (task: reboot).' || echo 'Up to date.'
