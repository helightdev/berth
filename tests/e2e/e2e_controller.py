"""berth's controller alone, on the bare test host: a machine added by its
address is reachable and refuses what needs more, install-controller puts
berthd and box on it without the rest of the role, and sessions, tasks and
secrets then work there without a full apply.

It runs on `dock` after e2e_access and before e2e_own_key, with one playbook
run. At the end the controller's files are gone again, berthd is stopped and
the key it handed over is taken back, so e2e_own_key finds the host as a
provider hands it over. The container has no systemd: its init starts berthd
once the controller's files are there, as ci/host.sh says."""
import json
import unittest

from harness import (HOST, OPERATOR, Workstation, hand_over_operator_key, needs_e2e, provisioned, random_text,
                     shell_quote, wait_for)

# What the controller puts on the host, removed again at the end.
INSTALLED = ('/usr/local/bin/berthd /usr/local/bin/box /usr/local/bin/devbox-desktop /usr/local/bin/devbox-secrets '
             '/usr/local/lib/berth /etc/berth /var/lib/berth /run/berth')


@needs_e2e
class Controller(unittest.TestCase):
    """The tests build on each other, in the order of their names."""

    @classmethod
    def setUpClass(cls):
        cls.station = Workstation('controller', ssh_key=False)
        hand_over_operator_key(cls.station.own_public())
        key = cls.station.own_key_file()
        wait_for('the operator taking the key of berth', lambda: cls.station.ssh(
            OPERATOR, 'true', key=key, check=False).returncode == 0, timeout=60)
        cls.added = cls.station.berth('host', 'add', HOST, HOST, '--user', OPERATOR, input='')

    @classmethod
    def tearDownClass(cls):
        cls.root(f'pkill -x berthd; rm -rf {INSTALLED} /home/{OPERATOR}/.config/devbox', check=False)
        hand_over_operator_key()

    @classmethod
    def root(cls, command, check=True):
        return cls.station.ssh(OPERATOR, f'sudo -n sh -c {shell_quote(command)}', key=cls.station.own_key,
                               check=check)

    def berth(self, *args, **kwargs):
        kwargs.setdefault('input', '')
        kwargs.setdefault('timeout', 120)
        return self.station.berth(*args, **kwargs)

    def test_1_a_machine_added_by_its_address_is_only_reachable(self):
        self.assertIn(f'Added {HOST} as a reachable host', self.added.stdout)
        self.assertIn(f'{HOST} takes the key berth already', self.added.stdout)
        self.assertEqual(self.station.read_profile(), {
            'level': 'reachable', 'access': {'address': HOST, 'user': OPERATOR, 'auth': 'berth'}})
        self.assertEqual(self.berth('ssh', HOST, 'id -un').stdout.strip(), OPERATOR)
        listed = self.berth('tasks', HOST).stdout
        self.assertRegex(listed, r'(?m)^  install-controller\s+Install the controller')
        # What needs more than a login is refused, and says what raises the level.
        refused = self.berth('apply', HOST, check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn('not a workstation', refused.stderr)
        refused = self.berth('exec', HOST, '--', 'id', check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn(f'berth host install {HOST}', refused.stderr)
        self.assertNotEqual(self.root('test -e /usr/local/bin/berthd', check=False).returncode, 0)

    def test_2_installing_the_controller_puts_berthd_and_box_there_alone(self):
        done = self.berth('host', 'install', HOST, timeout=600)
        self.assertIn(f'{HOST} is a controller host now', done.stdout)
        self.assertIn(f"berth's controller answers on {HOST}", done.stdout)
        self.assertEqual(self.station.read_profile()['level'], 'controller')
        applied = json.loads(self.root('cat /etc/berth/applied.json').stdout)
        self.assertEqual((applied['level'], applied['plugins']), ('controller', []))
        version = self.station.link().get('/v1/version')
        self.assertEqual(version['level'], 'controller')
        # Nothing of a workstation: no agent account, desktop or role record.
        for absent in ('id agent', 'test -e /etc/devbox', 'test -e /var/lib/devbox/status.json',
                       'test -e /etc/sudoers.d/90-devbox-agent', 'command -v Xvnc'):
            self.assertNotEqual(self.root(absent, check=False).returncode, 0, absent)

    def test_3_sessions_and_tasks_run_as_the_ssh_user(self):
        self.assertEqual(self.berth('exec', HOST, '--', 'id', '-un').stdout.strip(), OPERATOR)
        self.assertEqual(self.berth('exec', HOST, '--user', 'root', '--', 'id', '-un').stdout.strip(), 'root')
        listed = self.berth('tasks', HOST).stdout
        for name in ('status', 'update', 'secrets'):
            self.assertRegex(listed, rf'(?m)^  {name}\s+', name)
        self.assertNotRegex(listed, r'(?m)^  smoke\s+')
        self.assertRegex(listed, r'(?m)^  install-controller\s+Update the controller')
        status = self.berth('run', HOST, 'status')
        self.assertIn('Level       controller', status.stdout)
        marker = random_text('controller')
        self.berth('exec', HOST, '--keep', '--', 'echo', marker)
        line = next(line for line in self.berth('sessions', HOST).stdout.splitlines() if marker in line)
        self.assertEqual(self.berth('output', HOST, line.split()[0]).stdout.strip(), marker)

    def test_4_secrets_go_to_the_ssh_user(self):
        value = random_text('controller-secret')
        self.berth('secret', 'set', 'controller-token', input=value + '\n')
        self.berth('host', HOST, 'secret', 'controller-token', 'env', 'CONTROLLER_TOKEN')
        sent = self.berth('run', HOST, 'secrets')
        self.assertNotIn(value, sent.stdout + sent.stderr)
        env = self.root(f'cat /home/{OPERATOR}/.config/devbox/secrets.env').stdout
        self.assertIn(f'export CONTROLLER_TOKEN={value}', env)


@needs_e2e
class OnAWorkstation(unittest.TestCase):
    """The controller alone is never put on a workstation berth set up, though
    a profile saved here under another name says the host is less: on
    `studio`, as provisioned() applied it."""

    def test_a_workstation_added_again_as_reachable_keeps_its_controller(self):
        station = provisioned()
        files = 'cat /etc/berth/applied.json /etc/berth/berthd.json /etc/berth/tasks.json'
        before = station.root_on_host(files).stdout
        added = station.berth('host', 'add', 'adopted', HOST, '--user', OPERATOR, '--level', 'reachable', input='')
        try:
            # berth sees what the host is, and says how to adopt it.
            self.assertIn('adopted is a workstation; its profile here says reachable', added.stdout)
            self.assertIn('berth adopt adopted', added.stdout)
            listed = station.berth('tasks', 'adopted').stdout
            self.assertIn('adopted is a workstation; its profile here says reachable', listed)
            self.assertRegex(listed, r'(?m)^  install-controller\s+.*take its controller apart')
            refused = station.berth('host', 'install', 'adopted', check=False, timeout=600)
            self.assertNotEqual(refused.returncode, 0)
            self.assertIn('would take its controller apart', refused.stdout + refused.stderr)
            self.assertEqual(station.root_on_host(files).stdout, before)
            self.assertEqual(station.read_profile('adopted')['level'], 'reachable')
            # Adopted from the profile the host keeps, by the address it was added at.
            adopted = station.berth('adopt', 'adopted')
            self.assertIn('Adopted adopted', adopted.stdout)
            profile = station.read_profile('adopted')
            self.assertNotIn('level', profile)
            saved = station.read_profile()
            for key in ('hostname', 'operator_ssh_public_keys', 'agent_ssh_public_keys', 'ansible_vars'):
                self.assertEqual(profile[key], saved[key], key)
            self.assertEqual(profile['access'], {'address': HOST, 'user': OPERATOR, 'auth': 'berth'})
            self.assertEqual(station.root_on_host(files).stdout, before)
        finally:
            station.remove_host('adopted', check=False)
