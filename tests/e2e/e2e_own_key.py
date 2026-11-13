"""A host that takes nothing but the key of berth, as when that key is added
at a cloud provider: no key of the user's in the profile or in ~/.ssh, no
ssh-agent, and the operator's sudo as it is by default. Once straight from a
workstation, and once from a berth serve on that workstation, which has only
the same key, with a terminal client using it.

It runs on a test host of its own, `dock`, which nothing else applies a profile to: run.py
runs it there alongside the other modules on `studio`. Direct is the first
apply that host sees; Served applies the server's profile over it."""
import json
import os
import pwd
import unittest
from pathlib import Path

from harness import (AGENT, HOST, OPERATOR, SRC, BerthdLink, Serve, Workstation, _provisioned, ansible_recap,
                     changed_tasks, hand_over_operator_key, light_vars, needs_e2e, random_text, shell_quote, wait_for)


def bare_profile(template):
    """A profile for the test host as a new host starts, with no SSH key in it."""
    profile = json.loads(json.dumps(template))
    profile.update({'hostname': HOST, 'profile': 'cloud', 'keyboard': 'us', 'ansible_vars': light_vars()})
    profile['builder'] = {'shipped_plugins': True}
    profile['access'] = {'address': HOST, 'user': OPERATOR, 'auth': 'berth'}
    return profile


def keyed():
    """The workstation whose berth key is the only one the host takes, made once."""
    if 'own-key' not in _provisioned:
        station = Workstation('own-key', ssh_key=False)
        # The key the host takes from its provider, and nothing else of the run's.
        hand_over_operator_key(station.own_public())
        station.own_key_file()
        wait_for('the operator taking the key of berth', lambda: station.ssh(
            OPERATOR, 'true', key=station.own_key, check=False).returncode == 0)
        _provisioned['own-key'] = station
    return _provisioned['own-key']


class OwnKeyOnly:
    """What has to work with nothing but berth's key. `station` runs berth,
    `keyed` is the workstation whose key is on the host."""

    station: Workstation
    keyed: Workstation

    def berth(self, *args, **kwargs):
        return self.station.berth(*args, **kwargs)

    def root_on_host(self, command):
        return self.keyed.ssh(OPERATOR, f'sudo -n sh -c {shell_quote(command)}', key=self.keyed.own_key).stdout

    def test_nothing_of_the_users_is_there(self):
        for station in (self.station, self.keyed):
            self.assertEqual(list((station.home / '.ssh').glob('id_*')), [])
            self.assertNotIn('SSH_AUTH_SOCK', station.env)
        # ssh looks in the home of the password database too.
        runner = Path(pwd.getpwuid(os.getuid()).pw_dir) / '.ssh'
        self.assertEqual(list(runner.glob('id_*')), [])

    def test_the_operator_takes_only_berths_key_and_sudo_needs_no_password(self):
        own = self.keyed.own_public()
        block = self.root_on_host(f"sed -n '/BEGIN DEVBOX/,/END DEVBOX/p' /home/{OPERATOR}/.ssh/authorized_keys")
        self.assertEqual([line for line in block.splitlines() if not line.startswith('#')], [own])
        sudoers = self.root_on_host('cat /etc/sudoers.d/99-devbox-operator')
        self.assertIn(f'{OPERATOR} ALL=(ALL:ALL) NOPASSWD: ALL', sudoers)

    def test_applying_asks_for_nothing_and_again_changes_nothing(self):
        recap = ansible_recap(self.first_apply.stdout)
        self.assertEqual((recap['failed'], recap['unreachable']), (0, 0))
        for apply in (self.first_apply, self.second_apply):
            self.assertNotIn('BECOME password', apply.stdout + apply.stderr)
        self.assertEqual(ansible_recap(self.second_apply.stdout)['failed'], 0)
        self.assertEqual(changed_tasks(self.second_apply.stdout), [])

    def test_tasks_are_listed_and_run(self):
        self.assertRegex(self.berth('tasks', HOST).stdout, r'(?m)^  secrets\s+')
        self.assertEqual(self.berth('run', HOST, 'sign-in:status').returncode, 0)

    def test_commands_run_as_each_account(self):
        for user, name in (('agent', AGENT), ('operator', OPERATOR), ('root', 'root')):
            done = self.berth('exec', HOST, '--user', user, '--', 'id', '-un')
            self.assertEqual(done.stdout.strip(), name, user)
        done = self.berth('exec', HOST, '--', 'sh', '-c', 'cat; exit 4', input='typed\n', check=False)
        self.assertEqual((done.returncode, done.stdout), (4, 'typed\n'))

    def test_sessions_are_kept_and_attached_to(self):
        marker = random_text('own-key')
        self.berth('exec', HOST, '--keep', '--', 'echo', marker)
        line = next(line for line in self.berth('sessions', HOST).stdout.splitlines() if marker in line)
        self.assertEqual(self.berth('output', HOST, line.split()[0]).stdout.strip(), marker)
        terminal = self.station.terminal('attach', HOST)
        try:
            terminal.pump(2)
            terminal.send('echo "$(id -un) $((6 * 7))"\r')
            terminal.expect(rf'{AGENT} 42')
            terminal.send('exit\r')
            self.assertEqual(terminal.wait(), 0)
        finally:
            terminal.close()

    def test_secrets_are_sent_to_the_agent(self):
        value = random_text('own-key-secret')
        self.berth('secret', 'set', 'own-key-token', input=value + '\n')
        self.berth('host', HOST, 'secret', 'own-key-token', 'env', 'OWN_KEY_TOKEN')
        try:
            sent = self.berth('run', HOST, 'secrets')
            self.assertNotIn(value, sent.stdout + sent.stderr)
            env = self.root_on_host(f'cat /home/{AGENT}/.config/devbox/secrets.env')
            self.assertIn(f'export OWN_KEY_TOKEN={value}', env)
        finally:
            self.berth('host', HOST, 'secret', 'own-key-token', 'off')
            self.berth('run', HOST, 'secrets')


@needs_e2e
class Direct(OwnKeyOnly, unittest.TestCase):
    """berth on a workstation whose own key is the only way in."""

    @classmethod
    def setUpClass(cls):
        cls.station = cls.keyed = keyed()
        template = json.loads((SRC / 'provision/config/example.json').read_text())
        # Left out, as a profile written by hand would: its default applies.
        del template['operator_passwordless_sudo']
        cls.station.write_profile(bare_profile(template))
        cls.first_apply = cls.station.berth('apply', HOST, input='', timeout=900)
        cls.second_apply = cls.station.berth('apply', HOST, input='', timeout=900)

    def test_plain_ssh_and_the_files_of_the_agent(self):
        self.assertEqual(self.berth('ssh', HOST, 'id -un').stdout.strip(), OPERATOR)
        link = BerthdLink(self.station)
        path = f'/home/{AGENT}/own-key.txt'
        content = random_text('written').encode()
        status, _ = link.request('PUT', f'/v1/files?path={path}&overwrite=true', content)
        self.assertEqual(status, 200)
        self.assertEqual(link.request('GET', f'/v1/files?path={path}', raw=True), (200, content))
        self.assertEqual(link.request('DELETE', f'/v1/files?path={path}')[0], 200)


@needs_e2e
class Served(OwnKeyOnly, unittest.TestCase):
    """A berth serve whose own key is the only way in, and a terminal client
    with neither key nor profile of its own. The server runs on the
    workstation of Direct, whose key it shares."""

    @classmethod
    def setUpClass(cls):
        cls.keyed = keyed()
        cls.serve = Serve(cls.keyed, web=False)
        catalogue = cls.serve.call('GET', '/v1/setup')
        cls.serve.call('PUT', f'/v1/hosts/{HOST}/profile', bare_profile(catalogue['template']))
        cls.station = Workstation('own-key-client', ssh_key=False)
        cls.station.berth('config', 'server', cls.serve.url)
        cls.station.berth('config', 'server_token', cls.serve.token)
        cls.first_apply = cls.station.berth('apply', HOST, input='', timeout=900)
        cls.second_apply = cls.station.berth('apply', HOST, input='', timeout=900)

    @classmethod
    def tearDownClass(cls):
        cls.serve.stop()

    def test_the_server_hands_out_its_key_and_passwordless_sudo(self):
        keys = self.serve.call('GET', '/v1/keys')['keys']
        self.assertEqual([key['name'] for key in keys], ['berth'])
        self.assertEqual(keys[0]['public'], self.keyed.own_public())
        self.assertEqual(keys[0]['hosts'], [HOST])
        self.assertNotIn('PRIVATE KEY', json.dumps(keys))
        catalogue = self.serve.call('GET', '/v1/setup')
        self.assertNotIn('key', catalogue)
        self.assertIs(catalogue['template']['operator_passwordless_sudo'], True)
        self.assertEqual(catalogue['template']['operator_ssh_public_keys'], [])

    def test_the_agents_files_through_the_server(self):
        path = f'/v1/hosts/{HOST}/files?path=/home/{AGENT}/own-key-served.txt'
        content = random_text('served').encode()
        status, _ = self.serve.request('PUT', path + '&overwrite=true', content,
                                       headers={'Content-Type': 'application/octet-stream'})
        self.assertEqual(status, 200)
        status, read, _ = self.serve.request('GET', path, raw=True)
        self.assertEqual((status, read), (200, content))
