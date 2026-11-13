"""What the end-to-end tests share: workstations in scratch directories, the
test host, a link to its berthd, berth serve, an OAuth issuer and a small
WebSocket client.

The tests run in the runner container of ci/compose.yml, next to
the test host `studio`, on a network that reaches nothing else. Everything a
test writes goes into scratch directories; the checkout is read-only.
"""
import base64
import contextlib
import fcntl
import hashlib
import http.client
import json
import os
import pty
import re
import secrets
import select
import shutil
import signal
import socket
import struct
import subprocess
import tempfile
import threading
import time
import unittest
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

SRC = Path(os.environ.get('BERTH_E2E_SOURCE', '/src'))
BERTH = SRC / 'target/debug/berth'
HOST = os.environ.get('BERTH_E2E_HOST', 'studio')
SEED = Path(os.environ.get('BERTH_E2E_SEED', '/seed'))
OPERATOR, AGENT = 'ops', 'agent'

# Everything scratch lives below this, removed when the run ends.
SCRATCH = Path(tempfile.mkdtemp(prefix='berth-e2e-'))

# Each Ansible run of a workstation or a berth serve, noted by the callback in
# fixtures/callbacks, and what the run made: for the summary run.py prints.
APPLIES = SCRATCH / 'applies.jsonl'
MADE = {'workstation': 0, 'serve': 0}


def counts():
    """How many playbook runs, their seconds, the tasks that took longest in
    them, and the workstations and servers made so far."""
    runs = [json.loads(line) for line in APPLIES.read_text().splitlines()] if APPLIES.exists() else []
    tasks = {}
    for run in runs:
        for seconds, name in run.get('slowest', []):
            tasks[name] = tasks.get(name, 0) + seconds
    return {'apply': len(runs), 'apply_seconds': sum(run['seconds'] for run in runs), 'tasks': tasks, **MADE}


def runtime_dir():
    """A runtime directory: short, for the sockets of ssh's shared connections in it."""
    return Path(tempfile.mkdtemp(prefix='r', dir=SCRATCH.parent))


def needs_e2e(cls):
    """Skip a test case outside the runner container."""
    reason = 'the end-to-end tests run in containers: ci/run.sh e2e'
    return unittest.skipUnless(os.environ.get('BERTH_E2E') == '1', reason)(cls)


def wait_for(what, check, timeout=30.0, every=0.25):
    """Call check until it returns something true, and return that."""
    end = time.monotonic() + timeout
    last = None
    while time.monotonic() < end:
        try:
            found = check()
            if found:
                return found
        except Exception as error:  # noqa: BLE001 (kept to say what went wrong last)
            last = error
        time.sleep(every)
    raise AssertionError(f'{what} did not happen within {timeout:.0f}s' + (f': {last!r}' if last else ''))


def keygen(path, comment):
    subprocess.run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-C', comment, '-f', str(path)],
                   check=True, stdin=subprocess.DEVNULL)
    return Path(f'{path}.pub').read_text().strip()


# ---------------------------------------------------------------------------
# The provisioning source berth applies


def source():
    """The checkout with tests/e2e/site.yml as provision/ansible/site.yml, made once."""
    overlay = SCRATCH / 'source'
    if overlay.exists():
        return overlay
    overlay.mkdir()
    for entry in SRC.iterdir():
        # A virtual environment or build output of the checkout is not ours.
        if entry.name.startswith('.') or entry.name in ('provision', 'target', 'build'):
            continue
        (overlay / entry.name).symlink_to(entry)
    (overlay / 'provision').mkdir()
    for entry in (SRC / 'provision').iterdir():
        if entry.name != 'ansible':
            (overlay / 'provision' / entry.name).symlink_to(entry)
    (overlay / 'provision/ansible').mkdir()
    (overlay / 'provision/ansible/roles').symlink_to(SRC / 'provision/ansible/roles')
    (overlay / 'provision/ansible/site.yml').symlink_to(SRC / 'tests/e2e/site.yml')
    # The controller alone is installed by the role's own playbook for it.
    (overlay / 'provision/ansible/controller.yml').symlink_to(SRC / 'provision/ansible/controller.yml')
    return overlay


def source_with_plugins(name, plugins):
    """Another overlay of the checkout whose plugins/ has these besides the
    shipped ones, by id: {id: folder}, copied so a test may change them."""
    overlay = SCRATCH / name
    overlay.mkdir()
    for entry in source().iterdir():
        if entry.name != 'plugins':
            (overlay / entry.name).symlink_to(entry.resolve() if entry.name != 'provision' else entry)
    (overlay / 'plugins').mkdir()
    for shipped in (SRC / 'plugins').iterdir():
        (overlay / 'plugins' / shipped.name).symlink_to(shipped)
    for plugin, folder in plugins.items():
        shutil.copytree(folder, overlay / 'plugins' / plugin)
    return overlay


# ---------------------------------------------------------------------------
# A workstation: a HOME and XDG directories of its own


class Workstation:
    """A user's machine with berth: its own HOME, XDG directories and SSH key."""

    def __init__(self, name, ssh_key=True, source_dir=None, trust=True):
        self.root = SCRATCH / name
        self.home = self.root / 'home'
        self.config = self.home / '.config'
        self.data = self.home / '.local/share'
        self.run_dir = runtime_dir()
        _runtime_dirs.append(self.run_dir)
        for path in (self.home / '.ssh', self.config, self.data):
            path.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.env = {
            'HOME': str(self.home), 'USER': 'tester', 'LOGNAME': 'tester',
            'XDG_CONFIG_HOME': str(self.config), 'XDG_DATA_HOME': str(self.data),
            'XDG_RUNTIME_DIR': str(self.run_dir),
            'PATH': os.environ['PATH'], 'LANG': 'C.UTF-8', 'TERM': 'xterm-256color',
            'BERTH_SOURCE': str(source_dir or source()),
            # npm stays off the network: plugins come from folders here.
            'npm_config_registry': 'http://127.0.0.1:9/', 'npm_config_offline': 'true',
            'npm_config_cache': str(self.root / 'npm-cache'), 'npm_config_update_notifier': 'false',
            'ANSIBLE_LOCAL_TEMP': str(self.root / 'ansible-tmp'), 'ANSIBLE_FORCE_COLOR': '0',
            'ANSIBLE_CALLBACK_PLUGINS': str(Path(__file__).resolve().parent / 'fixtures/callbacks'),
            'ANSIBLE_CALLBACKS_ENABLED': 'e2e_applies', 'BERTH_E2E_APPLIES': str(APPLIES),
        }
        MADE['workstation'] += 1
        self.key = self.home / '.ssh/id_ed25519'
        self.public = keygen(self.key, f'tester@{name}') if ssh_key else None
        # A host that is not up yet is trusted once it answers.
        if trust:
            self.trust_host()

    @property
    def berth_dir(self):
        return self.config / 'berth'

    @property
    def own_key(self):
        """The file of berth's own key, the key `berth` of its key ring, which
        berth writes from its store when ssh needs it."""
        return self.data / 'berth/keys/berth'

    def own_public(self):
        """The public half of berth's own key, made when first asked for."""
        return self.berth('key', 'show').stdout.strip()

    def own_key_file(self):
        """berth's own key as a file: berth writes it for an inventory, here
        of a host that is only made for that and removed again."""
        probe = 'key-file'
        self.write_profile({'hostname': probe, 'builder': {}, 'access': {'address': '192.0.2.1'}}, name=probe)
        self.berth('inventory', probe)
        self.remove_host(probe)
        return self.own_key

    def trust_host(self):
        """Know the test host's key, as after a first ssh with a check of its fingerprint."""
        scanned = wait_for('the test host answering on SSH', lambda: subprocess.run(
            ['ssh-keyscan', '-T', '2', '-t', 'ed25519', HOST], capture_output=True, text=True, check=True
        ).stdout.strip(), timeout=60)
        (self.home / '.ssh/known_hosts').write_text(scanned + '\n')
        # ssh finds ~ in the password database, not in HOME: the runner's
        # own list of known hosts is the one it reads. The tests of another
        # host may run at the same time and keep theirs in it.
        known = Path('/etc/ssh/ssh_known_hosts')
        with open(f'{known}.lock', 'w') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            others = [line for line in (known.read_text().splitlines() if known.exists() else [])
                      if line.split(' ', 1)[0] != HOST]
            known.with_suffix('.new').write_text(''.join(line + '\n' for line in [*others, scanned]))
            known.with_suffix('.new').replace(known)

    def berth(self, *args, input=None, check=True, timeout=600, env=None):
        done = subprocess.run([str(BERTH), *args], env={**self.env, **(env or {})}, cwd=self.home,
                              input=input, capture_output=True, text=True, timeout=timeout)
        if check and done.returncode != 0:
            raise AssertionError(f'berth {" ".join(args)} exited {done.returncode}:\n'
                                 f'{done.stdout[-4000:]}\n{done.stderr[-4000:]}')
        return done

    def spawn(self, *args, env=None):
        return subprocess.Popen([str(BERTH), *args], env={**self.env, **(env or {})}, cwd=self.home,
                                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                text=True)

    def ssh(self, user, command, key=None, check=True, input=None, timeout=60):
        """A command on the test host over plain SSH, with one key only."""
        argv = ['ssh', '-o', 'BatchMode=yes', '-o', 'IdentitiesOnly=yes', '-o', 'ConnectTimeout=10',
                '-o', f'UserKnownHostsFile={self.home}/.ssh/known_hosts',
                '-i', str(key or self.key), f'{user}@{HOST}', command]
        done = subprocess.run(argv, env=self.env, input=input, capture_output=True, text=True, timeout=timeout)
        if check and done.returncode != 0:
            raise AssertionError(f'ssh {user}@{HOST} {command!r} exited {done.returncode}: {done.stderr}')
        return done

    def root_on_host(self, command, check=True):
        return self.ssh(OPERATOR, f'sudo -n sh -c {shell_quote(command)}', check=check)

    def write_profile(self, profile, name=HOST):
        """Save a host's profile in berth's store, as `berth profile --set` does."""
        self.berth('profile', name, '--set', '-', input=json.dumps(profile, indent=2) + '\n')

    def read_profile(self, name=HOST):
        return json.loads(self.berth('profile', name).stdout)

    def remove_host(self, name=HOST, check=True):
        """Forget a host, with what berth keeps of it."""
        return self.berth('remove', name, check=check)

    def link(self):
        return BerthdLink(self)

    def terminal(self, *args, cols=100, rows=30):
        return Terminal([str(BERTH), *args], self.env, cols, rows, self.home)


_runtime_dirs = []


def fresh_runtime_dir():
    """A runtime directory of its own, so no shared SSH connection is used again."""
    path = runtime_dir()
    _runtime_dirs.append(path)
    return str(path)


def shell_quote(text):
    return "'" + text.replace("'", "'\"'\"'") + "'"


def profile(**values):
    """A profile like berth writes for a new host, with the test host's address."""
    base = json.loads((SRC / 'provision/config/example.json').read_text())
    base.update({
        'hostname': HOST, 'profile': 'cloud', 'keyboard': 'us',
        # The operator's sudo needs no password, as by default, so berth asks
        # for none. shipped_plugins says the features are chosen already, as
        # in every profile berth saves. berth logs in with the user's SSH
        # configuration, which offers the user's key before berth's own: the
        # host takes the user's key until the first apply gives it berth's.
        'builder': {'shipped_plugins': True},
        'access': {'address': HOST, 'user': OPERATOR, 'auth': 'ssh'},
        'ansible_vars': light_vars(),
    })
    base.update(values)
    return base


def light_vars(features=()):
    """What keeps the role to the tasks the tests apply: no services to start
    (there is no systemd), no GPU, no plugins unless asked for."""
    return {
        'devbox_plugin_features': list(features),
        'devbox_start_services': False,
        'devbox_software_rendering': False,
        'devbox_selected_commands': [],
        'devbox_enabled_categories': [],
        'devbox_install_docker': False,
        'devbox_install_tailscale': False,
        'devbox_install_cockpit': False,
        'devbox_install_playwright': False,
    }


# ---------------------------------------------------------------------------
# The test host


def hand_over_operator_key(*public):
    """Put keys in place for the operator, as a cloud provider does at the start."""
    SEED.mkdir(parents=True, exist_ok=True)
    (SEED / f'{OPERATOR}.pub').write_text(''.join(key + '\n' for key in public))


_provisioned = {}


def provisioned():
    """The workstation that set the test host up, applying its profile once per run."""
    if 'workstation' in _provisioned:
        return _provisioned['workstation']
    station = Workstation('workstation')
    hand_over_operator_key(station.public)
    wait_for('the operator taking the provided key', lambda: station.ssh(OPERATOR, 'true', check=False).returncode == 0)
    extra = keygen(station.root / 'second_key', 'second@workstation')
    agent_one = keygen(station.root / 'agent_one', 'agent-one@workstation')
    agent_two = keygen(station.root / 'agent_two', 'agent-two@workstation')
    station.write_profile(profile(
        operator_ssh_public_keys=[station.public, extra],
        agent_ssh_public_keys=[agent_one, agent_two],
    ))
    started = time.monotonic()
    first = station.berth('apply', HOST, timeout=900)
    station.apply_seconds = time.monotonic() - started
    station.first_apply = first.stdout
    wait_for('berthd answering', lambda: station.link().get('/v1/version'), timeout=60)
    _provisioned['workstation'] = station
    return station


def adopt(station):
    """Take a workstation whose host was set up otherwise, such as by its
    first boot, as the one provisioned() gives the tests."""
    _provisioned['workstation'] = station


def served():
    """A berth serve on a workstation of its own, set up through its API to
    reach the test host, and trusting an OAuth issuer; started once per run."""
    if 'serve' in _provisioned:
        return _provisioned['serve']
    station = provisioned()
    server = Workstation('server', ssh_key=False)
    own = server.own_public()
    # The server's key reaches the operator the way the first one did.
    hand_over_operator_key(station.public, own)
    key = server.own_key_file()
    wait_for('the operator taking the server\'s key', lambda: server.ssh(
        OPERATOR, 'true', key=key, check=False).returncode == 0)
    issuer = Issuer(clients={'berth-client': ('client-secret', 'openid berth:operate')})
    serve = Serve(server, web=True, env={'BERTH_OAUTH_ISSUER': issuer.url, 'BERTH_OAUTH_AUDIENCE': 'berth',
                                         **package_registry().env()})
    serve.issuer = issuer
    # The host's profile as the workstation has it, so applying it from the
    # server keeps the workstation's keys; the server adds its own. What the
    # workstation turned on for its own tests stays there.
    host = station.read_profile()
    host['operator_ssh_public_keys'].append(station.own_public())
    # The server logs in with its own key; the SSH configuration is its own account's.
    host['access']['auth'] = 'berth'
    host['ansible_vars'] = light_vars()
    for key in ('secrets', 'registries', 'tailscale_auto_key', 'tailscale_auth_key', 'tailscale_login_server'):
        host.pop(key, None)
    serve.call('PUT', f'/v1/hosts/{HOST}/profile', host)
    _provisioned['serve'] = serve
    return serve


def stop_serving():
    serve = _provisioned.pop('serve', None)
    if serve:
        serve.stop()
        serve.issuer.stop()
        hand_over_operator_key(_provisioned['workstation'].public)


# ---------------------------------------------------------------------------
# States of the test host that several test cases share. Each is made with
# one apply (and one more to show that applying again changes nothing), the
# first time a test case asks for it, and its tests then only look. The host
# is in one of them at a time: asking for another first undoes what the
# current one set up on the workstation, the server and the tailnet, and
# then applies its own. run.py orders the test cases so each state is made
# once in a full run; a test case run alone makes the one it needs.

FIXTURE = SRC / 'tests/e2e/fixtures/plugin'
# The features of the extended state, from three kinds of plugin: a folder
# added to berth, a package from a registry, and one that comes with berth.
EXTENDED_FEATURES = ['e2e/hello', 'up/main', 'shipit/main']

_state = {'name': None, 'value': None, 'leave': None}


def state(name, enter):
    """The shared state `name`, made by `enter` unless it is the current one.
    `enter` returns (value, leave): what the tests get, and what undoes it."""
    if _state['name'] == name:
        return _state['value']
    leave, _state['name'], _state['leave'] = _state['leave'], None, None
    if leave:
        leave()
    value, leave = enter()
    _state.update(name=name, value=value, leave=leave)
    return value


def plugin_version(version, name='berth-plugin-up'):
    """The fixture plugin as `name` at a version, its id the name without
    berth-plugin-. From 2.0.0 its greeting and service changed, its Ansible
    task installs another file, the task `old` is gone and `added` is new."""
    plugin = name.removeprefix('berth-plugin-')
    folder = SCRATCH / f'{name}-{version}'
    if folder.exists():
        shutil.rmtree(folder)
    shutil.copytree(FIXTURE, folder)
    manifest = json.loads((folder / 'package.json').read_text())
    manifest['name'], manifest['version'] = name, version
    feature = manifest['berth']['features'][0]
    feature['id'] = 'main'
    feature['guide'] = ''
    newer = version != '1.0.0'
    feature['actions'] = [
        {'name': 'hello', 'label': 'Say hello', 'run': f'echo "hello from {version}"'},
        {'name': 'added' if newer else 'old', 'label': 'Added' if newer else 'Old',
         'run': f'echo "{"added" if newer else "old"} task"'},
    ]
    feature['services'][0]['run'] = f'exec box serve {plugin}/echo -- /usr/local/bin/berth-e2e-echo {version}'
    feature['channels'] = [{'topic': 'notes', 'keep': 'log', 'max': 50}]
    (folder / 'package.json').write_text(json.dumps(manifest, indent=2))
    (folder / 'tasks/install.yml').write_text(f'''---
- name: Create the folder of what the plugin brings
  ansible.builtin.file:
    path: /usr/local/lib/berth-{plugin}
    state: directory
    mode: '0755'
- name: Install what this version of the plugin brings
  ansible.builtin.copy:
    dest: /usr/local/lib/berth-{plugin}/{"two" if newer else "one"}
    content: "{version}\\n"
    mode: '0644'
''')
    return folder


def joining(profile, **values):
    """A profile whose host gets Tailscale and joins the tailnet."""
    profile = {**profile, **values}
    profile['ansible_vars'] = {**profile.get('ansible_vars', {}), 'devbox_install_tailscale': True}
    return profile


def leave_the_tailnet(station):
    """Start the host's tailscaled afresh, as a machine that never joined, and
    have Headscale forget it."""
    station.root_on_host('rm -rf /var/lib/tailscale; pkill -x tailscaled', check=False)
    wait_for('tailscaled answering again', lambda: station.root_on_host(
        'tailscale status --json', check=False).stdout.strip())
    headscale().forget_nodes()


def last_key_id():
    """The newest of Headscale's keys."""
    return max([int(key['id']) for key in headscale().keys()], default=0)


class Applied:
    """What a shared state was made with: its applies and what was asked before and after."""

    def __init__(self, **values):
        self.__dict__.update(values)


def extended():
    """The host applied from the workstation with three plugins' features
    (EXTENDED_FEATURES) and joined to the tailnet with a key berth made, then
    applied again. `first` and `second` are the two applies; `before` and
    `after_first` the newest Headscale key before each."""
    return state('extended', _enter_extended)


def _enter_extended():
    station = provisioned()
    kept = station.read_profile()
    registry = package_registry()
    # berth's source with a plugin that comes with it, for the rest of the run.
    station.env['BERTH_SOURCE'] = str(SCRATCH / 'source-shipped' if (SCRATCH / 'source-shipped').exists()
                                      else source_with_plugins('source-shipped', {
                                          'shipit': plugin_version('1.0.0', 'berth-plugin-shipit')}))
    station.berth('plugins', 'add', str(FIXTURE))
    registry.publish(plugin_version('1.0.0'))
    station.berth('plugins', 'add', 'berth-plugin-up', env=registry.env())
    leave_the_tailnet(station)
    station.berth('config', 'tailnet_login_server', headscale().url)
    station.berth('config', 'tailnet_user', headscale().user)
    station.berth('secret', 'set', 'tailnet_api_key', input=headscale().api_key + '\n')
    station.write_profile(joining({**kept, 'ansible_vars': light_vars(EXTENDED_FEATURES)}, tailscale_auto_key=True))
    before = last_key_id()
    first = station.berth('apply', HOST, timeout=900)
    after_first = last_key_id()
    second = station.berth('apply', HOST, timeout=900)

    def leave():
        leave_the_tailnet(station)
        station.write_profile(kept)
        for key in ('tailnet_login_server', 'tailnet_user'):
            station.berth('config', key)
        station.berth('secret', 'remove', 'tailnet_api_key', check=False)
        for package in ('berth-plugin-e2e', 'berth-plugin-up'):
            station.berth('plugins', 'remove', package, check=False)
    return Applied(station=station, kept=kept, first=first, second=second, before=before,
                   after_first=after_first), leave


def plain():
    """The host applied from the workstation again without the extended
    state's plugins, joining the tailnet with a key made by hand and pasted
    into the profile. `first` is the apply, `before` the newest Headscale key
    before the key was made."""
    return state('plain', _enter_plain)


def _enter_plain():
    station = provisioned()
    for package in ('berth-plugin-e2e', 'berth-plugin-up'):
        if (station.berth_dir / 'plugins/node_modules' / package).exists():
            station.berth('plugins', 'remove', package)
    kept = station.read_profile()
    leave_the_tailnet(station)
    before = last_key_id()
    station.write_profile(joining({**kept, 'ansible_vars': light_vars()}, tailscale_auth_key=headscale().new_key(),
                                  tailscale_login_server=headscale().url))
    first = station.berth('apply', HOST, timeout=900)

    def leave():
        leave_the_tailnet(station)
        station.write_profile(kept)
    return Applied(station=station, kept=kept, first=first, before=before), leave


def served_extended():
    """The host applied by berth serve with a plugin from the registry and
    joined to the tailnet with a key the server made: first by a terminal
    client of the server, then again as a job of the server, followed over
    the portal. `client` is that client; `first` its apply; `job`, `texts`
    and `data` the job and what following it gave; `before`, `after_first`
    and `after_second` the newest Headscale key before and after each."""
    return state('served-extended', _enter_served_extended)


def _enter_served_extended():
    serve = served()
    station = provisioned()
    leave_the_tailnet(station)
    registry = package_registry()
    registry.publish(plugin_version('1.0.0', 'berth-plugin-served'))
    serve.call('POST', '/v1/plugins', {'spec': 'berth-plugin-served'})
    client = Workstation('served-client', ssh_key=False)
    client.berth('config', 'server', serve.url)
    client.berth('config', 'server_token', serve.token)
    client.berth('config', 'tailnet_login_server', headscale().url)
    client.berth('config', 'tailnet_user', headscale().user)
    client.berth('secret', 'set', 'tailnet_api_key', input=headscale().api_key + '\n')
    kept = serve.call('GET', f'/v1/hosts/{HOST}/profile')
    serve.call('PUT', f'/v1/hosts/{HOST}/profile',
               joining({**kept, 'ansible_vars': light_vars(['served/main'])}, tailscale_auto_key=True))
    before = last_key_id()
    first = client.berth('apply', HOST, timeout=900)
    after_first = last_key_id()
    job = serve.call('POST', f'/v1/hosts/{HOST}/actions/apply', expect=202)['job']
    portal = serve.portal()
    try:
        texts, data, _ = portal.stream('jobs.follow', {'id': job['id']}, timeout=900,
                                       until=lambda texts, _: any('exited' in (text or {}) for text in texts))
    finally:
        portal.close()
    after_second = last_key_id()

    def leave():
        leave_the_tailnet(station)
        serve.call('PUT', f'/v1/hosts/{HOST}/profile', kept)
        serve.request('DELETE', '/v1/plugins/served')
        for key in ('tailnet_login_server', 'tailnet_user'):
            serve.call('PUT', f'/v1/settings/{key}', {'value': ''})
        serve.request('DELETE', '/v1/secrets/tailnet_api_key')
    return Applied(serve=serve, station=station, client=client, kept=kept, first=first, job=job, texts=texts,
                   data=data, before=before, after_first=after_first, after_second=after_second), leave


def ansible_recap(output):
    """ok, changed and failed of the one host in an Ansible recap."""
    found = re.search(r'ok=(\d+)\s+changed=(\d+)\s+unreachable=(\d+)\s+failed=(\d+)', output)
    if not found:
        raise AssertionError(f'No play recap in:\n{output[-3000:]}')
    ok, changed, unreachable, failed = map(int, found.groups())
    return {'ok': ok, 'changed': changed, 'unreachable': unreachable, 'failed': failed}


def changed_tasks(output):
    """The names of the tasks Ansible reported as changed."""
    names, current = [], None
    for line in output.splitlines():
        task = re.match(r'TASK \[(.*)\]', line)
        if task:
            current = task.group(1)
        elif line.startswith('changed:') and current and current not in names:
            names.append(current)
    return names


# ---------------------------------------------------------------------------
# berthd over the link berth uses: HTTP on the standard input and output of
# `sudo berthd connect`, over SSH as the operator


class Pipe:
    def __init__(self, stream):
        self.stream = stream

    def makefile(self, *_args, **_kwargs):
        return self.stream


class BerthdLink:
    def __init__(self, station, key=None):
        self.station = station
        # berth writes its key as a file only when something needs it.
        self.key = key or station.own_key_file()

    def request(self, method, path, body=None, raw=False, headers=None):
        """(status, answer) of one request; the answer is JSON unless raw."""
        data = b'' if body is None else body if isinstance(body, bytes) else json.dumps(body).encode()
        head = f'{method} {path} HTTP/1.1\r\nHost: berth\r\nConnection: close\r\nContent-Length: {len(data)}\r\n'
        if not isinstance(body, bytes):
            head += 'Content-Type: application/json\r\n'
        for name, value in (headers or {}).items():
            head += f'{name}: {value}\r\n'
        argv = ['ssh', '-o', 'BatchMode=yes', '-o', 'IdentitiesOnly=yes', '-i', str(self.key),
                '-o', f'UserKnownHostsFile={self.station.home}/.ssh/known_hosts',
                '-T', f'{OPERATOR}@{HOST}', 'sudo -n berthd connect']
        link = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                env=self.station.env)
        try:
            link.stdin.write(head.encode() + b'\r\n' + data)
            link.stdin.flush()
            answer = http.client.HTTPResponse(Pipe(link.stdout), method=method)
            answer.begin()
            text = answer.read()
            return answer.status, text if raw else json.loads(text or b'null')
        except (http.client.HTTPException, ConnectionError) as error:
            said = link.stderr.read().decode(errors='replace') if link.poll() is not None else ''
            raise AssertionError(f'{method} {path}: {said or error!r}') from error
        finally:
            link.kill()
            link.wait()

    def get(self, path):
        status, answer = self.request('GET', path)
        if status >= 400:
            raise AssertionError(f'GET {path}: {status} {answer}')
        return answer

    def post(self, path, body=None):
        status, answer = self.request('POST', path, body if body is not None else {})
        if status >= 400:
            raise AssertionError(f'POST {path}: {status} {answer}')
        return answer


def b64(data):
    return base64.b64encode(data if isinstance(data, bytes) else data.encode()).decode()


def unb64(text):
    return base64.b64decode(text or '').decode(errors='replace')


# ---------------------------------------------------------------------------
# berth serve


class Serve:
    """`berth serve --listen` of a workstation, on a port of its own."""

    def __init__(self, station, web=True, env=None):
        MADE['serve'] += 1
        self.station = station
        self.port = free_port()
        args = ['serve', '--listen', f'127.0.0.1:{self.port}'] + (['--web'] if web else [])
        self.process = station.spawn(*args, env=env)
        self.lines = []
        self._reader = threading.Thread(target=self._read, daemon=True)
        self._reader.start()
        wait_for('berth serve listening', lambda: any('Listening on http://' in line for line in self.lines),
                 timeout=60)
        self.token = (station.data / 'berth/serve.token').read_text().strip()
        self.url = f'http://127.0.0.1:{self.port}'

    def _read(self):
        for line in self.process.stdout:
            self.lines.append(line.rstrip('\n'))

    def stop(self):
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGINT)
            try:
                self.process.wait(10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()

    def request(self, method, path, body=None, token=None, raw=False, headers=None):
        """(status, answer) over HTTP with the server's token, or another bearer."""
        connection = http.client.HTTPConnection('127.0.0.1', self.port, timeout=120)
        sent = dict(headers or {})
        bearer = self.token if token is None else token
        if bearer:
            sent['Authorization'] = f'Bearer {bearer}'
        data = None
        if body is not None:
            data = body if isinstance(body, bytes) else json.dumps(body).encode()
            sent.setdefault('Content-Type', 'application/json')
        try:
            connection.request(method, path, body=data, headers=sent)
            answer = connection.getresponse()
            text = answer.read()
            if raw:
                return answer.status, text, {name.lower(): value for name, value in answer.getheaders()}
            try:
                return answer.status, json.loads(text or b'null')
            except ValueError:
                return answer.status, text.decode(errors='replace')
        finally:
            connection.close()

    def call(self, method, path, body=None, token=None, expect=200):
        status, answer = self.request(method, path, body, token)
        if status != expect:
            raise AssertionError(f'{method} {path}: {status} {answer}')
        return answer

    def portal(self, token=None):
        return Portal(self.port, self.token if token is None else token)


def free_port():
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        return probe.getsockname()[1]


# ---------------------------------------------------------------------------
# A WebSocket client, enough for the portal


class WebSocket:
    def __init__(self, port, path, protocols=(), headers=None, timeout=60):
        self.sock = socket.create_connection(('127.0.0.1', port), timeout=timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        lines = [f'GET {path} HTTP/1.1', f'Host: 127.0.0.1:{port}', 'Upgrade: websocket',
                 'Connection: Upgrade', f'Sec-WebSocket-Key: {key}', 'Sec-WebSocket-Version: 13']
        if protocols:
            lines.append('Sec-WebSocket-Protocol: ' + ', '.join(protocols))
        lines += [f'{name}: {value}' for name, value in (headers or {}).items()]
        self.sock.sendall(('\r\n'.join(lines) + '\r\n\r\n').encode())
        head = b''
        while b'\r\n\r\n' not in head:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise ConnectionError('closed during the handshake')
            head += chunk
        head, self.buffer = head.split(b'\r\n\r\n', 1)
        lines = head.decode().split('\r\n')
        self.status = int(lines[0].split()[1])
        self.headers = {name.lower(): value.strip() for name, _, value in
                        (line.partition(':') for line in lines[1:])}
        if self.status != 101:
            body = self.buffer.decode(errors='replace')
            self.sock.close()
            raise PermissionError(f'{self.status} {body}')
        accept = base64.b64encode(hashlib.sha1((key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest())
        assert self.headers.get('sec-websocket-accept') == accept.decode()

    def _read(self, count):
        while len(self.buffer) < count:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError('closed')
            self.buffer += chunk
        data, self.buffer = self.buffer[:count], self.buffer[count:]
        return data

    def send(self, payload, opcode=None):
        if isinstance(payload, (dict, list)):
            payload = json.dumps(payload)
        if isinstance(payload, str):
            opcode, payload = opcode or 1, payload.encode()
        else:
            opcode = opcode or 2
        mask = os.urandom(4)
        length = len(payload)
        if length < 126:
            head = struct.pack('!BB', 0x80 | opcode, 0x80 | length)
        elif length < 65536:
            head = struct.pack('!BBH', 0x80 | opcode, 0x80 | 126, length)
        else:
            head = struct.pack('!BBQ', 0x80 | opcode, 0x80 | 127, length)
        masked = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
        self.sock.sendall(head + mask + masked)

    def recv(self):
        """The next text (as JSON) or binary message; None once closed."""
        message, kind = b'', None
        while True:
            first, second = self._read(2)
            opcode, length = first & 0x0F, second & 0x7F
            if length == 126:
                length = struct.unpack('!H', self._read(2))[0]
            elif length == 127:
                length = struct.unpack('!Q', self._read(8))[0]
            payload = self._read(length)
            if opcode == 8:
                return None
            if opcode == 9:
                self.send(payload, opcode=10)
                continue
            if opcode == 10:
                continue
            if opcode in (1, 2):
                kind = opcode
            message += payload
            if first & 0x80:
                return json.loads(message) if kind == 1 else message

    def close(self):
        with contextlib.suppress(OSError):
            self.send(b'', opcode=8)
        self.sock.close()


class Portal:
    """Calls and streams over /v1/portal, numbered as the frames say."""

    def __init__(self, port, token):
        # A browser cannot set a header on a WebSocket: the token is the protocol.
        self.ws = WebSocket(port, '/v1/portal', protocols=[token] if token else ())
        self.next = 1
        self.pending = []

    def send(self, frame):
        self.ws.send(frame)

    def _take(self, matches, timeout):
        for frame in list(self.pending):
            if matches(frame):
                self.pending.remove(frame)
                return frame
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            self.ws.sock.settimeout(max(0.1, end - time.monotonic()))
            frame = self.ws.recv()
            if frame is None:
                raise ConnectionError('the portal closed')
            if matches(frame):
                return frame
            self.pending.append(frame)
        raise TimeoutError('no answer on the portal')

    def call(self, op, args=None, data=None, host=None, timeout=120):
        """The whole answer frame: result or error."""
        number = self.next
        self.next += 1
        frame = {'id': number, 'op': op}
        if args is not None:
            frame['args'] = args
        if data is not None:
            frame['data'] = data
        if host:
            frame['host'] = host
        self.send(frame)
        return self._take(lambda got: isinstance(got, dict) and got.get('id') == number, timeout)

    def result(self, op, args=None, data=None, host=None, timeout=120):
        answer = self.call(op, args, data, host, timeout)
        if 'error' in answer:
            raise AssertionError(f'{op}: {answer["error"]}')
        return answer['result']

    def stream(self, op, args=None, host=None, until=None, timeout=120):
        """Open a stream and collect its frames: text as JSON, bytes joined,
        until it ends or `until(texts, data)` is true."""
        opened = self.result(op, args, host=host, timeout=timeout)
        assert opened == {'open': True}, opened
        number = self.next - 1
        texts, data = [], b''
        tag = struct.pack('!I', number)

        def mine(frame):
            if isinstance(frame, bytes):
                return frame[:4] == tag
            return frame.get('sub') == number

        end = time.monotonic() + timeout
        while time.monotonic() < end:
            frame = self._take(mine, end - time.monotonic())
            if isinstance(frame, bytes):
                data += frame[4:]
            elif frame.get('end'):
                return texts, data, frame
            else:
                texts.append(frame.get('data'))
            if until and until(texts, data):
                self.send({'sub': number, 'end': True})
                return texts, data, None
        raise TimeoutError(f'{op} did not end')

    def close(self):
        self.ws.close()


# ---------------------------------------------------------------------------
# An OAuth 2.0 issuer: metadata, keys and a token endpoint for clients with
# a secret, signing with an Ed25519 key


def url64(data):
    return base64.urlsafe_b64encode(data).rstrip(b'=').decode()


class Issuer:
    def __init__(self, clients=None):
        from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey  # noqa: PLC0415
        from cryptography.hazmat.primitives import serialization  # noqa: PLC0415
        self.key = Ed25519PrivateKey.generate()
        public = self.key.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
        self.jwks = {'keys': [{'kty': 'OKP', 'crv': 'Ed25519', 'alg': 'EdDSA', 'kid': 'e2e', 'x': url64(public)}]}
        # client id: (secret, scope it is given)
        self.clients = clients or {}
        issuer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def answer(self, status, body):
                data = json.dumps(body).encode()
                self.send_response(status)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):  # noqa: N802 (the name http.server calls)
                if self.path == '/.well-known/openid-configuration':
                    self.answer(200, {'issuer': issuer.url, 'jwks_uri': f'{issuer.url}/keys',
                                      'token_endpoint': f'{issuer.url}/token'})
                elif self.path == '/keys':
                    self.answer(200, issuer.jwks)
                else:
                    self.answer(404, {'error': 'not_found'})

            def do_POST(self):  # noqa: N802
                from urllib.parse import parse_qs  # noqa: PLC0415
                length = int(self.headers.get('Content-Length') or 0)
                form = {name: values[0] for name, values in parse_qs(self.rfile.read(length).decode()).items()}
                client = issuer.clients.get(form.get('client_id'))
                if self.path != '/token' or form.get('grant_type') != 'client_credentials':
                    return self.answer(400, {'error': 'unsupported_grant_type'})
                if not client or client[0] != form.get('client_secret'):
                    return self.answer(401, {'error': 'invalid_client'})
                token = issuer.token(client[1], subject=form['client_id'])
                self.answer(200, {'access_token': token, 'token_type': 'Bearer', 'expires_in': 600})

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.url = f'http://127.0.0.1:{self.server.server_address[1]}'
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def token(self, scope, subject='ada', audience='berth', expires=600, issuer=None, key=None):
        header = {'alg': 'EdDSA', 'typ': 'JWT', 'kid': 'e2e'}
        claims = {'iss': issuer or self.url, 'sub': subject, 'aud': audience, 'scope': scope,
                  'iat': int(time.time()), 'exp': int(time.time()) + expires}
        signing = f'{url64(json.dumps(header).encode())}.{url64(json.dumps(claims).encode())}'
        signature = (key or self.key).sign(signing.encode())
        return f'{signing}.{url64(signature)}'

    def stop(self):
        self.server.shutdown()


# ---------------------------------------------------------------------------
# An npm registry that answers /-/ping and /-/whoami for one login


class NpmRegistry:
    def __init__(self, user, password):
        expected = 'Basic ' + b64(f'{user}:{password}')
        self.asked = []
        registry = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_GET(self):  # noqa: N802
                registry.asked.append(self.path)
                if self.headers.get('Authorization') != expected:
                    status, body = 401, {'error': 'login'}
                elif self.path.endswith('/-/whoami'):
                    status, body = 200, {'username': user}
                else:
                    status, body = 200, {}
                data = json.dumps(body).encode()
                self.send_response(status)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.url = f'http://127.0.0.1:{self.server.server_address[1]}/repository/npm/'
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def stop(self):
        self.server.shutdown()


# ---------------------------------------------------------------------------
# An npm registry that serves packages: what npm asks to install and to tell
# whether a newer version is there


class PackageRegistry:
    def __init__(self):
        # package name: {version: (manifest, tarball bytes)}
        self.packages = {}
        self.dir = Path(tempfile.mkdtemp(prefix='registry-', dir=SCRATCH))
        registry = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def answer(self, status, data, kind='application/json'):
                self.send_response(status)
                self.send_header('Content-Type', kind)
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):  # noqa: N802
                from urllib.parse import unquote  # noqa: PLC0415
                path = unquote(self.path.split('?')[0]).lstrip('/')
                if '/-/' in path:
                    name, file = path.split('/-/', 1)
                    for manifest, tarball in registry.packages.get(name, {}).values():
                        if manifest['dist']['tarball'].endswith('/' + file):
                            return self.answer(200, tarball, 'application/octet-stream')
                    return self.answer(404, b'{}')
                versions = registry.packages.get(path)
                if not versions:
                    return self.answer(404, b'{"error": "not found"}')
                latest = max(versions, key=lambda version: [int(part) for part in version.split('.')])
                body = {'name': path, 'dist-tags': {'latest': latest},
                        'versions': {version: manifest for version, (manifest, _) in versions.items()}}
                self.answer(200, json.dumps(body).encode())

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.url = f'http://127.0.0.1:{self.server.server_address[1]}/'
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def publish(self, folder):
        """Pack a folder as npm does and serve it as its name and version."""
        manifest = json.loads((Path(folder) / 'package.json').read_text())
        subprocess.run(['npm', 'pack', '--silent', '--ignore-scripts', '--pack-destination', str(self.dir),
                        str(folder)], check=True, capture_output=True, env={**os.environ, 'npm_config_offline': 'true'})
        name, version = manifest['name'], manifest['version']
        file = f'{name.split("/")[-1]}-{version}.tgz'
        tarball = (self.dir / file).read_bytes()
        integrity = 'sha512-' + base64.b64encode(hashlib.sha512(tarball).digest()).decode()
        manifest['dist'] = {'tarball': f'{self.url}{name}/-/{file}', 'integrity': integrity,
                            'shasum': hashlib.sha1(tarball).hexdigest()}  # noqa: S324 (what npm checks)
        self.packages.setdefault(name, {})[version] = (manifest, tarball)

    def env(self):
        """What lets npm reach it, for berth and berth serve."""
        return {'npm_config_registry': self.url, 'npm_config_offline': 'false'}

    def stop(self):
        self.server.shutdown()


_registry = []


def package_registry():
    """The registry of packages for this run, started once."""
    if not _registry:
        _registry.append(PackageRegistry())
    return _registry[0]


# ---------------------------------------------------------------------------
# The Headscale server of ci/compose.yml, which the test host can join


HEADSCALE = os.environ.get('BERTH_E2E_HEADSCALE', 'headscale')


class Headscale:
    """The `headscale` service of ci/compose.yml: its login server, the API key
    it hands out at start, and its API. It has one user, `berth`, number 1."""

    def __init__(self):
        self.url = f'http://{HEADSCALE}:8080'
        self.user = 'berth'

        def ready():
            with urllib.request.urlopen(f'http://{HEADSCALE}:8081/api-key', timeout=5) as answer:
                key = answer.read().decode().strip()
            # A key from an earlier start is refused until the new one is there.
            self.api_key = key
            return key and self.api('GET', '/api/v1/user') is not None
        wait_for('Headscale handing out its API key', ready, timeout=60)

    def api(self, method, path, body=None):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(self.url + path, data=data, method=method, headers={
            'Authorization': f'Bearer {self.api_key}', 'Content-Type': 'application/json'})
        with urllib.request.urlopen(request, timeout=20) as answer:
            return json.loads(answer.read() or b'null')

    def nodes(self):
        return self.api('GET', '/api/v1/node').get('nodes', [])

    def keys(self):
        return self.api('GET', '/api/v1/preauthkey').get('preAuthKeys', [])

    def new_key(self, hours=1):
        """A one-use key of the user, as an admin makes one by hand."""
        expiry = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime(time.time() + hours * 3600))
        made = self.api('POST', '/api/v1/preauthkey', {'user': '1', 'reusable': False, 'ephemeral': False,
                                                     'expiration': expiry})
        return made['preAuthKey']['key']

    def forget_nodes(self):
        for node in self.nodes():
            self.api('DELETE', f'/api/v1/node/{node["id"]}')


_headscale = []


def headscale():
    """The Headscale server, once it is ready."""
    if not _headscale:
        _headscale.append(Headscale())
    return _headscale[0]


# ---------------------------------------------------------------------------
# A pseudo-terminal, for the commands that want one


class Terminal:
    def __init__(self, argv, env, cols, rows, cwd):
        import fcntl  # noqa: PLC0415
        import termios  # noqa: PLC0415
        self.output = b''
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(cwd)
            os.execve(argv[0], argv, env)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack('HHHH', rows, cols, 0, 0))
        self.status = None

    def pump(self, seconds=0.2):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            ready, _, _ = select.select([self.fd], [], [], max(0.0, end - time.monotonic()))
            if not ready:
                continue
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                return
            if not data:
                return
            # Answer what crossterm asks the terminal, so it does not wait.
            if b'\x1b[6n' in data:
                os.write(self.fd, b'\x1b[1;1R')
            if b'\x1b[c' in data:
                os.write(self.fd, b'\x1b[?62;22c')
            self.output += data

    def text(self):
        plain = re.sub(rb'\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b[()][A-Z0-9]|\x1b[=>]',
                       b'', self.output)
        return plain.decode(errors='replace')

    def expect(self, pattern, timeout=30):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            if re.search(pattern, self.text()):
                return
            self.pump(0.2)
        raise AssertionError(f'{pattern!r} did not appear; the terminal showed:\n{self.text()[-3000:]}')

    def send(self, text):
        os.write(self.fd, text.encode() if isinstance(text, str) else text)

    def wait(self, timeout=30):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            self.pump(0.1)
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.status = os.waitstatus_to_exitcode(status)
                return self.status
        raise AssertionError(f'the program did not end; it showed:\n{self.text()[-3000:]}')

    def close(self):
        if self.status is None:
            with contextlib.suppress(ProcessLookupError):
                os.kill(self.pid, signal.SIGTERM)
            with contextlib.suppress(ChildProcessError):
                os.waitpid(self.pid, 0)
        with contextlib.suppress(OSError):
            os.close(self.fd)


def random_text(prefix):
    return f'{prefix}-{secrets.token_hex(6)}'


def cleanup():
    """Stop what runs, remove the scratch directories and the keys handed to the test host."""
    with contextlib.suppress(Exception):
        stop_serving()
    with contextlib.suppress(OSError):
        for key in SEED.glob('*.pub'):
            key.unlink()
    for path in [SCRATCH, *_runtime_dirs]:
        shutil.rmtree(path, ignore_errors=True)
