"""The Docker container provisioner, run against a fake docker executable."""
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
import json, os, sys
state = os.environ['FAKE_DIR']
args = sys.argv[1:]
data = sys.stdin.read() if '-i' in args else ''
with open(state + '/calls.jsonl', 'a') as log:
    log.write(json.dumps({'args': args, 'stdin': data, 'DOCKER_HOST': os.environ.get('DOCKER_HOST')}) + '\n')
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


class Provision(unittest.TestCase):
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
        request = {'version': 1, 'verb': verb, 'host': 'studio', 'provisioner': 'container',
                   'arguments': {'host': '', 'image': '', 'ssh_port': '', 'publish_address': '127.0.0.1',
                                 'volume': '', 'keep_volume': 'no'},
                   'secrets': {}}
        request['arguments'].update(arguments or {})
        request.update(extra or {})
        env = dict(os.environ, PATH=path or self.path, FAKE_DIR=str(self.state))
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

    def test_manifest_has_provisioner_and_no_features(self):
        package = json.loads((PLUGIN / 'package.json').read_text())
        self.assertNotIn('features', package['berth'])
        [entry] = package['berth']['provisioners']
        self.assertEqual((entry['id'], entry['setup'], entry['command']), ('container', 'apply', 'bin/provision'))
        self.assertEqual(entry['verbs'], ['start', 'stop', 'list', 'log'])
        self.assertNotIn('secrets', entry)
        self.assertEqual([a['name'] for a in entry['arguments']],
                         ['host', 'image', 'ssh_port', 'publish_address', 'volume', 'cpus', 'memory', 'keep_volume'])
        self.assertTrue(os.access(PLUGIN / 'bin/provision', os.X_OK))

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
        self.assertEqual(run[-2:], ['berth-docker-host:0.1.0', '/sbin/init'])
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
        self.assertEqual(build[:3], ['build', '--tag', 'berth-docker-host:0.1.0'])

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

    def test_log_answers_the_latest_console_lines(self):
        console = ''.join(f'[  OK  ] Started unit-{n}.service.\n' for n in range(150)) + '\n'
        rules = [{'prefix': ['container', 'inspect'], 'results': [inspect(OURS)]},
                 {'prefix': ['logs'], 'results': [[0, console, '']]}]
        done = self.run_verb('log', {'id': 'berth-studio'}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        lines = json.loads(done.stdout)['lines']
        self.assertEqual(len(lines), 100)
        self.assertEqual(lines[-1], '[  OK  ] Started unit-149.service.')
        self.assertEqual(self.argv('logs'), [['logs', '--tail', '100', 'berth-studio']])

    def test_log_reads_the_journal_when_the_console_is_empty(self):
        rules = [{'prefix': ['container', 'inspect'], 'results': [inspect(OURS)]},
                 {'prefix': ['logs'], 'results': [[0, '', '']]},
                 {'prefix': ['exec', 'berth-studio', 'journalctl'],
                  'results': [[0, 'Started ssh.service - OpenBSD Secure Shell server.\n', '']]}]
        done = self.run_verb('log', {'id': 'berth-studio'}, rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(json.loads(done.stdout), {'lines': ['Started ssh.service - OpenBSD Secure Shell server.']})
        # A container without a journal has no lines, which is no failure.
        rules[2]['results'] = [[1, '', 'OCI runtime exec failed']]
        (self.state / 'seen-2').unlink(missing_ok=True)
        done = self.run_verb('log', {'id': 'berth-studio'}, rules=rules)
        self.assertEqual((done.returncode, json.loads(done.stdout)), (0, {'lines': []}))

    def test_log_of_a_container_that_is_gone_is_empty(self):
        done = self.run_verb('log', {'id': 'berth-studio'},
                             rules=[{'prefix': ['container', 'inspect'], 'results': [MISSING]}])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(json.loads(done.stdout), {'lines': []})
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
        rules = [{'prefix': ['ps'], 'results': [[0, 'berth-studio\nberth-studio-2\n', '']]},
                 {'prefix': ['container', 'inspect', 'berth-studio'],
                  'results': [inspect({'berth-host': 'studio'})]},
                 {'prefix': ['container', 'inspect', 'berth-studio-2'],
                  'results': [inspect({'berth-host': 'studio-2'}, running=False)]}]
        done = self.run_verb('list', rules=rules)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(json.loads(done.stdout), {'instances': [
            {'id': 'berth-studio', 'host': 'studio', 'state': 'running'},
            {'id': 'berth-studio-2', 'host': 'studio-2', 'state': 'stopped'}]})
        self.assertIn('label=berth-host', self.argv('ps')[0])

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


if __name__ == '__main__':
    unittest.main()
