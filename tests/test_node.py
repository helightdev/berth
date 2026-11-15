"""A node: the role's path without the desktop, its firewall helper against a
fake ufw, the helper that upgrades a berth server's container against a fake
docker, and the features of the Incus and Docker plugins that a node takes."""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

try:
    import yaml
except ImportError:
    yaml = None

SRC = Path(__file__).resolve().parents[1]
ROLE = SRC / 'provision/ansible/roles/devbox'
FIREWALL = ROLE / 'files/node-firewall.sh'
UPGRADE = ROLE / 'files/berth-server-upgrade.py'
PLAYBOOK = shutil.which('ansible-playbook')


def tasks(path):
    """The tasks of a task file, with those inside blocks."""
    found = []

    def take(task):
        found.append(task)
        for key in ('block', 'rescue', 'always'):
            for child in task.get(key, []):
                take(child)

    for task in yaml.safe_load(Path(path).read_text()):
        take(task)
    return found


def named(name, path):
    return next(task for task in tasks(path) if task['name'] == name)


def node_tasks():
    """The tasks of the node's path in the order they run, with those of the
    node's own task files it imports in their place."""
    found = []
    for task in tasks(ROLE / 'tasks/node.yml'):
        imported = task.get('ansible.builtin.import_tasks', '')
        found.append(task)
        if imported.startswith('node_'):
            found.extend(tasks(ROLE / 'tasks' / imported))
    return found


def node_named(name):
    return next(task for task in node_tasks() if task['name'] == name)


def node_text():
    return ''.join((ROLE / 'tasks' / name).read_text()
                   for name in ('node.yml', 'node_facts.yml', 'node_ssh.yml', 'node_berth.yml'))


def write_program(path, text):
    path.write_text(text)
    path.chmod(0o755)


@unittest.skipUnless(yaml, 'Run with the Ansible venv Python for YAML checks')
class RolePath(unittest.TestCase):
    def test_the_role_takes_a_node_apart_from_a_workstation(self):
        main = yaml.safe_load((ROLE / 'tasks/main.yml').read_text())
        self.assertEqual([(task['ansible.builtin.include_tasks'], task['when']) for task in main], [
            ('node.yml', "devbox_level == 'node'"), ('workstation.yml', "devbox_level != 'node'")])

    def test_a_node_has_nothing_of_the_desktop_or_its_accounts(self):
        node = node_text()
        included = [task.get('ansible.builtin.import_tasks') or task.get('ansible.builtin.include_tasks')
                    for task in node_tasks()]
        included = sorted(name for name in included if name and not name.startswith('node_'))
        self.assertEqual(included, ['applied.yml', 'berth.yml', 'plugins.yml', 'system.yml',
                                    'tailnet.yml', 'tailnet_check.yml', 'tailscale.yml'])
        for absent in ('desktop.yml', 'accounts.yml', 'applications.yml', 'toolchains.yml', 'laptop.yml',
                       'host.yml', 'devbox_packages', 'novnc', 'cockpit', 't3code'):
            self.assertNotIn(absent, node, absent)
        packages = yaml.safe_load((ROLE / 'defaults/main.yml').read_text())['devbox_node_packages']
        for absent in ('xfce4', 'tigervnc-standalone-server', 'novnc', 'nodejs'):
            self.assertNotIn(absent, packages)
        self.assertIn('openssh-server', packages)
        self.assertIn('unattended-upgrades', packages)

    def test_root_keeps_a_login_with_keys_and_others_none(self):
        task = node_named('Require SSH keys, and keep root\'s login as berth uses it')
        content = task['ansible.builtin.copy']['content']
        self.assertIn('PasswordAuthentication no', content)
        self.assertIn("PermitRootLogin {{ 'prohibit-password' if devbox_node_user == 'root' else 'no' }}", content)
        # Only when the SSH user has a key, which berth's own is, so no apply locks berth out.
        self.assertEqual(task['when'], 'devbox_harden_ssh and devbox_node_keys.rc == 0')
        given = node_named('Give the SSH user the keys of the berth that applies this')
        self.assertEqual(given['ansible.builtin.blockinfile']['path'], '{{ devbox_node_home }}/.ssh/authorized_keys')
        order = [task['name'] for task in node_tasks()]
        self.assertLess(order.index('Give the SSH user the keys of the berth that applies this'),
                        order.index('Require SSH keys, and keep root\'s login as berth uses it'))

    def test_the_firewall_keeps_ssh_and_comes_last(self):
        task = node_named("Set the node's firewall")
        rules = task['vars']['devbox_node_firewall']
        self.assertIn("'allow 22/tcp' if devbox_ssh_from == 'any'", rules)
        self.assertIn("'allow from ' ~ devbox_apply_from ~ ' to any port 22 proto tcp'", rules)
        self.assertIn('devbox_firewall_rules', rules)
        order = [task['name'] for task in node_tasks()]
        self.assertGreater(order.index("Set the node's firewall"), order.index('Apply the features of the node'))

    def test_a_node_is_not_applied_over_a_workstation(self):
        task = node_named('Refuse a host that berth set up as a workstation')
        self.assertIn("level | default('workstation') == 'workstation'", task['when'][1])

    def test_the_actions_of_a_node_run_what_it_installs(self):
        actions = {task['name']: task for task in yaml.safe_load((ROLE / 'vars/main.yml').read_text())['devbox_node_tasks']}
        self.assertEqual(list(actions), ['status', 'update', 'reboot', 'secrets', 'upgrade-server'])
        self.assertTrue(actions['status']['run'].endswith('/status.sh node'))
        self.assertIn('if [[ ${1:-} == node ]]', (ROLE / 'files/tasks/status.sh').read_text())
        upgrade = actions['upgrade-server']
        self.assertTrue(upgrade['run'].startswith('/usr/local/lib/berth/berth-server-upgrade '))
        self.assertEqual(upgrade['arguments'][0]['name'], 'version')
        installed = node_named("Install the helper that upgrades a berth server's container")
        self.assertEqual(installed['ansible.builtin.copy']['dest'], '/usr/local/lib/berth/berth-server-upgrade')

    def test_a_feature_says_what_it_offers_and_for_which_hosts(self):
        plugin = tasks(ROLE / 'tasks/plugin.yml')
        names = [task['name'] for task in plugin]
        self.assertLess(names.index('Require the feature to be for this type of host'), names.index("Run the feature's tasks"))
        self.assertGreater(names.index('Record the provisioners the feature offers'), names.index("Run the feature's tasks"))
        applied = named('Record which berth and plugins this configuration came from', ROLE / 'tasks/applied.yml')
        self.assertIn("{'offers': devbox_plugin_offers}", applied['ansible.builtin.copy']['content'])

    @unittest.skipUnless(PLAYBOOK, 'ansible-playbook is not installed')
    def test_the_node_path_and_the_features_parse(self):
        # A role's task files that are included at run time are checked only when imported.
        with tempfile.TemporaryDirectory() as scratch:
            playbook = Path(scratch) / 'check.yml'
            files = [ROLE / 'tasks/node.yml', SRC / 'plugins/incus/tasks/server.yml', SRC / 'plugins/docker/tasks/engine.yml']
            playbook.write_text('- hosts: all\n  tasks:\n' + ''.join(
                f'    - ansible.builtin.import_tasks: {path}\n' for path in files))
            env = dict(os.environ, ANSIBLE_ROLES_PATH=str(ROLE.parent), ANSIBLE_HOME=str(Path(scratch) / 'home'),
                       ANSIBLE_LOCAL_TEMP=str(Path(scratch) / 'tmp'), ANSIBLE_NOCOLOR='1')
            done = subprocess.run([PLAYBOOK, '--syntax-check', '-i', 'big,', str(playbook)],
                                  capture_output=True, text=True, env=env, timeout=120)
            self.assertEqual(done.returncode, 0, done.stdout + done.stderr)


@unittest.skipUnless(yaml, 'Run with the Ansible venv Python for YAML checks')
class Features(unittest.TestCase):
    def feature(self, plugin, feature):
        manifest = json.loads((SRC / 'plugins' / plugin / 'package.json').read_text())
        found = next(item for item in manifest['berth']['features'] if item['id'] == feature)
        return manifest, found

    def test_the_engines_are_features_of_a_node_that_offer_their_provisioners(self):
        for plugin, feature, offers in (('incus', 'server', ['vm', 'container']), ('docker', 'engine', ['container'])):
            with self.subTest(plugin=plugin):
                manifest, found = self.feature(plugin, feature)
                self.assertEqual(found['types'], ['node'])
                self.assertEqual(found['offers'], offers)
                provisioners = [item['id'] for item in manifest['berth']['provisioners']]
                self.assertTrue(set(offers) <= set(provisioners))
                self.assertIn('tasks', manifest['files'])
                steps = tasks(SRC / 'plugins' / plugin / found['tasks'])
                offered = [step for step in steps if 'devbox_offer' in step.get('ansible.builtin.set_fact', {})]
                self.assertEqual(len(offered), 1)
                # The status of a node goes on with the feature's own.
                self.assertEqual(found['actions'][0]['extends'], 'status')
                # Actions are templated by the role, so they carry no Jinja of their own.
                self.assertNotIn('{{', json.dumps(found['actions']))

    def test_incus_trusts_berths_certificate_and_pins_its_own(self):
        manifest, found = self.feature('incus', 'server')
        self.assertEqual(found['trusts'], {'certificate': 'incus_client_cert', 'key': 'incus_client_key'})
        for provisioner in manifest['berth']['provisioners']:
            names = [secret if isinstance(secret, str) else secret['name'] for secret in provisioner['secrets']]
            self.assertTrue({'incus_client_cert', 'incus_client_key', 'incus_server_cert'} <= set(names))
        offer = named('Open the firewall to the guests and to the API, and offer the provisioners',
                      SRC / 'plugins/incus/tasks/server.yml')['ansible.builtin.set_fact']['devbox_offer']
        self.assertEqual(offer['settings'], {'server': 'https://{address}:{{ devbox_incus_port }}'})
        self.assertIn('incus_server_cert', offer['secrets'])
        self.assertEqual(offer['aliases']['server'], ['https://incus-host:{{ devbox_incus_port }}'])

    def test_incus_never_makes_a_pool_on_a_disk(self):
        steps = tasks(SRC / 'plugins/incus/tasks/server.yml')
        check = next(step for step in steps if step['name'] == 'Check the Incus settings')
        self.assertIn("not devbox_incus.pool_source.startswith('/dev/')", check['ansible.builtin.assert']['that'])
        init = next(step for step in steps if 'incus admin init' in str(step.get('ansible.builtin.command', '')))
        # Only an Incus without any storage pool is set up; one that has a pool is kept.
        self.assertEqual(init['when'], 'devbox_incus_pools | length == 0')
        defaults = next(step for step in steps if step['name'] == 'Take the Incus settings of the profile')
        self.assertIn("default('dir')", defaults['ansible.builtin.set_fact']['devbox_incus']['pool_driver'])

    def test_docker_keeps_an_engine_that_is_installed(self):
        steps = tasks(SRC / 'plugins/docker/tasks/engine.yml')
        install = next(step for step in steps if step['name'] == "Install Docker from Ubuntu's packages")
        self.assertEqual(install['when'], 'devbox_docker_found.rc != 0')
        offer = next(step for step in steps if step['name'] == 'Offer the provisioner of this engine')
        self.assertIn("'ssh://{user}@{address}:{port}'", offer['ansible.builtin.set_fact']['devbox_offer']['settings']['host'])


FAKE_UFW = r'''#!/usr/bin/env python3
"""A ufw that keeps its rules in a file, as `ufw show added` lists them."""
import json, os, sys
state = os.environ['FAKE_UFW']
data = json.load(open(state)) if os.path.exists(state) else {'rules': [], 'active': False, 'calls': []}
args = sys.argv[1:]
data['calls'].append(args)
words = ' '.join(args)
if args[:2] == ['show', 'added']:
    print("Added user rules (see 'ufw status' for running firewall):")
    for rule in data['rules']:
        print('ufw ' + rule)
elif args[:1] == ['status']:
    print('Status: active' if data['active'] else 'Status: inactive')
    if data['active']:
        print('Default: deny (incoming), allow (outgoing), deny (routed)')
elif args[:2] == ['--force', 'enable']:
    data['active'] = True
elif args[:1] == ['default']:
    pass
elif args[:2] == ['route', 'delete']:
    rule = 'route ' + ' '.join(args[2:])
    data['rules'] = [known for known in data['rules'] if not known.startswith(rule + ' comment') and known != rule]
elif args[:1] == ['delete']:
    if args[1] == 'route':
        print('ERROR: Invalid syntax', file=sys.stderr)
        sys.exit(1)
    rule = ' '.join(args[1:])
    data['rules'] = [known for known in data['rules'] if not known.startswith(rule + ' comment') and known != rule]
else:
    # Real ufw lists a comment in quotes.
    if 'comment' in args:
        at = args.index('comment')
        words = ' '.join(args[:at]) + " comment '" + ' '.join(args[at + 1:]) + "'"
    if words not in data['rules']:
        data['rules'].append(words)
json.dump(data, open(state, 'w'))
'''


class Firewall(unittest.TestCase):
    def setUp(self):
        self.scratch = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.scratch)
        write_program(self.scratch / 'ufw', FAKE_UFW)
        self.state = self.scratch / 'ufw.json'

    def run_firewall(self, *rules):
        env = dict(os.environ, DEVBOX_UFW=str(self.scratch / 'ufw'), FAKE_UFW=str(self.state))
        done = subprocess.run(['bash', str(FIREWALL)], input='\n'.join(rules) + '\n', capture_output=True,
                              text=True, env=env, timeout=60)
        self.assertEqual(done.returncode, 0, done.stderr)
        return done.stdout

    def kept(self):
        return json.loads(self.state.read_text())

    def test_rules_go_in_before_the_firewall_is_switched_on_and_once(self):
        said = self.run_firewall('allow 22/tcp', 'allow from 192.0.2.7 to any port 22 proto tcp', 'allow in on incusbr0')
        self.assertIn('added: allow 22/tcp', said)
        self.assertIn('switched the firewall on', said)
        calls = self.kept()['calls']
        enabled = calls.index(['--force', 'enable'])
        added = calls.index(['allow', '22/tcp', 'comment', 'berth'])
        self.assertLess(added, enabled)
        self.assertEqual(self.run_firewall('allow 22/tcp', 'allow from 192.0.2.7 to any port 22 proto tcp',
                                           'allow in on incusbr0'), '')

    def test_a_rule_no_longer_asked_for_goes_and_one_by_hand_stays(self):
        self.run_firewall('allow 22/tcp', 'allow in on incusbr0', 'route allow in on incusbr0', 'route allow out on incusbr0')
        data = self.kept()
        data['rules'].append('allow 80/tcp')
        self.state.write_text(json.dumps(data))
        said = self.run_firewall('allow 22/tcp')
        self.assertIn('removed: route allow in on incusbr0', said)
        self.assertEqual(self.kept()['rules'], ["allow 22/tcp comment 'berth'", 'allow 80/tcp'])

    def test_ssh_stays_whatever_ufw_makes_of_a_rule(self):
        # One ufw lists otherwise than it was given is deleted as stale, and goes back in.
        self.state.write_text(json.dumps({'rules': ["allow 22 comment 'berth'"], 'active': True, 'calls': []}))
        self.run_firewall('allow 22/tcp')
        self.assertIn("allow 22/tcp comment 'berth'", self.kept()['rules'])


FAKE_DOCKER = r'''#!/usr/bin/env python3
import json, os, sys
state = os.environ['FAKE_DOCKER']
data = json.load(open(state))
args = sys.argv[1:]
data.setdefault('calls', []).append(args)
if '--env-file' in args:
    data['env_file'] = open(args[args.index('--env-file') + 1]).read()
    data['env_mode'] = oct(os.stat(args[args.index('--env-file') + 1]).st_mode & 0o777)
def save():
    json.dump(data, open(state, 'w'))
containers, images = data['containers'], data['images']
VALUED = {'--name', '--restart', '--network', '--hostname', '--domainname', '--publish', '--volume',
          '--group-add', '--add-host', '--memory', '--cpus', '--shm-size', '--log-driver', '--log-opt',
          '--stop-signal', '--stop-timeout', '--user', '--label', '--env-file'}
def made(args):
    # A container as docker inspect shows one that `docker run ARGS` made.
    given, rest = {}, list(args)
    while rest and rest[0].startswith('--'):
        flag = rest.pop(0)
        given.setdefault(flag, []).append(rest.pop(0) if flag in VALUED else True)
    ref, command = rest[0], rest[1:]
    image = images[ref]
    one = lambda flag, default=None: given.get(flag, [default])[-1]
    env = list(image['Config'].get('Env') or [])
    if '--env-file' in given:
        env += [line for line in open(one('--env-file')).read().splitlines() if line]
    ports = {}
    for published in given.get('--publish', []):
        parts = published.rsplit(':', 2)
        ports.setdefault(parts[-1] + '/tcp', []).append({'HostIp': parts[0] if len(parts) == 3 else '',
                                                        'HostPort': parts[-2] if len(parts) > 1 else ''})
    mounts = []
    for volume in given.get('--volume', []):
        source, target, *mode = volume.split(':')
        mounts.append({'Type': 'bind' if source.startswith('/') else 'volume', 'Source': source, 'Name': source,
                       'Destination': target, 'Mode': ','.join(mode), 'RW': 'ro' not in mode})
    restart = (one('--restart', 'no') or 'no').split(':')
    return {
        'Id': 'f00d' + str(len(data['calls'])).rjust(12, '0'), 'Name': '/' + one('--name'), 'Image': image['Id'],
        'State': {'Running': data.get('new_runs', True)},
        'Config': {'Image': ref, 'Hostname': 'f00d' + str(len(data['calls'])).rjust(8, '0'), 'Env': env,
                   'Cmd': command or image['Config'].get('Cmd'), 'Entrypoint': image['Config'].get('Entrypoint'),
                   'User': one('--user', image['Config'].get('User') or ''),
                   'WorkingDir': image['Config'].get('WorkingDir') or '', 'Labels': {}},
        'HostConfig': {'RestartPolicy': {'Name': restart[0], 'MaximumRetryCount': int(restart[1]) if len(restart) > 1 else 0},
                       'NetworkMode': one('--network', 'bridge'), 'PortBindings': ports,
                       'GroupAdd': given.get('--group-add') or None, 'ExtraHosts': given.get('--add-host') or None,
                       'LogConfig': {'Type': one('--log-driver', 'json-file'),
                                     'Config': dict(opt.split('=', 1) for opt in given.get('--log-opt', []))},
                       'Runtime': 'runc', 'IpcMode': 'private', 'ShmSize': 67108864, 'CgroupnsMode': 'private',
                       'MaskedPaths': ['/proc/acpi', '/proc/kcore']},
        'NetworkSettings': {'Networks': {one('--network', 'bridge'): {}}},
        'Mounts': mounts,
    }
if args[:2] == ['container', 'inspect'] and args[2] == '--format':
    found = containers.get(args[-1])
    save()
    print('true' if found and found['State']['Running'] else 'false')
    sys.exit(0 if found else 1)
if args[:2] == ['container', 'inspect']:
    found = containers.get(args[2])
    save()
    if not found:
        print('Error: No such container', file=sys.stderr); sys.exit(1)
    print(json.dumps([found])); sys.exit(0)
if args[:2] == ['image', 'inspect']:
    save()
    if args[2] not in images:
        print('Error: No such image', file=sys.stderr); sys.exit(1)
    print(json.dumps([images[args[2]]])); sys.exit(0)
if args[:1] == ['pull']:
    images[args[1]] = images[images['new']['Id']] = images['new']
    save(); sys.exit(0)
if args[:2] == ['container', 'rename']:
    containers[args[3]] = containers.pop(args[2])
if args[:2] == ['container', 'stop']:
    containers[args[2]]['State']['Running'] = False
if args[:2] in (['container', 'start'], ['start']):
    containers[args[-1]]['State']['Running'] = True
if args[:2] == ['container', 'rm']:
    containers.pop(args[-1], None)
if args[:1] == ['run']:
    container = made(args[1:])
    containers[container['Name'][1:]] = container
if args[:1] == ['exec'] and args[-3:] == ['berth', 'server', 'setup-key']:
    print('berth-setup-made-up-key')
if args[:1] == ['exec'] and args[-3:] == ['berth', 'key', 'show']:
    print('ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMadeUpMadeUpMadeUpMadeUpMadeUpMadeUpMadeUp berth')
save()
'''

# The berth server's container that the incus-host recipe of berth 0.5.2 ran,
# as docker inspect shows it on a daemon with log options in daemon.json.
FOUND_052 = json.loads((SRC / 'tests/fixtures/berth-server-0.5.2.json').read_text())


def container(**host):
    """The berth server's container as the recipe ran it, with what a real one has besides."""
    host_config = {
        'RestartPolicy': {'Name': 'unless-stopped', 'MaximumRetryCount': 0},
        'NetworkMode': 'bridge',
        'PortBindings': {'7777/tcp': [{'HostIp': '127.0.0.1', 'HostPort': '7777'}]},
        'GroupAdd': ['989'],
        'ExtraHosts': ['incus-host:host-gateway'],
        'LogConfig': {'Type': 'json-file', 'Config': {}},
        'Runtime': 'runc',
        'IpcMode': 'private',
    }
    host_config.update(host)
    return {
        'Id': 'abc123def4567890', 'Name': '/berth', 'Image': 'sha256:old',
        'State': {'Running': True},
        'Config': {'Image': 'ghcr.io/helightdev/berth:0.5.2', 'Hostname': 'abc123def456',
                   'Env': ['XDG_CONFIG_HOME=/config', 'BERTH_TOKEN_HINT=s3cret'],
                   'Cmd': ['serve', '--listen', '0.0.0.0:7777', '--expose'], 'Entrypoint': ['tini', '--', 'berth'],
                   'User': 'berth', 'WorkingDir': '/src', 'Labels': {}},
        'HostConfig': host_config,
        'NetworkSettings': {'Networks': {'bridge': {}}},
        'Mounts': [
            {'Type': 'bind', 'Source': '/srv/berth/config', 'Destination': '/config', 'RW': True},
            {'Type': 'bind', 'Source': '/srv/berth/data', 'Destination': '/data', 'RW': True},
            {'Type': 'bind', 'Source': '/srv/berth/ssh', 'Destination': '/home/berth/.ssh', 'RW': True},
            {'Type': 'bind', 'Source': '/var/run/docker.sock', 'Destination': '/var/run/docker.sock', 'RW': True},
        ],
    }


def image(version, image_id):
    return {'Id': image_id, 'Config': {
        'Env': ['XDG_CONFIG_HOME=/config'], 'Cmd': ['serve', '--listen', '0.0.0.0:7777', '--expose'],
        'Entrypoint': ['tini', '--', 'berth'], 'User': 'berth', 'WorkingDir': '/src',
        'Labels': {'org.opencontainers.image.version': version}}}


class ServerUpgrade(unittest.TestCase):
    def setUp(self):
        self.scratch = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.scratch)
        write_program(self.scratch / 'docker', FAKE_DOCKER)
        self.state = self.scratch / 'docker.json'

    def helper(self, *args, found=None, old_image=None, new='0.6.0', new_runs=True, images=None, keep=False):
        if not keep:
            found = found or container()
            old_image = old_image or image('0.5.2', 'sha256:old')
            self.state.write_text(json.dumps({
                'containers': {'berth': found},
                'images': dict({found['Image']: old_image, found['Config']['Image']: old_image,
                                'new': image(new, 'sha256:new')}, **(images or {})),
                'new_runs': new_runs}))
        else:
            data = json.loads(self.state.read_text())
            data['calls'] = []
            self.state.write_text(json.dumps(data))
        env = dict(os.environ, DEVBOX_DOCKER=str(self.scratch / 'docker'), FAKE_DOCKER=str(self.state),
                   DEVBOX_UPGRADE_SETTLE='0', BERTH_ARG_VERSION=new)
        done = subprocess.run([sys.executable, str(UPGRADE), 'berth', *args], capture_output=True, text=True,
                              env=env, timeout=60)
        return done, json.loads(self.state.read_text())

    def upgrade(self, version, found=None, new_runs=True):
        return self.helper(found=found, new=version, new_runs=new_runs)

    @staticmethod
    def run_call(data):
        return next(call for call in data['calls'] if call[0] == 'run')

    def test_the_new_container_keeps_the_data_and_the_options(self):
        done, data = self.upgrade('0.6.0')
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn('berth runs ghcr.io/helightdev/berth:0.6.0 now', done.stdout)
        run = self.run_call(data)
        words = ' '.join(run)
        for kept in ('--name berth', '--restart unless-stopped', '--publish 127.0.0.1:7777:7777',
                     '--volume /srv/berth/data:/data', '--volume /srv/berth/ssh:/home/berth/.ssh',
                     '--volume /var/run/docker.sock:/var/run/docker.sock', '--group-add 989',
                     '--add-host incus-host:host-gateway'):
            self.assertIn(kept, words)
        self.assertEqual(run[-1], 'ghcr.io/helightdev/berth:0.6.0')
        # The environment the image does not set travels in a private file, not in argv.
        self.assertNotIn('s3cret', words)
        self.assertEqual((data['env_file'], data['env_mode']), ('BERTH_TOKEN_HINT=s3cret\n', '0o600'))
        self.assertEqual(sorted(data['containers']), ['berth'])
        self.assertIn(['pull', 'ghcr.io/helightdev/berth:0.6.0'], data['calls'])

    def test_a_container_of_0_5_2_on_a_daemon_with_log_options_is_carried_over(self):
        found, old = FOUND_052['container'], FOUND_052['image']
        new = dict(old, Id='sha256:new', Config=dict(old['Config'], Cmd=['serve', '--listen', '0.0.0.0:7777', '--expose']))
        done, data = self.helper(found=found, old_image=old, images={'new': new})
        self.assertEqual(done.returncode, 0, done.stderr)
        run = self.run_call(data)
        words = ' '.join(run)
        for kept in ('--log-driver json-file', '--log-opt max-file=5', '--log-opt max-size=10m',
                     '--add-host incus-host:host-gateway', '--group-add 989',
                     '--volume /var/run/docker.sock:/var/run/docker.sock', '--volume /srv/berth/config:/config',
                     '--publish 127.0.0.1:7777:7777', '--restart unless-stopped'):
            self.assertIn(kept, words)
        # What docker fills in for every container is not an option of this one.
        for left in ('--shm-size', '--hostname', '--user', '--privileged', 'MaskedPaths'):
            self.assertNotIn(left, words)
        # Its command was its image's, so the new image's own runs: no --web of 0.5.2.
        self.assertEqual(run[-1], 'ghcr.io/helightdev/berth:0.6.0')
        self.assertEqual(data['env_file'], '')

    def test_an_option_it_cannot_carry_over_changes_nothing(self):
        done, data = self.upgrade('0.6.0', found=container(Privileged=True, Devices=[{'PathOnHost': '/dev/kvm'}]))
        self.assertNotEqual(done.returncode, 0)
        self.assertIn('it runs privileged; it has devices', done.stderr)
        self.assertFalse([call for call in data['calls'] if call[0] in ('pull', 'run', 'container') and call[1:2] != ['inspect']])
        for host in ({'ReadonlyRootfs': True}, {'DeviceRequests': [{'Count': -1}]}, {'Runtime': 'sysbox-runc'},
                     {'AutoRemove': True}):
            done, _ = self.upgrade('0.6.0', found=container(**host))
            self.assertNotEqual(done.returncode, 0, host)
            self.assertIn('Upgrade it by hand', done.stderr)

    def test_an_older_version_is_refused(self):
        done, data = self.upgrade('0.5.1')
        self.assertNotEqual(done.returncode, 0)
        self.assertIn('runs berth 0.5.2, which is newer than 0.5.1', done.stderr)
        self.assertNotIn('pull', [call[0] for call in data['calls']])
        done, _ = self.upgrade('latest')
        self.assertIn("'latest' is not a version of berth", done.stderr)

    def test_the_old_container_runs_again_when_the_new_one_stops(self):
        done, data = self.upgrade('0.6.0', new_runs=False)
        self.assertNotEqual(done.returncode, 0)
        self.assertIn('so berth runs ghcr.io/helightdev/berth:0.5.2 again', done.stderr)
        self.assertEqual(sorted(data['containers']), ['berth'])
        self.assertTrue(data['containers']['berth']['State']['Running'])
        self.assertIn(['container', 'rename', 'berth-before-0.6.0', 'berth'], data['calls'])

    def test_the_recipe_sets_its_options_over_the_containers_and_keeps_the_others(self):
        asked = ['--publish', '127.0.0.1:7878:7777', '--volume', '/srv/berth/data:/data',
                 '--add-host', 'node:host-gateway', '--add-host', 'incus-host:host-gateway', '--group-add', '989']
        done, data = self.helper('--image', 'ghcr.io/helightdev/berth', '--pull', *asked)
        self.assertEqual(done.returncode, 0, done.stderr)
        run = self.run_call(data)
        words = ' '.join(run)
        self.assertIn('--publish 127.0.0.1:7878:7777', words)
        self.assertNotIn('127.0.0.1:7777:7777', words)
        self.assertEqual(words.count('--add-host incus-host:host-gateway'), 1)
        self.assertEqual(words.count('--group-add 989'), 1)
        for kept in ('--add-host node:host-gateway', '--volume /var/run/docker.sock:/var/run/docker.sock'):
            self.assertIn(kept, words)
        self.assertEqual(run[-1], 'ghcr.io/helightdev/berth')
        # Run again with the same, it changes nothing.
        done, data = self.helper('--image', 'ghcr.io/helightdev/berth', '--pull', *asked, keep=True)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn('berth is unchanged', done.stdout)
        self.assertNotIn('run', [call[0] for call in data['calls']])

    def test_without_an_update_the_recipe_keeps_the_image(self):
        done, data = self.helper('--image', 'ghcr.io/helightdev/berth', '--keep-image',
                                 '--publish', '127.0.0.1:7777:7777')
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn('berth is unchanged', done.stdout)
        self.assertNotIn('pull', [call[0] for call in data['calls']])
        done, data = self.helper('--image', 'ghcr.io/helightdev/berth', '--keep-image',
                                 '--add-host', 'node:host-gateway')
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.run_call(data)[-1], 'ghcr.io/helightdev/berth:0.5.2')
        # A tag pulled again since is not taken up without an update.
        done, data = self.helper('--image', 'x', '--keep-image', '--add-host', 'node:host-gateway',
                                 images={'ghcr.io/helightdev/berth:0.5.2': image('0.6.0', 'sha256:new')})
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.run_call(data)[-1], 'sha256:old')

    def test_the_recipe_never_goes_back_to_an_older_server(self):
        done, data = self.helper('--image', 'ghcr.io/helightdev/berth:0.5.1', '--pull', new='0.5.1')
        self.assertNotEqual(done.returncode, 0)
        self.assertIn('which is newer than 0.5.1', done.stderr)
        self.assertNotIn('run', [call[0] for call in data['calls']])


if __name__ == '__main__':
    unittest.main()
