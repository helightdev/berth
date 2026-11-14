"""The Docker provisioner, its machines and its Compose deployments, run against a fake docker executable."""
import json
import os
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

PLUGIN = Path(__file__).resolve().parents[1] / 'plugins/docker'
KEY = 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleKeyForTestsOnly berth'

# Answers by the start of the argument list. Each rule has results, [code, stdout, stderr],
# used in turn with the last one repeating. A call that matches no rule succeeds quietly.
FAKE = r'''#!/usr/bin/env python3
import json, os, stat, sys
state = os.environ['FAKE_DIR']
args = sys.argv[1:]
pairs = [args[i:i + 2] for i in range(len(args) - 1)]
data = sys.stdin.read() if '-i' in args or ['-f', '-'] in pairs else ''
# Files given with -f other than stdin, with their mode, as compose would read them.
files = {pair[1]: {'text': open(pair[1]).read(), 'mode': stat.S_IMODE(os.stat(pair[1]).st_mode)}
         for pair in pairs if pair[0] == '-f' and pair[1] != '-'}
with open(state + '/calls.jsonl', 'a') as log:
    log.write(json.dumps({'args': args, 'stdin': data, 'DOCKER_HOST': os.environ.get('DOCKER_HOST'),
                          'env': dict(os.environ), 'cwd': os.getcwd(), 'files': files}) + '\n')
rules = json.load(open(state + '/rules.json'))
for number, rule in enumerate(rules):
    if args[:len(rule['prefix'])] == rule['prefix']:
        seen = state + '/seen-%d' % number
        used = int(open(seen).read()) if os.path.exists(seen) else 0
        open(seen, 'w').write(str(used + 1))
        code, out, err = rule['results'][min(used, len(rule['results']) - 1)]
        sys.stdout.write(out)
        sys.stderr.write(err)
        sys.exit(code)
'''

MISSING = [1, '', 'Error response from daemon: No such container: x']


def inspect(labels=None, running=True, ip='172.17.0.2'):
    return [0, json.dumps([{
        'Config': {'Labels': labels or {}},
        'State': {'Running': running, 'Status': 'running' if running else 'exited'},
        'NetworkSettings': {'Networks': {'bridge': {'IPAddress': ip}}},
    }]), '']


OURS = {'berth-host': 'studio', 'berth-instance': 'tok-1'}

SPEC = 'services:\n  web:\n    image: example/web:1\n    environment:\n      DB_PASSWORD: ${DB_PASSWORD}\n  db:\n    image: example/db:2\n'
SECRET = 'hunter2-made-up-for-tests'
COMPOSE = ['compose', '-p', 'shop']
BERTH_LABELS = 'com.docker.compose.project=shop,berth-resource=shop,berth-token=tok-9'


def ps(*services, labels=BERTH_LABELS, lines=False):
    """What docker compose ps --format json prints for (service, state) pairs."""
    rows = [{'Name': f'shop-{name}-1', 'Service': name, 'State': state, 'Labels': labels,
             'Status': 'Up 2 minutes' if state == 'running' else 'Exited (0) 1 minute ago'}
            for name, state in services]
    return [0, ''.join(json.dumps(row) + '\n' for row in rows) if lines else json.dumps(rows), '']


class Harness(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.state = root / 'state'
        self.state.mkdir()
        bin_dir = root / 'bin'
        bin_dir.mkdir()
        fake = bin_dir / 'docker'
        fake.write_text(FAKE)
        fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
        self.path = f'{bin_dir}:{os.environ["PATH"]}'
        self.rules = []

    def run_verb(self, verb, extra=None, arguments=None, rules=None, path=None):
        (self.state / 'rules.json').write_text(json.dumps(rules or []))
        request = {'version': 2, 'verb': verb, 'host': 'studio', 'provisioner': 'container',
                   'arguments': {'host': '', 'image': '', 'ssh_port': '', 'publish_address': '127.0.0.1',
                                 'volume': '', 'keep_volume': 'no'},
                   'secrets': {}}
        request['arguments'].update(arguments or {})
        request.update(extra or {})
        env = dict(os.environ, PATH=path or self.path, FAKE_DIR=str(self.state))
        # The endpoint is the request's, not that of whoever runs the tests.
        env.pop('DOCKER_HOST', None)
        done = subprocess.run([sys.executable, str(PLUGIN / 'bin/provision'), verb], input=json.dumps(request),
                              text=True, capture_output=True, env=env, cwd=PLUGIN)
        return done

    def calls(self):
        path = self.state / 'calls.jsonl'
        if not path.exists():
            return []
        return [json.loads(line) for line in path.read_text().splitlines()]

    def argv(self, *prefix):
        return [c['args'] for c in self.calls() if c['args'][:len(prefix)] == list(prefix)]

    def create_request(self, **extra):
        request = {'token': 'tok-1', 'labels': dict(OURS),
                   'access': {'user': 'ops', 'public_keys': [KEY]}}
        request.update(extra)
        return request

    def fresh_rules(self):
        return [
            {'prefix': ['container', 'inspect'], 'results': [MISSING, inspect(OURS)]},
            {'prefix': ['image', 'inspect'], 'results': [[0, '[]', '']]},
            {'prefix': ['volume', 'inspect'], 'results': [[1, '', 'Error: No such volume']]},
        ]


class Provision(Harness):
    def test_manifest_has_provisioner_and_no_features(self):
        package = json.loads((PLUGIN / 'package.json').read_text())
        self.assertNotIn('features', package['berth'])
        [entry] = package['berth']['provisioners']
        self.assertEqual((entry['id'], entry['protocol'], entry['command']), ('container', 2, 'bin/provision'))
        self.assertEqual(entry['verbs'], ['list'])
        self.assertNotIn('secrets', entry)
        self.assertEqual([a['name'] for a in entry['arguments']], ['host'])
        machine, deployment, instance = entry['kinds']
        self.assertEqual((instance['id'], instance['verbs']), ('instance', ['start', 'stop', 'change', 'logs']))
        self.assertEqual(instance['arguments'][0]['name'], 'image')
        self.assertEqual((machine['id'], machine['setup']), ('machine', 'apply'))
        self.assertEqual(machine['verbs'], ['start', 'stop', 'change', 'logs'])
        self.assertEqual([a['name'] for a in machine['arguments']],
                         ['image', 'runtime', 'ssh_port', 'publish_address', 'volume', 'cpus', 'memory', 'keep_volume'])
        # Every action the manifest declares is one the program answers, and what changes asks first.
        source = (PLUGIN / 'bin/provision').read_text()
        for action in entry['actions']:
            self.assertIn(f"'{action['name']}': act_", source)
        self.assertEqual([a['name'] for a in entry['actions'] if a.get('changes', True)],
                         ['create-volume', 'remove-volume', 'pull-image', 'remove-image', 'build', 'prune'])
        prune = [a for a in entry['actions'] if a['name'] == 'prune'][0]
        self.assertTrue(prune['confirm'])
        self.assertTrue(os.access(PLUGIN / 'bin/provision', os.X_OK))
        self.assertNotIn('actions', machine)
        self.assertEqual((deployment['id'], deployment['label']), ('deployment', 'Compose project'))
        self.assertEqual(deployment['spec'], {'format': 'compose', 'label': 'Compose file'})
        self.assertEqual(deployment['verbs'], ['start', 'stop', 'change', 'logs'])
        [keep] = deployment['arguments']
        self.assertEqual((keep['name'], keep['default'], keep['values']), ('keep_volumes', 'no', ['no', 'yes']))
        self.assertEqual([a['name'] for a in deployment['actions']], ['services', 'restart', 'pull'])
        for action in deployment['actions']:
            self.assertIn(f"'{action['name']}': act_", source)
            self.assertTrue(action['label'] and action['group'] and action['help'])
            self.assertEqual(bool(action.get('confirm')), action.get('changes', True), action['name'])

    def test_create_runs_systemd_container_with_labels(self):
        done = self.run_verb('create', self.create_request(), rules=self.fresh_rules())
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(json.loads(done.stdout), {'id': 'berth-studio', 'address': '172.17.0.2', 'port': 22})
        [run] = self.argv('run')
        for flag in (['--name', 'berth-studio'], ['--restart', 'unless-stopped'],
                     ['--label', 'berth-host=studio'], ['--label', 'berth-instance=tok-1'],
                     ['--volume', 'berth-studio-data:/srv/agent'], ['--stop-signal', 'SIGRTMIN+3'],
                     ['--cgroupns', 'private'], ['--tmpfs', '/run']):
            self.assertIn(flag, [run[i:i + 2] for i in range(len(run) - 1)], flag)
        self.assertIn('--privileged', run)
        self.assertNotIn('--publish', run)
        self.assertEqual(run[-2:], ['berth-docker-host:0.2.0', '/sbin/init'])
        self.assertEqual(self.argv('info'), [])
        self.assertEqual(self.argv('volume', 'create')[0][-2:], ['berth-host=studio', 'berth-studio-data'])

    def test_keys_travel_on_stdin_only(self):
        done = self.run_verb('create', self.create_request(), rules=self.fresh_rules())
        self.assertEqual(done.returncode, 0, done.stderr)
        for call in self.calls():
            self.assertNotIn('AAAAC3', ' '.join(call['args']))
        [setup] = [c for c in self.calls() if c['args'][:2] == ['exec', '-i']]
        self.assertEqual(setup['stdin'].strip(), KEY)
        self.assertEqual(setup['args'][-1], 'ops')

    def test_published_port_and_custom_endpoint(self):
        done = self.run_verb('create', self.create_request(), rules=self.fresh_rules(), arguments={
            'ssh_port': '2222', 'publish_address': '192.0.2.5', 'host': 'ssh://192.0.2.9',
            'image': 'example/host:1', 'volume': 'studio-vol'})
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(json.loads(done.stdout), {'id': 'berth-studio', 'address': '192.0.2.5', 'port': 2222})
        [run] = self.argv('run')
        self.assertIn('0.0.0.0:2222:22', run)
        self.assertEqual(run[-2:], ['example/host:1', '/sbin/init'])
        self.assertIn('studio-vol:/srv/agent', run)
        self.assertEqual({c['DOCKER_HOST'] for c in self.calls()}, {'ssh://192.0.2.9'})
        self.assertEqual(self.argv('build'), [])

    def test_loopback_publish_binds_loopback(self):
        done = self.run_verb('create', self.create_request(), rules=self.fresh_rules(),
                             arguments={'ssh_port': '2200'})
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn('127.0.0.1:2200:22', self.argv('run')[0])

    def test_builds_the_bundled_image_when_missing(self):
        rules = self.fresh_rules()
        rules[1] = {'prefix': ['image', 'inspect'], 'results': [[1, '', 'No such image']]}
        done = self.run_verb('create', self.create_request(), rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        [build] = self.argv('build')
        self.assertEqual(build[:3], ['build', '--tag', 'berth-docker-host:0.2.0'])

    def test_create_is_idempotent(self):
        rules = [{'prefix': ['container', 'inspect'], 'results': [inspect(OURS, running=False), inspect(OURS)]}]
        done = self.run_verb('create', self.create_request(), rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(json.loads(done.stdout)['id'], 'berth-studio')
        self.assertEqual(self.argv('start'), [['start', 'berth-studio']])
        self.assertEqual(self.argv('run'), [])

    def test_create_refuses_a_name_taken_by_a_stranger(self):
        for labels in ({}, {'berth-host': 'studio', 'berth-instance': 'other'}):
            with self.subTest(labels=labels):
                done = self.run_verb('create', self.create_request(), rules=[
                    {'prefix': ['container', 'inspect'], 'results': [inspect(labels)]}])
                self.assertEqual(done.returncode, 1)
                self.assertIn('berth-studio', done.stderr)
                self.assertEqual(done.stdout, '')
                self.assertEqual(self.argv('run'), [])

    def test_status_mapping(self):
        cases = [(inspect(OURS), {'state': 'running', 'address': '172.17.0.2'}),
                 (inspect(OURS, running=False), {'state': 'stopped'}),
                 (MISSING, {'state': 'gone'})]
        for result, expected in cases:
            with self.subTest(expected=expected):
                done = self.run_verb('status', {'id': 'berth-studio'},
                                     rules=[{'prefix': ['container', 'inspect'], 'results': [result]}])
                self.assertEqual(done.returncode, 0, done.stderr)
                self.assertEqual(json.loads(done.stdout), expected)

    def test_status_with_published_port_answers_the_publish_address(self):
        done = self.run_verb('status', {'id': 'berth-studio'}, arguments={'ssh_port': '2222', 'publish_address': '192.0.2.5'},
                             rules=[{'prefix': ['container', 'inspect'], 'results': [inspect(OURS)]}])
        self.assertEqual(json.loads(done.stdout), {'state': 'running', 'address': '192.0.2.5'})

    def test_logs_without_follow_are_the_latest_console_lines(self):
        console = ''.join(f'[  OK  ] Started unit-{n}.service.\n' for n in range(100))
        rules = [{'prefix': ['container', 'inspect'], 'results': [inspect(OURS)]},
                 {'prefix': ['logs'], 'results': [[0, console, '']]}]
        done = self.run_verb('logs', {'id': 'berth-studio'}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout, console)
        self.assertEqual(self.argv('logs'), [['logs', '--tail', '100', 'berth-studio']])

    def test_logs_read_the_journal_when_the_console_is_empty(self):
        rules = [{'prefix': ['container', 'inspect'], 'results': [inspect(OURS)]},
                 {'prefix': ['logs'], 'results': [[0, '', '']]},
                 {'prefix': ['exec', 'berth-studio', 'journalctl'],
                  'results': [[0, 'Started ssh.service - OpenBSD Secure Shell server.\n', '']]}]
        done = self.run_verb('logs', {'id': 'berth-studio', 'tail': 40}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout, 'Started ssh.service - OpenBSD Secure Shell server.\n')
        [journal] = self.argv('exec', 'berth-studio', 'journalctl')
        self.assertEqual(journal[journal.index('--lines') + 1], '40')
        # A container without a journal has no lines, which is no failure.
        rules[2]['results'] = [[1, '', 'OCI runtime exec failed']]
        (self.state / 'seen-2').unlink(missing_ok=True)
        done = self.run_verb('logs', {'id': 'berth-studio'}, rules=rules)
        self.assertEqual((done.returncode, done.stdout), (0, ''))

    def test_logs_of_a_container_that_is_gone_say_so(self):
        done = self.run_verb('logs', {'id': 'berth-studio'},
                             rules=[{'prefix': ['container', 'inspect'], 'results': [MISSING]}])
        self.assertEqual(done.returncode, 1)
        self.assertIn('does not exist', done.stderr)
        self.assertEqual(self.argv('logs'), [])

    def test_start_and_stop(self):
        rules = [{'prefix': ['container', 'inspect'], 'results': [inspect(OURS)]}]
        for verb in ('start', 'stop'):
            done = self.run_verb(verb, {'id': 'berth-studio'}, rules=rules)
            self.assertEqual(done.returncode, 0, done.stderr)
            self.assertEqual(json.loads(done.stdout), {})
        self.assertEqual(len(self.argv('start')), 1)
        self.assertEqual(self.argv('stop')[0][-1], 'berth-studio')

    def test_destroy_removes_container_and_volume(self):
        rules = [{'prefix': ['container', 'inspect'], 'results': [inspect(OURS)]},
                 {'prefix': ['volume', 'inspect'], 'results': [[0, json.dumps([{'Labels': {'berth-host': 'studio'}}]), '']]}]
        done = self.run_verb('destroy', {'id': 'berth-studio'}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(json.loads(done.stdout), {})
        self.assertEqual(self.argv('rm')[0][-1], 'berth-studio')
        self.assertEqual(self.argv('volume', 'rm'), [['volume', 'rm', 'berth-studio-data']])

    def test_destroy_keeps_the_volume_when_asked(self):
        rules = [{'prefix': ['container', 'inspect'], 'results': [inspect(OURS)]}]
        done = self.run_verb('destroy', {'id': 'berth-studio'}, arguments={'keep_volume': 'yes'}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(len(self.argv('rm')), 1)
        self.assertEqual(self.argv('volume'), [])

    def test_destroy_keeps_a_volume_berth_did_not_make(self):
        rules = [{'prefix': ['container', 'inspect'], 'results': [inspect(OURS)]},
                 {'prefix': ['volume', 'inspect'], 'results': [[0, json.dumps([{'Labels': None}]), '']]}]
        done = self.run_verb('destroy', {'id': 'berth-studio'}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.argv('volume', 'rm'), [])

    def test_destroy_when_gone_succeeds(self):
        rules = [{'prefix': ['container', 'inspect'], 'results': [MISSING]},
                 {'prefix': ['volume', 'inspect'], 'results': [[1, '', 'No such volume']]}]
        done = self.run_verb('destroy', {'id': 'berth-studio'}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.argv('rm'), [])

    def test_destroy_refuses_an_unlabelled_container(self):
        done = self.run_verb('destroy', {'id': 'berth-studio'},
                             rules=[{'prefix': ['container', 'inspect'], 'results': [inspect({})]}])
        self.assertEqual(done.returncode, 1)
        self.assertIn('not created by berth', done.stderr)
        self.assertEqual(self.argv('rm'), [])

    def test_list_shows_labelled_containers(self):
        rules = [{'prefix': ['ps', '--all', '--filter', 'label=berth-resource'], 'results': [[0, '', '']]},
                 {'prefix': ['ps'], 'results': [[0, 'berth-studio\nberth-studio-2\n', '']]},
                 {'prefix': ['container', 'inspect', 'berth-studio'],
                  'results': [inspect({'berth-host': 'studio'})]},
                 {'prefix': ['container', 'inspect', 'berth-studio-2'],
                  'results': [inspect({'berth-host': 'studio-2'}, running=False)]}]
        done = self.run_verb('list', rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(json.loads(done.stdout), {'resources': [
            {'id': 'berth-studio', 'kind': 'machine', 'name': 'studio', 'host': 'studio', 'state': 'running'},
            {'id': 'berth-studio-2', 'kind': 'machine', 'name': 'studio-2', 'host': 'studio-2', 'state': 'stopped'}]})
        self.assertIn('label=berth-host', self.argv('ps')[0])
        done = self.run_verb('list', {'kind': 'deployment'}, rules=rules)
        self.assertEqual(json.loads(done.stdout), {'resources': []})

    def test_sysbox_runs_without_privileges(self):
        rules = self.fresh_rules() + [
            {'prefix': ['info'], 'results': [[0, json.dumps({'runc': {}, 'sysbox-runc': {}}), '']]}]
        done = self.run_verb('create', self.create_request(), rules=rules, arguments={'runtime': 'sysbox'})
        self.assertEqual(done.returncode, 0, done.stderr)
        [run] = self.argv('run')
        self.assertIn(['--runtime', 'sysbox-runc'], [run[i:i + 2] for i in range(len(run) - 1)])
        for flag in ('--privileged', '--cgroupns', '--tmpfs'):
            self.assertNotIn(flag, run)
        self.assertEqual(run[-2:], ['berth-docker-host:0.2.0', '/sbin/init'])

    def test_sysbox_is_refused_before_anything_is_made_where_the_engine_lacks_it(self):
        rules = self.fresh_rules() + [{'prefix': ['info'], 'results': [[0, json.dumps({'runc': {}}), '']]}]
        done = self.run_verb('create', self.create_request(), rules=rules,
                             arguments={'runtime': 'sysbox', 'host': 'ssh://ops@192.0.2.9'})
        self.assertEqual(done.returncode, 1)
        self.assertIn('ssh://ops@192.0.2.9 has no sysbox-runc runtime', done.stderr)
        self.assertIn('Install Sysbox', done.stderr)
        self.assertEqual(self.argv('run') + self.argv('volume', 'create') + self.argv('build'), [])
        done = self.run_verb('create', self.create_request(), rules=rules, arguments={'runtime': 'gvisor'})
        self.assertEqual(done.returncode, 1)
        self.assertIn('privileged or sysbox', done.stderr)

    def test_status_without_an_id_is_the_engine(self):
        version = {'Server': {'Version': '27.1.0', 'ApiVersion': '1.46', 'Os': 'linux', 'Arch': 'amd64'}}
        rules = [{'prefix': ['version'], 'results': [[0, json.dumps(version), '']]},
                 {'prefix': ['info'], 'results': [[0, json.dumps({'runc': {}, 'sysbox-runc': {}}), '']]},
                 {'prefix': ['ps'], 'results': [[0, 'berth-studio\n', '']]}]
        done = self.run_verb('status', rules=rules, arguments={'host': 'ssh://ops@192.0.2.9'})
        self.assertEqual(done.returncode, 0, done.stderr)
        answer = json.loads(done.stdout)
        self.assertEqual(answer['state'], 'ready')
        self.assertEqual(answer['message'], 'Docker 27.1.0 answers at ssh://ops@192.0.2.9')
        self.assertEqual(answer['values'], {'runtime': ['privileged', 'sysbox']})
        rows = {row['label']: row['value'] for row in answer['details']}
        self.assertEqual((rows['Engine'], rows['Runtimes'], rows['Machines berth made']),
                         ('27.1.0', 'runc, sysbox-runc', '1'))
        # Without Sysbox only privileged is offered; one that does not answer says so.
        rules[1] = {'prefix': ['info'], 'results': [[0, json.dumps({'runc': {}}), '']]}
        done = self.run_verb('status', rules=rules)
        self.assertEqual(json.loads(done.stdout)['values'], {'runtime': ['privileged']})
        done = self.run_verb('status', rules=[{'prefix': ['version'], 'results': [
            [1, '', 'Cannot connect to the Docker daemon at unix:///var/run/docker.sock.']]}])
        self.assertEqual(done.returncode, 1)
        self.assertIn('not reachable', done.stderr)

    def test_change_sets_new_limits(self):
        rules = [{'prefix': ['container', 'inspect'], 'results': [inspect(OURS)]}]
        done = self.run_verb('change', {'id': 'berth-studio'}, arguments={'cpus': '2', 'memory': '1GiB'}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(json.loads(done.stdout), {'state': 'running'})
        self.assertEqual(self.argv('update'), [['update', '--cpus', '2', '--memory', str(1024 ** 3),
                                                '--memory-swap', str(2 * 1024 ** 3), 'berth-studio']])
        done = self.run_verb('change', {'id': 'berth-studio'}, rules=[
            {'prefix': ['container', 'inspect'], 'results': [inspect({})]}])
        self.assertEqual(done.returncode, 1)
        self.assertIn('not created by berth', done.stderr)

    def test_logs_are_written_as_they_come(self):
        rules = [{'prefix': ['container', 'inspect'], 'results': [inspect(OURS)]},
                 {'prefix': ['logs'], 'results': [[0, 'booted\nready\n', '']]}]
        done = self.run_verb('logs', {'id': 'berth-studio', 'tail': 20, 'follow': True}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout, 'booted\nready\n')
        self.assertEqual(self.argv('logs'), [['logs', '--tail', '20', 'berth-studio'],
                                             ['logs', '--tail', '20', '--follow', 'berth-studio']])

    def test_actions_answer_with_a_message_and_rows(self):
        images = [{'Repository': 'berth-docker-host', 'Tag': '0.2.0', 'Size': '900MB', 'ID': 'a1'},
                  {'Repository': 'ubuntu', 'Tag': '26.04', 'Size': '80MB', 'ID': 'b2'}]
        rules = [{'prefix': ['image', 'ls'], 'results': [[0, ''.join(json.dumps(i) + '\n' for i in images), '']]}]
        done = self.run_verb('act', {'action': {'name': 'images', 'arguments': {}}}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        answer = json.loads(done.stdout)
        self.assertEqual(answer['message'], '2 images at the local Docker')
        self.assertEqual(answer['details'][1], {'label': 'ubuntu:26.04', 'value': '80MB · b2'})
        done = self.run_verb('act', {'action': {'name': 'dance'}})
        self.assertEqual(done.returncode, 1)
        self.assertIn('no action', done.stderr)

    def test_prune_removes_only_what_berth_left_and_nothing_in_use(self):
        images = [{'Repository': 'berth-docker-host', 'Tag': '0.2.0', 'ID': 'now'},
                  {'Repository': 'berth-docker-host', 'Tag': '0.1.0', 'ID': 'old'},
                  {'Repository': 'berth-docker-host', 'Tag': '0.0.9', 'ID': 'used'}]
        volumes = [{'Name': 'berth-studio-data', 'Labels': 'berth-host=studio'},
                   {'Name': 'berth-attic-data', 'Labels': 'berth-host=attic'}]
        rules = [{'prefix': ['image', 'ls'], 'results': [[0, ''.join(json.dumps(i) + '\n' for i in images), '']]},
                 {'prefix': ['volume', 'ls'], 'results': [[0, ''.join(json.dumps(v) + '\n' for v in volumes), '']]},
                 {'prefix': ['ps', '--all', '--format', '{{.Names}}', '--filter', 'ancestor=used'],
                  'results': [[0, 'someones-container\n', '']]},
                 {'prefix': ['ps', '--all', '--format', '{{.Names}}', '--filter', 'volume=berth-studio-data'],
                  'results': [[0, 'berth-studio\n', '']]}]
        done = self.run_verb('act', {'action': {'name': 'prune', 'arguments': {'volumes': 'no'}}}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.argv('image', 'rm'), [['image', 'rm', 'berth-docker-host:0.1.0']])
        self.assertEqual(self.argv('volume', 'rm'), [])
        self.assertEqual(self.argv('ls'), [])
        done = self.run_verb('act', {'action': {'name': 'prune', 'arguments': {'volumes': 'yes'}}}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.argv('volume', 'rm'), [['volume', 'rm', 'berth-attic-data']])
        self.assertEqual(self.argv('image', 'ls')[0][:3], ['image', 'ls', 'berth-docker-host'])
        self.assertEqual(self.argv('rm') + self.argv('container', 'rm'), [])

    def test_another_version_of_the_protocol_is_refused(self):
        (self.state / 'rules.json').write_text('[]')
        env = dict(os.environ, PATH=self.path, FAKE_DIR=str(self.state))
        done = subprocess.run([sys.executable, str(PLUGIN / 'bin/provision'), 'status'],
                              input=json.dumps({'version': 1, 'id': 'x'}), text=True, capture_output=True,
                              env=env, cwd=PLUGIN)
        self.assertEqual(done.returncode, 1)
        self.assertIn('version 2', done.stderr)
        self.assertEqual(self.calls(), [])

    def test_errors_are_one_line_and_exit_1(self):
        done = self.run_verb('status', {'id': 'berth-studio'}, path='/nonexistent')
        self.assertEqual(done.returncode, 1)
        self.assertEqual(done.stderr.strip().count('\n'), 0)
        self.assertEqual(done.stdout, '')
        done = self.run_verb('status', {'id': 'berth-studio'}, rules=[{'prefix': ['container', 'inspect'], 'results': [
            [1, '', 'Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?']]}])
        self.assertEqual(done.returncode, 1)
        self.assertIn('daemon is not reachable', done.stderr)

    def test_build_failure_shows_docker_last_lines(self):
        rules = self.fresh_rules()
        rules[1] = {'prefix': ['image', 'inspect'], 'results': [[1, '', 'No such image']]}
        rules.append({'prefix': ['build'], 'results': [[1, '', 'step 3\nE: Unable to locate package nope\n']]})
        done = self.run_verb('create', self.create_request(), rules=rules)
        self.assertEqual(done.returncode, 1)
        self.assertIn('Unable to locate package nope', done.stderr)


class Instance(Harness):
    def make(self, verb, extra=None, arguments=None, rules=None):
        request = {'kind': 'instance', 'name': 'web-1', 'token': 'tok-web', 'provisioner': 'container',
                   'labels': {'berth-resource': 'r1', 'berth-token': 'tok-web'}}
        if verb != 'create':
            request['id'] = 'web-1'
        request.update(extra or {})
        arguments = dict({'image': 'debian:12', 'command': '', 'environment': '', 'ports': '', 'volumes': '',
                          'network': '', 'restart': 'no', 'cpus': '', 'memory': ''}, **(arguments or {}))
        return self.run_verb(verb, request, arguments=arguments, rules=rules)

    def test_create_runs_a_plain_container_from_the_image_with_no_berth_host(self):
        rules = [{'prefix': ['ps', '--all', '--format', '{{.Names}}', '--filter', 'label=berth-token=tok-web'], 'results': [[0, '', '']]},
                 {'prefix': ['container', 'inspect'], 'results': [MISSING]}]
        done = self.make('create', arguments={'command': 'sleep infinity', 'environment': f'A={SECRET}\n# note\n',
                                              'ports': '8080:80, 9000:9000', 'restart': 'unless-stopped',
                                              'memory': '2GiB'}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(json.loads(done.stdout), {'id': 'web-1', 'state': 'running'})
        [call] = [c for c in self.calls() if c['args'][:1] == ['run']]
        run = call['args']
        self.assertIn('berth-token=tok-web', run)
        self.assertIn('berth-resource=r1', run)
        self.assertNotIn('berth-host=', ' '.join(run))
        # A value reaches docker in its environment, and only the name is on its command line.
        self.assertEqual(run[run.index('--env') + 1], 'A')
        self.assertNotIn(SECRET, ' '.join(run))
        self.assertEqual(call['env']['A'], SECRET)
        self.assertEqual(run[run.index('--restart') + 1], 'unless-stopped')
        self.assertEqual(run[-3:], ['debian:12', 'sleep', 'infinity'])

    def test_create_again_with_its_token_is_the_same_container(self):
        rules = [{'prefix': ['ps', '--all', '--format', '{{.Names}}', '--filter', 'label=berth-token=tok-web'],
                  'results': [[0, 'web-1\n', '']]},
                 {'prefix': ['container', 'inspect'], 'results': [inspect(OURS)]}]
        done = self.make('create', rules=rules)
        self.assertEqual(json.loads(done.stdout), {'id': 'web-1', 'state': 'running'})
        self.assertEqual(self.argv('run'), [])

    def test_a_taken_name_or_a_bad_image_is_refused(self):
        rules = [{'prefix': ['ps', '--all', '--format', '{{.Names}}', '--filter', 'label=berth-token=tok-web'], 'results': [[0, '', '']]},
                 {'prefix': ['container', 'inspect'], 'results': [inspect(OURS)]}]
        done = self.make('create', rules=rules)
        self.assertEqual(done.returncode, 1)
        self.assertIn('already has a container named web-1', done.stderr)
        empty = [{'prefix': ['ps', '--all', '--format', '{{.Names}}', '--filter', 'label=berth-token=tok-web'], 'results': [[0, '', '']]},
                 {'prefix': ['container', 'inspect'], 'results': [MISSING]}]
        done = self.make('create', arguments={'image': ''}, rules=empty)
        self.assertEqual(done.returncode, 1)
        self.assertIn('needs an image', done.stderr)
        done = self.make('create', arguments={'environment': 'NOEQUALS'}, rules=empty)
        self.assertEqual(done.returncode, 1)
        self.assertIn('NAME=VALUE', done.stderr)
        done = self.make('create', arguments={'image': '--privileged'}, rules=empty)
        self.assertEqual(done.returncode, 1)
        self.assertIn('cannot start with a dash', done.stderr)
        self.assertEqual(self.argv('run'), [])

    def test_a_container_that_fails_to_start_is_removed(self):
        rules = [{'prefix': ['ps', '--all', '--format', '{{.Names}}', '--filter', 'label=berth-token=tok-web'],
                  'results': [[0, '', ''], [0, 'web-1\n', '']]},
                 {'prefix': ['container', 'inspect'], 'results': [MISSING]},
                 {'prefix': ['run'], 'results': [[125, '', 'Error: port is already allocated']]}]
        done = self.make('create', arguments={'ports': '8080:80'}, rules=rules)
        self.assertEqual(done.returncode, 1)
        self.assertIn('port is already allocated', done.stderr)
        self.assertEqual(self.argv('rm'), [['rm', '--force', 'web-1']])

    def test_an_instance_is_listed_beside_the_machines_and_not_as_one(self):
        rules = [{'prefix': ['ps', '--all', '--format', '{{.Names}}', '--filter', 'label=berth-resource'], 'results': [[0, 'web-1\n', '']]},
                 {'prefix': ['container', 'inspect', 'web-1'], 'results': [inspect({'berth-resource': 'r1'})]},
                 {'prefix': ['container', 'inspect', 'berth-studio'], 'results': [inspect(OURS)]},
                 {'prefix': ['ps'], 'results': [[0, '', '']]}]
        done = self.run_verb('list', {'kind': 'instance'}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(json.loads(done.stdout), {'resources': [
            {'id': 'web-1', 'kind': 'instance', 'name': 'web-1', 'host': '', 'state': 'running'}]})

    def test_the_containers_of_a_deployment_are_no_instances(self):
        compose = {'berth-resource': 'shop', 'berth-token': 'tok-9', 'com.docker.compose.project': 'shop'}
        rules = [{'prefix': ['ps', '--all', '--format', '{{.Names}}', '--filter', 'label=berth-resource'],
                  'results': [[0, 'web-1\nshop-web-1\n', '']]},
                 {'prefix': ['container', 'inspect', 'web-1'], 'results': [inspect({'berth-resource': 'r1'})]},
                 {'prefix': ['container', 'inspect', 'shop-web-1'], 'results': [inspect(compose)]},
                 {'prefix': ['ps'], 'results': [[0, '', '']]}]
        done = self.run_verb('list', {'kind': 'instance'}, rules=rules)
        self.assertEqual([r['id'] for r in json.loads(done.stdout)['resources']], ['web-1'])
        done = self.make('stop', {'id': 'shop-web-1'}, rules=rules)
        self.assertEqual(done.returncode, 1)
        self.assertIn('was not made by berth as an instance', done.stderr)
        self.assertEqual(self.argv('stop'), [])

    def test_change_applies_limits_and_refuses_what_is_fixed(self):
        rules = [{'prefix': ['container', 'inspect', 'web-1'], 'results': [inspect({'berth-resource': 'r1'})]}]
        done = self.make('change', {'changed': ['cpus']}, arguments={'cpus': '2'}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.argv('update'), [['update', '--cpus', '2', 'web-1']])
        done = self.make('change', {'changed': ['cpus', 'image']}, arguments={'cpus': '4', 'image': 'debian:13'},
                         rules=rules)
        self.assertEqual(done.returncode, 1)
        self.assertIn('cannot change its image', done.stderr)
        self.assertIn('make a new instance', done.stderr)
        self.assertEqual(len(self.argv('update')), 1)

    def test_stop_start_and_destroy_only_what_berth_made_as_an_instance(self):
        rules = [{'prefix': ['container', 'inspect', 'web-1'], 'results': [inspect({'berth-resource': 'r1'})]}]
        self.assertEqual(self.make('stop', rules=rules).returncode, 0)
        self.assertEqual(self.argv('stop'), [['stop', '--time', '30', 'web-1']])
        self.assertEqual(self.make('start', rules=rules).returncode, 0)
        self.assertEqual(self.argv('start'), [['start', 'web-1']])
        self.assertEqual(self.make('destroy', rules=rules).returncode, 0)
        self.assertIn(['rm', '--force', '--volumes', 'web-1'], self.calls() and self.argv('rm'))
        stranger = [{'prefix': ['container', 'inspect', 'web-1'], 'results': [inspect(OURS)]}]
        done = self.make('stop', rules=stranger)
        self.assertEqual(done.returncode, 1)
        self.assertIn('was not made by berth as an instance', done.stderr)

    def test_the_storage_actions_list_create_and_remove(self):
        rules = [{'prefix': ['volume', 'ls'], 'results': [[0, json.dumps({'Name': 'wiki-data', 'Labels': 'berth-host=studio'}) + '\n', '']]},
                 {'prefix': ['ps'], 'results': [[0, '', '']]}]
        done = self.run_verb('act', {'kind': 'instance', 'action': {'name': 'volumes', 'arguments': {}}}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(json.loads(done.stdout)['details'][0]['label'], 'wiki-data')
        done = self.run_verb('act', {'action': {'name': 'create-volume', 'arguments': {'name': 'data_1'}}})
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.argv('volume', 'create'), [['volume', 'create', 'data_1']])
        done = self.run_verb('act', {'action': {'name': 'create-volume', 'arguments': {'name': 'bad name!'}}})
        self.assertEqual(done.returncode, 1)
        done = self.run_verb('act', {'action': {'name': 'remove-volume', 'arguments': {'name': 'data_1'}}})
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn(['volume', 'rm', 'data_1'], self.argv('volume', 'rm'))
        done = self.run_verb('act', {'action': {'name': 'pull-image', 'arguments': {'image': 'debian:12'}}})
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.argv('pull'), [['pull', 'debian:12']])
        done = self.run_verb('act', {'action': {'name': 'remove-image', 'arguments': {'image': 'debian:12'}}})
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.argv('image', 'rm'), [['image', 'rm', 'debian:12']])
        # Nothing that docker would read as an option reaches it.
        for name, arguments in [('pull-image', {'image': '--all-tags'}), ('remove-image', {'image': '-f'}),
                                ('remove-volume', {'name': '-f'}), ('remove-volume', {'name': ''})]:
            done = self.run_verb('act', {'action': {'name': name, 'arguments': arguments}})
            self.assertEqual(done.returncode, 1, name)
        self.assertEqual(len(self.argv('pull') + self.argv('image', 'rm') + self.argv('volume', 'rm')), 3)


class Deployment(Harness):
    def deploy(self, verb, extra=None, arguments=None, rules=None, environment=None):
        request = {'kind': 'deployment', 'name': 'shop', 'environment': environment or {}}
        if verb != 'create':
            request['id'] = 'shop'
        request.update(extra or {})
        arguments = dict({'keep_volumes': 'no'}, **(arguments or {}))
        return self.run_verb(verb, request, arguments=arguments, rules=rules)

    def compose(self, *rest):
        return self.argv(*COMPOSE, *rest)

    def create_rules(self, state=None):
        return [{'prefix': [*COMPOSE, '-f', '-', 'config', '--services'], 'results': [[0, 'web\ndb\n', '']]},
                {'prefix': [*COMPOSE, 'ps'], 'results': [state or ps(('web', 'running'), ('db', 'running'))]}]

    def test_create_brings_the_project_up_with_the_spec_on_stdin(self):
        done = self.deploy('create', {'spec': SPEC, 'token': 'tok-9',
                                      'labels': {'berth-resource': 'shop', 'berth-token': 'tok-9'}},
                           rules=self.create_rules())
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(json.loads(done.stdout), {'id': 'shop', 'state': 'running'})
        calls = [c for c in self.calls() if c['args'][:1] == ['compose']]
        self.assertEqual(calls[0]['args'], [*COMPOSE, '-f', '-', 'config', '--services'])
        up = calls[1]['args']
        self.assertEqual(up[:6], [*COMPOSE, '-f', '-', '-f'])
        self.assertEqual(up[7:], ['up', '-d', '--remove-orphans'])
        self.assertEqual(calls[2]['args'], [*COMPOSE, 'ps', '--all', '--format', 'json'])
        self.assertEqual(len(calls), 3)
        for call in calls[:2]:
            self.assertEqual(call['stdin'], SPEC)
        for call in self.calls():
            self.assertNotIn('example/web', ' '.join(call['args']))
        # berth's labels go on every service, in a private file of their own that does not stay.
        override = calls[1]['files'][up[6]]
        self.assertEqual(override['mode'], 0o600)
        labels = {'berth-resource': 'shop', 'berth-token': 'tok-9'}
        self.assertEqual(json.loads(override['text']), {'services': {'web': {'labels': labels}, 'db': {'labels': labels}}})
        self.assertFalse(os.path.exists(up[6]))
        # Compose runs in an empty directory, so no compose.yaml or .env of the caller comes in.
        self.assertNotEqual(calls[1]['cwd'], str(PLUGIN))
        self.assertFalse(os.path.exists(calls[1]['cwd']))

    def test_create_again_is_up_again(self):
        for _ in range(2):
            done = self.deploy('create', {'spec': SPEC, 'token': 'tok-9'}, rules=self.create_rules())
            self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(len([c for c in self.compose('-f') if 'up' in c]), 2)

    def test_secrets_reach_compose_only_through_its_environment(self):
        rules = self.create_rules()
        rules.insert(0, {'prefix': [*COMPOSE, '-f', '-', '-f'], 'results': [[0, '', f'pulling {SECRET} layer\n']]})
        done = self.deploy('create', {'spec': SPEC, 'token': 'tok-9'}, rules=rules,
                           environment={'DB_PASSWORD': SECRET})
        self.assertEqual(done.returncode, 0, done.stderr)
        compose = [c for c in self.calls() if c['args'][:1] == ['compose']]
        for call in compose:
            self.assertEqual(call['env']['DB_PASSWORD'], SECRET)
        for call in self.calls():
            self.assertNotIn(SECRET, ' '.join(call['args']))
            self.assertNotIn(SECRET, call['stdin'])
            self.assertNotIn(SECRET, json.dumps(call['files']))
        self.assertNotIn(SECRET, done.stdout + done.stderr)
        self.assertIn('pulling *** layer', done.stderr)
        # Nor in the one-line error when compose fails and names it.
        rules[0] = {'prefix': [*COMPOSE, '-f', '-', '-f'], 'results': [[1, '', f'invalid reference format: {SECRET}\n']]}
        done = self.deploy('create', {'spec': SPEC, 'token': 'tok-9'}, rules=rules,
                           environment={'DB_PASSWORD': SECRET})
        self.assertEqual(done.returncode, 1)
        self.assertNotIn(SECRET, done.stdout + done.stderr)
        self.assertIn('docker compose up failed', done.stderr)

    def test_environment_names_are_checked_without_showing_values(self):
        for name in ('DOCKER_HOST', 'COMPOSE_FILE', 'not a name'):
            with self.subTest(name=name):
                done = self.deploy('create', {'spec': SPEC}, rules=self.create_rules(), environment={name: SECRET})
                self.assertEqual(done.returncode, 1)
                self.assertNotIn(SECRET, done.stderr)
                self.assertEqual(self.calls(), [])

    def test_the_endpoint_is_docker_host_for_compose(self):
        done = self.deploy('create', {'spec': SPEC}, rules=self.create_rules(),
                           arguments={'host': 'ssh://ops@192.0.2.10'})
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual({c['DOCKER_HOST'] for c in self.calls()}, {'ssh://ops@192.0.2.10'})
        self.assertIn('ssh://ops@192.0.2.10', done.stderr)

    def test_a_bad_project_name_is_refused_before_anything_runs(self):
        for name in ('Shop', '-shop', 'shop.web', '', 'shop web'):
            with self.subTest(name=name):
                done = self.deploy('create', {'name': name, 'spec': SPEC}, rules=self.create_rules())
                self.assertEqual(done.returncode, 1)
                self.assertIn('not a Compose project name', done.stderr)
                self.assertEqual(done.stderr.strip().count('\n'), 0)
                self.assertEqual(self.calls(), [])
        done = self.deploy('status', {'id': 'Shop'})
        self.assertEqual(done.returncode, 1)
        self.assertEqual(self.calls(), [])

    def test_create_without_a_spec_is_refused(self):
        done = self.deploy('create', {'spec': ''}, rules=self.create_rules())
        self.assertEqual(done.returncode, 1)
        self.assertIn('no Compose file', done.stderr)
        self.assertEqual(self.calls(), [])

    def test_change_applies_the_new_spec_and_keeps_the_labels(self):
        done = self.deploy('change', {'spec': SPEC}, rules=self.create_rules())
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(json.loads(done.stdout), {'state': 'running'})
        [up] = [c for c in self.calls() if 'up' in c['args']]
        self.assertEqual(up['args'][-3:], ['up', '-d', '--remove-orphans'])
        self.assertEqual(up['stdin'], SPEC)
        override = json.loads(list(up['files'].values())[0]['text'])
        self.assertEqual(override['services']['web']['labels'], {'berth-resource': 'shop', 'berth-token': 'tok-9'})

    def test_status_reads_both_forms_of_ps(self):
        cases = [(ps(('web', 'running'), ('db', 'running')), {'state': 'running'}),
                 (ps(('web', 'running'), ('db', 'running'), lines=True), {'state': 'running'}),
                 (ps(('web', 'exited'), ('db', 'exited'), lines=True), {'state': 'stopped'}),
                 (ps(('web', 'running'), ('db', 'exited')), {'state': 'running', 'message': 'db not running'}),
                 ([0, '[]', ''], {'state': 'gone'}),
                 ([0, '', ''], {'state': 'gone'})]
        for result, expected in cases:
            with self.subTest(expected=expected, result=result[1][:20]):
                done = self.deploy('status', rules=[{'prefix': [*COMPOSE, 'ps'], 'results': [result]}])
                self.assertEqual(done.returncode, 0, done.stderr)
                self.assertEqual(json.loads(done.stdout), expected)
        self.assertEqual(set(map(tuple, self.compose())), {(*COMPOSE, 'ps', '--all', '--format', 'json')})

    def test_destroy_removes_volumes_unless_kept(self):
        rules = [{'prefix': [*COMPOSE, 'ps'], 'results': [ps(('web', 'running'))]}]
        done = self.deploy('destroy', rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(json.loads(done.stdout), {})
        self.assertEqual(self.compose('down'), [[*COMPOSE, 'down', '--remove-orphans', '--volumes']])
        done = self.deploy('destroy', rules=rules, arguments={'keep_volumes': 'yes'})
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.compose('down')[1], [*COMPOSE, 'down', '--remove-orphans'])

    def test_destroy_of_a_project_that_is_gone_succeeds(self):
        rules = [{'prefix': [*COMPOSE, 'ps'], 'results': [[0, '[]', '']]},
                 {'prefix': [*COMPOSE, 'down'], 'results': [[1, '', 'no such project']]}]
        done = self.deploy('destroy', rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        rules = [{'prefix': [*COMPOSE, 'ps'], 'results': [ps(('web', 'running'))]},
                 {'prefix': [*COMPOSE, 'down'], 'results': [[1, '', 'Error: something held on']]}]
        done = self.deploy('destroy', rules=rules)
        self.assertEqual(done.returncode, 1)
        self.assertIn('something held on', done.stderr)

    def test_a_project_without_berth_labels_is_left_alone(self):
        rules = [{'prefix': [*COMPOSE, 'ps'], 'results': [ps(('web', 'running'), labels='com.docker.compose.project=shop')]}]
        for verb in ('destroy', 'stop', 'change'):
            with self.subTest(verb=verb):
                done = self.deploy(verb, {'spec': SPEC}, rules=rules)
                self.assertEqual(done.returncode, 1)
                self.assertIn('not created by berth', done.stderr)
        self.assertEqual([c for c in self.compose() if 'ps' not in c], [])

    def test_start_and_stop(self):
        rules = [{'prefix': [*COMPOSE, 'ps'], 'results': [ps(('web', 'exited'))]}]
        for verb in ('start', 'stop'):
            done = self.deploy(verb, rules=rules)
            self.assertEqual(done.returncode, 0, done.stderr)
            self.assertEqual(json.loads(done.stdout), {})
        self.assertEqual([c for c in self.compose() if 'ps' not in c], [[*COMPOSE, 'start'], [*COMPOSE, 'stop']])
        done = self.deploy('start', rules=[{'prefix': [*COMPOSE, 'ps'], 'results': [[0, '[]', '']]}])
        self.assertEqual(done.returncode, 1)
        self.assertIn('has no containers', done.stderr)

    def test_logs_pass_through_as_they_come(self):
        rules = [{'prefix': [*COMPOSE, 'logs'], 'results': [[0, 'web-1  | ready\ndb-1  | ready\n', '']]}]
        done = self.deploy('logs', {'tail': 20, 'follow': True}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(done.stdout, 'web-1  | ready\ndb-1  | ready\n')
        done = self.deploy('logs', {'tail': 0, 'follow': False}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.compose('logs'), [[*COMPOSE, 'logs', '--no-color', '--tail', '20', '--follow'],
                                                [*COMPOSE, 'logs', '--no-color', '--tail', 'all']])

    def test_list_shows_projects_with_berth_labels(self):
        projects = 'shop\trunning\nshop\texited\nblog\texited\n'
        rules = [{'prefix': ['ps', '--all', '--filter', 'label=berth-resource'], 'results': [[0, projects, '']]},
                 {'prefix': ['ps'], 'results': [[0, '', '']]}]
        done = self.run_verb('list', {'kind': 'deployment'}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(json.loads(done.stdout), {'resources': [
            {'id': 'blog', 'kind': 'deployment', 'name': 'blog', 'host': '', 'state': 'stopped'},
            {'id': 'shop', 'kind': 'deployment', 'name': 'shop', 'host': '', 'state': 'running'}]})
        self.assertEqual(self.argv('ps'), [['ps', '--all', '--filter', 'label=berth-resource', '--format',
                                            '{{.Label "com.docker.compose.project"}}\t{{.State}}']])
        done = self.run_verb('list', rules=rules)
        self.assertEqual(len(json.loads(done.stdout)['resources']), 2)
        done = self.run_verb('list', {'kind': 'machine'}, rules=rules)
        self.assertEqual(json.loads(done.stdout), {'resources': []})

    def test_services_action_has_a_row_per_service(self):
        rules = [{'prefix': [*COMPOSE, 'ps'], 'results': [ps(('web', 'running'), ('db', 'exited'), lines=True)]}]
        done = self.deploy('act', {'action': {'name': 'services', 'arguments': {}}}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(json.loads(done.stdout), {
            'message': '2 services of shop, 1 running',
            'details': [{'label': 'web', 'value': 'running · Up 2 minutes'},
                        {'label': 'db', 'value': 'exited · Exited (0) 1 minute ago'}]})

    def test_restart_and_pull_actions(self):
        rules = self.create_rules()
        done = self.deploy('act', {'action': {'name': 'restart', 'arguments': {}}}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(json.loads(done.stdout), {'message': 'Restarted 2 containers of shop'})
        self.assertEqual(self.compose('restart'), [[*COMPOSE, 'restart']])
        done = self.deploy('act', {'action': {'name': 'pull', 'arguments': {}}, 'spec': SPEC},
                           rules=rules, environment={'DB_PASSWORD': SECRET})
        self.assertEqual(done.returncode, 0, done.stderr)
        [pull] = [c for c in self.calls() if c['args'][-1:] == ['pull']]
        self.assertEqual(pull['args'], [*COMPOSE, '-f', '-', 'pull'])
        self.assertEqual(pull['stdin'], SPEC)
        self.assertEqual(pull['env']['DB_PASSWORD'], SECRET)
        self.assertEqual(len([c for c in self.calls() if 'up' in c['args']]), 1)
        self.assertNotIn(SECRET, done.stdout + done.stderr)

    def test_actions_go_by_kind(self):
        done = self.deploy('act', {'action': {'name': 'images', 'arguments': {}}})
        self.assertEqual(done.returncode, 1)
        self.assertIn('of a deployment', done.stderr)
        done = self.run_verb('act', {'kind': 'machine', 'id': 'berth-studio', 'action': {'name': 'services'}})
        self.assertEqual(done.returncode, 1)
        self.assertIn('the kind machine has no actions', done.stderr)
        # Without an id an action is the provisioner's own.
        done = self.run_verb('act', {'kind': 'deployment', 'action': {'name': 'images', 'arguments': {}}})
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.compose(), [])


if __name__ == '__main__':
    unittest.main()
