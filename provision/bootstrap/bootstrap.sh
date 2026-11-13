#!/usr/bin/env bash
set -euo pipefail
umask 077
base=/opt/devbox-bootstrap
state=/var/lib/devbox
# Progress is meant to be readable without sudo; secrets live elsewhere.
install -d -m 0755 "$state"
if [[ ${1:-} == --stage-only ]]; then
  install -m 0755 "$base/bootstrap.sh" /usr/local/sbin/devbox-provision
  cat > /etc/systemd/system/devbox-provision.service <<'UNIT'
[Unit]
Description=Provision the devbox from its verified release
After=network-online.target
Wants=network-online.target
[Service]
Type=oneshot
ExecStart=/usr/local/sbin/devbox-provision --run
TimeoutStartSec=2h
[Install]
WantedBy=multi-user.target
UNIT
  cat > /etc/systemd/system/devbox-provision.timer <<'UNIT'
[Unit]
Description=Retry incomplete devbox provisioning
[Timer]
OnBootSec=2min
OnUnitInactiveSec=10min
RandomizedDelaySec=30
[Install]
WantedBy=timers.target
UNIT
  systemctl enable devbox-provision.service devbox-provision.timer
  exit 0
fi
[[ ${1:-} == --run ]] || { echo 'Usage: bootstrap.sh --stage-only|--run' >&2; exit 2; }
exec 9>/run/devbox-provision.lock
flock -n 9 || exit 0
digest=$(python3 -c 'import json; print(json.load(open("/opt/devbox-bootstrap/deployment.json"))["release_sha256"])')
[[ "$digest" =~ ^[a-f0-9]{64}$ ]] || exit 2
if [[ -f "$state/ready" && $(cat "$state/ready") == "$digest" ]]; then exit 0; fi
set_state() { printf '%s\n' "$1" > "$state/provision-state"; chmod 0644 "$state/provision-state"; }
# berth relays the lines that start with ==> while it waits for the machine.
step() { printf '==> %s\n' "$*"; }
trap 'set_state failed' ERR
set_state provisioning
export DEBIAN_FRONTEND=noninteractive
step 'Installing the packages the first boot needs'
apt-get -o DPkg::Lock::Timeout=300 update
apt-get -o DPkg::Lock::Timeout=300 install -y python3-venv python3-pip curl ca-certificates
url=$(python3 -c 'import json; print(json.load(open("/opt/devbox-bootstrap/deployment.json")).get("release_url", ""))')
release_ready() { [[ -f "$base/release.tar.gz" ]] && printf '%s  %s\n' "$digest" "$base/release.tar.gz" | sha256sum --check --status; }
if [[ -z "$url" ]]; then
  step 'Waiting for the release to be copied in'
  # Without a URL the release is copied in by what made the machine, possibly
  # only after boot; the digest tells a finished copy from one still being written.
  release_ready || echo "Waiting up to 15 minutes for $base/release.tar.gz" >&2
  for _ in $(seq 180); do
    release_ready && break
    sleep 5
  done
elif [[ ! -f "$base/release.tar.gz" ]]; then
  [[ "$url" == https://* ]] || { echo 'Expected an HTTPS release URL' >&2; exit 1; }
  step 'Downloading the release'
  # A release that is not public comes with a login for curl, used once.
  login=()
  if [[ -f /opt/devbox-bootstrap/release-login ]]; then login=(--config /opt/devbox-bootstrap/release-login); fi
  curl --proto '=https' --tlsv1.2 --fail --location --retry 5 "${login[@]}" --output "$base/release.tar.gz.tmp" "$url"
  mv "$base/release.tar.gz.tmp" "$base/release.tar.gz"
fi
rm -f /opt/devbox-bootstrap/release-login
printf '%s  %s\n' "$digest" "$base/release.tar.gz" | sha256sum --check --status
release="/opt/devbox/releases/$digest"
mkdir -p "$release"
step 'Unpacking the release'
python3 - "$base/release.tar.gz" "$release" <<'PY'
import sys, tarfile
with tarfile.open(sys.argv[1]) as archive:
    archive.extractall(sys.argv[2], filter='data')
PY
# The release is laid out as the checkout it was made from; this script and the
# release come from the same seed builder.
for need in provision/requirements.txt provision/ansible.cfg provision/ansible/site.yml; do
  [[ -f "$release/$need" ]] || { echo "The release has no $need: render the seed and the release with the same berth" >&2; set_state failed; exit 1; }
done
step 'Installing Ansible'
python3 -m venv /opt/devbox/ansible-venv
/opt/devbox/ansible-venv/bin/pip install -r "$release/provision/requirements.txt"
python3 - <<'PY'
import json, os
from pathlib import Path
d=json.loads(Path('/opt/devbox-bootstrap/deployment.json').read_text())
p=Path('/etc/devbox'); p.mkdir(parents=True,exist_ok=True)
v=d['ansible_vars']; v['devbox_profile']=d['profile']
# A seed of the qemu kind there once was is a cloud host that SSH reaches only from the QEMU host.
if v['devbox_profile']=='qemu':
    v['devbox_profile']='cloud'
    v['devbox_bootstrap_ssh_cidr']=v.get('devbox_bootstrap_ssh_cidr') or '10.0.2.2/32'
(p/'host.json').write_text(json.dumps(v))
os.chmod(p/'host.json',0o600)
PY
cd "$release"
step 'Applying the role'
# The role creates shared tools under /opt that the agent must be able to run,
# so it gets the normal umask rather than this script's private one.
(umask 022; ANSIBLE_CONFIG="$release/provision/ansible.cfg" /opt/devbox/ansible-venv/bin/ansible-playbook \
  -i localhost, -c local provision/ansible/site.yml -e @/etc/devbox/host.json)
ln -sfn "$release" /opt/devbox/current
printf '%s\n' "$digest" > "$state/ready"
chmod 0644 "$state/ready"
set_state configured-needs-enrollment
step 'The role is applied'
# Successful enrollment consumes the temporary key. The original USB/cloud user data
# may still contain it; use short-lived one-use keys, never a reusable credential.
python3 - <<'PY'
import json
from pathlib import Path
for path in ['/etc/devbox/host.json','/opt/devbox-bootstrap/deployment.json']:
    p=Path(path); d=json.loads(p.read_text()); v=d.get('ansible_vars', d)
    v.pop('devbox_tailscale_auth_key',None)
    p.write_text(json.dumps(d))
PY
