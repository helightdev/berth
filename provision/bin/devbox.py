#!/usr/bin/env python3
"""Offline release/seed/ISO tooling. Does not write physical disks or create cloud resources."""
import argparse
import base64
import gzip
import hashlib
import importlib.util
import ipaddress
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import urllib.request
import sys
import tarfile
import tempfile
import uuid

# The root of the source: the checkout, or the unpacked release.
ROOT = Path(__file__).resolve().parents[2]
# The berth UI keeps build output outside the checkout.
BUILD = Path(os.environ.get('DEVBOX_BUILD_DIR') or ROOT / 'build').resolve()

# Explicit allowlist: local profiles, keys and build outputs never enter a release.
# A release is laid out as the checkout is.
RELEASE_SOURCES = [
    'provision/ansible', 'provision/bin', 'provision/bootstrap', 'provision/requirements.txt',
    'provision/ansible.cfg', 'provision/config/example.json',
    'container', 'plugins', 'tests', 'README.md', 'LICENSE-MIT', 'LICENSE-APACHE',
    'crates/berth/Cargo.toml', 'crates/berth/build.rs', 'crates/berth/src', 'crates/vendor/tui-input',
    'crates/vendor/README.md',
]
PROFILES = {'laptop', 'cloud'}
# Where a QEMU guest on the user-mode network sees the machine it runs on.
QEMU_HOST = '10.0.2.2/32'
DEFAULT_OPERATOR = 'ops'
# Names Ubuntu already uses for system accounts or groups; useradd refuses them.
RESERVED_NAMES = set('''
    root daemon bin sys adm tty disk lp mail news uucp man
    proxy kmem dialout fax voice cdrom floppy tape sudo audio dip www-data
    backup operator list irc src shadow utmp video sasl plugdev staff games
    users nogroup admin docker
'''.split())
USERNAME = re.compile(r'[a-z_][a-z0-9_-]{0,31}')
HOSTNAME = re.compile(r'[a-z](?:[a-z0-9-]{0,61}[a-z0-9])?')
SSH_KEY = re.compile(r'(ssh-ed25519|ssh-rsa|ecdsa-sha2-nistp256) [A-Za-z0-9+/=]+(?: [^\n]*)?')


def _load_disk_guard():
    spec = importlib.util.spec_from_file_location('disk_guard', ROOT / 'provision/bin/check-target-disk.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


disk_guard = _load_disk_guard()


def digest(path):
    h = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def private_write(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, 'w') as stream:
        stream.write(value)
    path.chmod(0o600)


def text(config, key, default=''):
    value = config.get(key)
    if value is None:
        return default
    if not isinstance(value, str):
        raise ValueError(f'{key} must be a string')
    return value


def flag(config, key, default):
    value = config.get(key)
    if value is None:
        return default
    if not isinstance(value, bool):
        raise ValueError(f'{key} must be true or false')
    return value


def check_password_hash(value, account):
    if not value.startswith(('$6$', '$y$')) or len(value) < 40 or any(c.isspace() for c in value):
        raise ValueError(f'Provide a SHA512-crypt or yescrypt password hash for {account}')


def check_public_key(value, account):
    if not SSH_KEY.fullmatch(value):
        raise ValueError(f'Provide a real {account} SSH public key')
    try:
        decoded = base64.b64decode(value.split()[1], validate=True)
    except ValueError as exc:
        raise ValueError(f'Invalid {account} SSH public-key encoding') from exc
    if len(decoded) < 32:
        raise ValueError(f'{account} SSH public key is too short')


def berth_key():
    """The public key of the berth that builds this, which the operator always takes."""
    return os.environ.get('BERTH_SSH_PUBLIC_KEY', '').strip()


def account_keys(config, account, berth=True):
    """The public keys an account takes: the profile's list, the single key of
    an older profile, and for the operator the key of the berth that builds
    this. An older profile's `berth_ssh_key` no longer leaves that key out."""
    listed = config.get(f'{account}_ssh_public_keys') or []
    if not isinstance(listed, list) or not all(isinstance(key, str) for key in listed):
        raise ValueError(f'{account}_ssh_public_keys must be a list of keys')
    keys = [key.strip() for key in listed]
    keys += [key.strip() for key in text(config, f'{account}_ssh_public_key').splitlines()]
    if account == 'operator' and berth:
        keys.append(berth_key())
    unique = []
    for key in keys:
        if key and key not in unique:
            unique.append(key)
    return unique


def wifi_enabled(config):
    return flag(config, 'wifi_enabled', bool(config.get('wifi')))


def release_delivered():
    """Whether whatever creates the machine copies the release into it, so that
    first boot waits for the archive instead of downloading it."""
    return os.environ.get('BERTH_RELEASE_DELIVERED') == '1'


def retire_qemu(c):
    """A profile of the qemu kind there once was, as the cloud one it is now.
    That kind let SSH in only from the QEMU host when no recovery network was
    named, so it gets that network rather than SSH open to every address."""
    if c.get('profile') != 'qemu':
        return c
    c = dict(c, profile='cloud')
    variables = dict(c.get('ansible_vars') or {})
    if not variables.get('devbox_bootstrap_ssh_cidr'):
        variables['devbox_bootstrap_ssh_cidr'] = QEMU_HOST
    c['ansible_vars'] = variables
    return c


def checked_config(path, kind):
    """Load a host profile and reject anything that could not produce a working seed."""
    c = json.loads(Path(path).read_text())
    if not isinstance(c, dict):
        raise ValueError('Configuration must be a JSON object')
    c = retire_qemu(c)
    if not HOSTNAME.fullmatch(text(c, 'hostname')):
        raise ValueError('Invalid hostname: use lowercase letters, digits and hyphens, starting with a letter')
    if text(c, 'profile') not in PROFILES:
        raise ValueError('profile must be laptop or cloud')

    operator = text(c, 'operator_username', DEFAULT_OPERATOR)
    agent = text(c, 'agent_username', 'agent')
    for name in (operator, agent):
        if not USERNAME.fullmatch(name):
            raise ValueError(f'Invalid account name: {name!r}')
        if name in RESERVED_NAMES:
            raise ValueError(f'Account name {name!r} is reserved by Ubuntu; choose another')
    if operator == agent:
        raise ValueError('Operator and agent need distinct account names')
    if not account_keys(c, 'operator'):
        raise ValueError('Provide a real operator SSH public key')
    for account in ('operator', 'agent'):
        for key in account_keys(c, account):
            check_public_key(key, account)
    # The operator logs in over SSH with keys and uses sudo without a password, so its
    # password is for the console. The USB installer's identity section needs one.
    password = text(c, 'operator_password_hash')
    if password or kind == 'usb':
        check_password_hash(password, 'operator')
    if text(c, 'agent_password_hash'):
        check_password_hash(c['agent_password_hash'], 'agent')
    privileges = text(c, 'agent_privileges')
    if privileges not in ('', 'full', 'limited', 'none'):
        raise ValueError('agent_privileges must be full, limited or none')
    if not text(c, 'agent_password_hash') and not flag(c, 'agent_passwordless_sudo', True) and not privileges:
        raise ValueError('Agent password is required when passwordless sudo is off')

    variables = c.setdefault('ansible_vars', {})
    if not isinstance(variables, dict) or any(not k.startswith('devbox_') for k in variables):
        raise ValueError('ansible_vars must contain only devbox_ settings')
    text(c, 'tailscale_auth_key')
    login_server = text(c, 'tailscale_login_server')
    if login_server and not re.fullmatch(r'https?://[^\s/]+(/[^\s]*)?', login_server):
        raise ValueError('tailscale_login_server must be an http:// or https:// address')
    # berth puts a new key into the profile it renders; without berth there is none.
    if flag(c, 'tailscale_auto_key', False) and not c.get('tailscale_auth_key') \
            and variables.get('devbox_install_tailscale', True):
        raise ValueError('tailscale_auto_key needs berth to create the key: render the seed with berth, '
                         'or paste a key into tailscale_auth_key')

    if kind == 'usb':
        c['target_disk_mode'] = disk_guard.policy(c)['mode']
        if wifi_enabled(c):
            wifi = c.get('wifi')
            if not isinstance(wifi, dict) or not all(
                    isinstance(wifi.get(k), str) and wifi[k] for k in ('ssid', 'password')):
                raise ValueError('wifi needs an ssid and password')
            if not isinstance(wifi.get('interface') or '', str):
                raise ValueError('wifi interface must be a string')
    if kind == 'cloud':
        url = text(c, 'release_url')
        if not release_delivered() and (not url.startswith('https://') or 'REPLACE' in url):
            raise ValueError('Cloud seed needs an HTTPS release_url for the generated tarball')
        # The firewall keeps this route open until the tailnet has been enrolled.
        cidr = variables.get('devbox_bootstrap_ssh_cidr')
        # Left out, SSH is reachable from any address.
        try:
            if cidr not in (None, ''):
                if not isinstance(cidr, str):
                    raise ValueError
                ipaddress.ip_network(cidr, strict=False)
        except ValueError:
            raise ValueError('devbox_bootstrap_ssh_cidr must be a network such as YOUR_ADMIN_IP/32, or empty') from None
    return c


def release():
    """Build the deterministic provisioning archive shared by every deployment path."""
    BUILD.mkdir(exist_ok=True, mode=0o700)
    target = BUILD / 'devbox-release.tar.gz'
    paths = []
    for entry in RELEASE_SOURCES:
        p = ROOT / entry
        if p.is_dir():
            paths.extend(x for x in p.rglob('*')
                         if x.is_file() and '__pycache__' not in x.parts and x.suffix != '.pyc')
        elif p.exists():
            paths.append(p)
    entries = {str(p.relative_to(ROOT)): p for p in paths}
    # The user's own plugins travel with the release; theirs win over the source's.
    # berth names them as {id: directory}; each lands in plugins/ID like a shipped one.
    for name, directory in sorted(json.loads(os.environ.get('BERTH_PLUGINS') or '{}').items()):
        directory = Path(directory).resolve()
        if not re.fullmatch(r'[a-z][a-z0-9_-]*', name) or directory == ROOT / 'plugins' / name:
            continue
        stale = [entry for entry in entries if entry.startswith(f'plugins/{name}/')]
        for entry in stale:
            del entries[entry]
        for p in directory.rglob('*'):
            # Only the plugin's own dependencies: berth installs the plugin itself in a node_modules.
            if p.is_file() and not {'node_modules', '__pycache__'} & set(p.relative_to(directory).parts):
                entries[str(Path('plugins') / name / p.relative_to(directory))] = p
    with target.open('wb') as stream, \
            gzip.GzipFile(fileobj=stream, mode='wb', mtime=0, filename='') as gz, \
            tarfile.open(fileobj=gz, mode='w') as archive:
        for name, p in sorted(entries.items()):
            if p.is_symlink():
                raise ValueError(f'Release source contains symlink: {p}')
            info = archive.gettarinfo(str(p), arcname=name)
            info.uid = info.gid = info.mtime = 0
            info.uname = info.gname = ''
            # berth runs a plugin's provisioner command directly, so plugins keep their executables.
            executable = p.suffix == '.sh' or (name.startswith('plugins/') and p.stat().st_mode & 0o111)
            info.mode = 0o755 if executable else 0o644
            with p.open('rb') as src:
                archive.addfile(info, src)
    private_write(BUILD / 'release.sha256', digest(target) + '\n')
    return target


def account_vars(c):
    return {
        'devbox_user': c.get('agent_username', 'agent'),
        'devbox_agent_password_hash': c.get('agent_password_hash', ''),
        'devbox_agent_ssh_public_key': '\n'.join(account_keys(c, 'agent')),
        'devbox_agent_passwordless_sudo': c.get('agent_passwordless_sudo', True),
        # full, limited or none; a profile without it follows the flag above.
        'devbox_agent_privileges': c.get('agent_privileges', ''),
        'devbox_operator_user': c.get('operator_username', DEFAULT_OPERATOR),
        'devbox_operator_password_hash': c.get('operator_password_hash', ''),
        'devbox_operator_ssh_public_key': '\n'.join(account_keys(c, 'operator', berth=False)),
        'devbox_berth_ssh_public_key': berth_key(),
    }


def deployment(c, sha):
    """Everything the first-boot provisioner needs, read by bootstrap.sh on the target."""
    variables = dict(c.get('ansible_vars') or {})
    variables.update(account_vars(c))
    if c.get('tailscale_auth_key'):
        variables['devbox_tailscale_auth_key'] = c['tailscale_auth_key']
    if c.get('tailscale_login_server'):
        variables['devbox_tailscale_login_server'] = c['tailscale_login_server']
    # berth hands over the profile as the host keeps it, and the role writes it.
    if isinstance(c.get('berth_profile'), dict):
        variables['devbox_berth_profile'] = c['berth_profile']
    return {'profile': c['profile'], 'release_sha256': sha,
            'release_url': '' if release_delivered() else c.get('release_url', ''), 'ansible_vars': variables}


def autoinstall(c):
    payload = '/cdrom/agent-bootstrap'
    # The early command re-selects the disk on the target and pins its path, so
    # this match is only what Subiquity sees if that command is skipped.
    if disk_guard.policy(c)['mode'] == 'serial':
        match = {'serial': c['target_disk_serial']}
    else:
        match = {'path': '/dev/DEVBOX_SELECTION_PENDING'}
    install = {
        'version': 1,
        'interactive-sections': [],
        'locale': 'en_US.UTF-8',
        'keyboard': {'layout': c.get('keyboard') or 'de'},
        'refresh-installer': {'update': False},
        'identity': {'hostname': c['hostname'],
                     'username': c.get('operator_username', DEFAULT_OPERATOR),
                     'password': c['operator_password_hash']},
        'ssh': {'install-server': True, 'allow-pw': False,
                'authorized-keys': account_keys(c, 'operator')},
        'early-commands': [[
            'python3', f'{payload}/check-target-disk.py',
            '--policy', f'{payload}/disk-policy.json',
            '--source', f'{payload}/autoinstall.json', '--autoinstall', '/autoinstall.yaml']],
        'storage': {'layout': {'name': 'direct', 'match': match}},
        'packages': ['python3', 'ca-certificates', 'curl'],
        'late-commands': [
            'mkdir -p /target/opt/devbox-bootstrap',
            f'cp -r {payload}/. /target/opt/devbox-bootstrap/',
            # The copies hold the password hash and enrollment key.
            'chown -R root:root /target/opt/devbox-bootstrap',
            'chmod -R go-rwx /target/opt/devbox-bootstrap',
            'curtin in-target -- /bin/bash /opt/devbox-bootstrap/bootstrap.sh --stage-only'],
        'shutdown': 'poweroff',
    }
    if wifi_enabled(c):
        w = c['wifi']
        interface = w.get('interface') or ''
        if interface in ('', 'auto'):
            interface = disk_guard.WIFI_AUTO
        # A network section replaces the installer's default, so keep wired DHCP too.
        install['network'] = {
            'version': 2,
            'ethernets': {'wired': {'match': {'name': 'en*'}, 'dhcp4': True, 'optional': True}},
            # Without a name, the installer's early command fills in the detected interface.
            'wifis': {interface: {'dhcp4': True,
                                       'access-points': {w['ssid']: {'password': w['password']}}}},
        }
    return {'autoinstall': install}


def cloud_config(c, sha):
    boot = (ROOT / 'provision/bootstrap/bootstrap.sh').read_bytes()
    # Without a password the field is `*` rather than the `!` cloud-init locks with:
    # an sshd without PAM refuses key logins to an account locked with `!`.
    operator = {
        'name': c.get('operator_username', DEFAULT_OPERATOR), 'shell': '/bin/bash', 'groups': ['sudo'],
        'sudo': ['ALL=(ALL) NOPASSWD:ALL'], 'ssh_authorized_keys': account_keys(c, 'operator'),
        'lock_passwd': False, 'passwd': c.get('operator_password_hash') or '*',
    }
    files = [
        {'path': '/opt/devbox-bootstrap/bootstrap.sh', 'encoding': 'b64',
         'content': base64.b64encode(boot).decode(), 'permissions': '0700'},
        {'path': '/opt/devbox-bootstrap/deployment.json',
         'content': json.dumps(deployment(c, sha)), 'permissions': '0600'}]
    # A login for downloading the release, as a line of curl configuration. berth
    # passes it in the environment; the bootstrap deletes the file after use.
    login = os.environ.get('BERTH_RELEASE_LOGIN', '')
    if login:
        if '\n' in login or not re.match(r'(user|header) = "', login):
            raise ValueError('BERTH_RELEASE_LOGIN must be one user or header line for curl')
        files.append({'path': '/opt/devbox-bootstrap/release-login',
                      'content': login + '\n', 'permissions': '0600'})
    return {
        'hostname': c['hostname'], 'manage_etc_hosts': True,
        'users': ['default', operator],
        'ssh_pwauth': False,
        'write_files': files,
        'runcmd': [
            ['/bin/bash', '/opt/devbox-bootstrap/bootstrap.sh', '--stage-only'],
            ['systemctl', 'daemon-reload'],
            ['systemctl', 'start', '--no-block', 'devbox-provision.service', 'devbox-provision.timer']],
    }


def render(config, kind):
    c = checked_config(config, kind)
    # Always repackage: a seed must carry the digest of the current source.
    archive = release()
    sha = digest(archive)
    out = BUILD / c['hostname'] / kind
    out.mkdir(parents=True, exist_ok=True, mode=0o700)
    if kind == 'usb':
        seed = autoinstall(c)
        private_write(out / 'nocloud/user-data', '#cloud-config\n' + json.dumps(seed, indent=2) + '\n')
        private_write(out / 'nocloud/meta-data', json.dumps(
            {'instance-id': str(uuid.uuid4()), 'local-hostname': c['hostname']}) + '\n')
        payload = out / 'agent-bootstrap'
        payload.mkdir(exist_ok=True, mode=0o700)
        for source, name in [(ROOT / 'provision/bootstrap/bootstrap.sh', 'bootstrap.sh'),
                             (ROOT / 'provision/bin/check-target-disk.py', 'check-target-disk.py'),
                             (archive, 'release.tar.gz')]:
            shutil.copyfile(source, payload / name)
            (payload / name).chmod(0o600)
        policy = {k: v for k, v in c.items() if k.startswith('target_disk_')}
        private_write(payload / 'disk-policy.json', json.dumps(policy) + '\n')
        private_write(payload / 'autoinstall.json', json.dumps(seed) + '\n')
        private_write(payload / 'deployment.json', json.dumps(deployment(c, sha), indent=2) + '\n')
    else:
        value = '#cloud-config\n' + json.dumps(cloud_config(c, sha), indent=2) + '\n'
        if len(value.encode()) > 32768:
            raise ValueError('Cloud user-data exceeds 32 KiB')
        private_write(out / 'user-data', value)
    return out


def patch_grub(config):
    """Boot the first installer entry unattended with the NoCloud seed on the ISO."""
    seed = 'autoinstall "ds=nocloud;s=/cdrom/nocloud/"'
    out = []
    entries = 0
    for line in config.splitlines():
        if re.match(r'\s*linux(?:efi)?\s+', line) and '/casper/vmlinuz' in line:
            if 'autoinstall' not in line:
                line = line.replace(' ---', f' {seed} ---') if ' ---' in line else f'{line} {seed}'
            entries += 1
        elif re.match(r'\s*set (timeout|default)=', line):
            continue
        out.append(line)
    if not entries:
        raise ValueError('No supported Ubuntu installer kernel entry found')
    return 'set default=0\nset timeout=5\n' + '\n'.join(out) + '\n'


def build_iso(config, source, sha=None, overwrite=False):
    """Remaster an Ubuntu Server ISO. With `sha`, the source must match it; without,
    the computed digest is printed so it can be compared with Ubuntu's SHA256SUMS."""
    source = Path(source).resolve()
    if sha and not re.fullmatch('[a-f0-9]{64}', sha):
        raise ValueError('The source ISO SHA256 must be 64 lowercase hex characters')
    if not source.is_file():
        raise ValueError(f'Source ISO not found: {source}')
    if not shutil.which('xorriso'):
        raise ValueError('xorriso is required to build the installer ISO')
    c = checked_config(config, 'usb')
    target = BUILD / f"{c['hostname']}-autoinstall.iso"
    if target.exists() and not overwrite:
        raise ValueError(f'Refusing to overwrite {target}; remove it or pass --overwrite')
    actual = digest(source)
    if sha and actual != sha:
        raise ValueError(f'Source ISO does not match the expected SHA256 (it is {actual})')
    if not sha:
        print(f'Source ISO SHA256: {actual}\nNot checked against a trusted value; '
              "compare it with Ubuntu's published SHA256SUMS.", file=sys.stderr)
    seed = render(config, 'usb')
    # xorriso would treat an existing image as media to append to.
    target.unlink(missing_ok=True)
    with tempfile.TemporaryDirectory(prefix='devbox-iso-') as temp:
        temp = Path(temp)
        subprocess.run(['xorriso', '-osirrox', 'on', '-indev', str(source),
                        '-extract', '/boot/grub', str(temp / 'grub')], check=True)
        cmd = ['xorriso', '-indev', str(source), '-outdev', str(target),
               '-boot_image', 'any', 'replay',
               '-map', str(seed / 'nocloud'), '/nocloud',
               '-map', str(seed / 'agent-bootstrap'), '/agent-bootstrap']
        for name in ('grub.cfg', 'loopback.cfg'):
            menu = temp / 'grub' / name
            if menu.exists():
                menu.chmod(0o644)
                menu.write_text(patch_grub(menu.read_text()))
                cmd += ['-map', str(menu), f'/boot/grub/{name}']
            elif name == 'grub.cfg':
                raise ValueError('Source ISO has no /boot/grub/grub.cfg')
        subprocess.run(cmd, check=True)
    target.chmod(0o600)
    return target


UBUNTU_RELEASES = 'https://releases.ubuntu.com/26.04/'


def pick_iso(sums):
    """The newest server installer's (name, sha256) from Ubuntu's SHA256SUMS text."""
    found = []
    for line in sums.splitlines():
        match = re.fullmatch(r'([a-f0-9]{64}) [ *](ubuntu-([0-9.]+)-live-server-amd64\.iso)', line.strip())
        if match:
            found.append((tuple(int(part) for part in match[3].split('.')), match[2], match[1]))
    if not found:
        raise ValueError('No Ubuntu Server amd64 installer listed in SHA256SUMS')
    return max(found)[1:]


def fetch_iso(directory):
    """Download the Ubuntu Server installer and check it against the published
    SHA256SUMS. Both come over HTTPS; the signature on SHA256SUMS is not checked."""
    directory = Path(directory).expanduser().resolve()
    directory.mkdir(parents=True, exist_ok=True)
    with urllib.request.urlopen(UBUNTU_RELEASES + 'SHA256SUMS', timeout=30) as response:
        name, sha = pick_iso(response.read().decode())
    target = directory / name
    if target.exists() and digest(target) == sha:
        print(f'{name} is already downloaded and matches SHA256SUMS.', file=sys.stderr)
        return target
    part = target.with_name(name + '.part')
    print(f'Downloading {UBUNTU_RELEASES}{name}', file=sys.stderr)
    subprocess.run(['curl', '-fL', '--progress-bar', '-C', '-', '-o', str(part),
                    UBUNTU_RELEASES + name], check=True)
    if digest(part) != sha:
        part.unlink()
        raise ValueError(f'{name} does not match SHA256SUMS; the download was removed')
    part.replace(target)
    print(f'SHA256 {sha} matches SHA256SUMS.', file=sys.stderr)
    return target


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='action', required=True)
    sub.add_parser('release', help='build build/devbox-release.tar.gz')
    render_parser = sub.add_parser('render', help='render a USB or cloud seed from a host profile')
    render_parser.add_argument('--config', required=True)
    render_parser.add_argument('--kind', choices=['usb', 'cloud'], default='usb')
    iso_parser = sub.add_parser('iso', help='build an unattended installer ISO')
    iso_parser.add_argument('--config', required=True)
    iso_parser.add_argument('--iso', required=True)
    iso_parser.add_argument('--sha256', help='expected SHA256 of the source ISO; computed and printed when omitted')
    iso_parser.add_argument('--overwrite', action='store_true', help='replace an existing output ISO')
    fetch_parser = sub.add_parser('fetch-iso', help='download the Ubuntu Server installer and check its SHA256')
    fetch_parser.add_argument('--dir', required=True)
    args = parser.parse_args()
    try:
        if args.action == 'fetch-iso':
            result = fetch_iso(args.dir)
        elif args.action == 'release':
            result = release()
        elif args.action == 'render':
            result = render(args.config, args.kind)
        else:
            result = build_iso(args.config, args.iso, args.sha256, args.overwrite)
    except (ValueError, OSError, subprocess.SubprocessError) as exc:
        parser.exit(1, f'{exc}\n')
    print(result)


if __name__ == '__main__':
    main()
