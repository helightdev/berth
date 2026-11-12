"""What berth does on the workstation alone: its key, the cloud seed, the
inventory and the API documents."""
import json
import unittest

from harness import HOST, OPERATOR, Workstation, needs_e2e, profile, provisioned


@needs_e2e
class Key(unittest.TestCase):
    def test_berth_makes_its_own_key_once(self):
        station = Workstation('keys', ssh_key=False)
        self.assertFalse(station.own_key.exists())
        first = station.berth('key').stdout.strip()
        self.assertRegex(first, r'^ssh-ed25519 \S+ berth@\S+$')
        self.assertEqual(station.berth('key').stdout.strip(), first)
        self.assertEqual(station.own_key.stat().st_mode & 0o777, 0o600)
        # Half a key is of no use: both halves are made again.
        station.own_key.with_suffix('.pub').unlink()
        self.assertNotEqual(station.berth('key').stdout.strip(), first)


@needs_e2e
class Seed(unittest.TestCase):
    def test_the_cloud_seed_carries_the_operators_keys(self):
        station = Workstation('seeds')
        own = station.berth('key').stdout.strip()
        station.write_profile(profile(
            hostname='cloud-1', operator_ssh_public_keys=[station.public], builder={},
            # What F5 or mkpasswd would store; nobody logs in with it.
            operator_password_hash='$6$e2e$' + 'x' * 86,
            release_url='https://192.0.2.30/berth-release.tar.gz'), name='cloud-1')
        done = station.berth('seed', 'cloud-1', timeout=300)
        seeds = list((station.data / 'berth/build').rglob('user-data'))
        self.assertEqual(len(seeds), 1, done.stdout + done.stderr)
        seed = seeds[0].read_text()
        self.assertTrue(seed.startswith('#cloud-config'), seed[:200])
        self.assertIn(station.public.split()[1], seed)
        self.assertIn(own.split()[1], seed)
        self.assertIn(OPERATOR, seed)
        self.assertEqual(seeds[0].stat().st_mode & 0o077, 0)


@needs_e2e
class Screens(unittest.TestCase):
    """The terminal UI starts, shows the host and what berthd says of it, and quits."""

    @classmethod
    def setUpClass(cls):
        cls.station = provisioned()

    def test_the_home_screen_lists_the_host(self):
        terminal = self.station.terminal(cols=120, rows=30)
        try:
            terminal.expect(HOST)
            terminal.send('q')
            self.assertEqual(terminal.wait(), 0)
        finally:
            terminal.close()

    def test_the_overview_shows_the_hosts_tasks(self):
        terminal = self.station.terminal(HOST, cols=120, rows=46)
        try:
            # The rows of the tasks come from berthd on the host.
            terminal.expect('Secrets and registry logins')
            terminal.send('q')
            self.assertEqual(terminal.wait(), 0)
        finally:
            terminal.close()


@needs_e2e
class Listings(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.station = provisioned()

    def test_hosts_and_their_inventory(self):
        self.assertRegex(self.station.berth('list').stdout, rf'(?m)^{HOST}\s+{HOST}$')
        inventory = json.loads(self.station.berth('inventory', HOST).stdout)
        target = inventory['all']['hosts'][HOST]
        self.assertEqual((target['ansible_host'], target['ansible_user']), (HOST, OPERATOR))
        self.assertIn('IdentityFile=', target['ansible_ssh_extra_args'])

    def test_the_settings_say_where_things_are(self):
        shown = self.station.berth('config').stdout
        self.assertIn(str(self.station.berth_dir / 'hosts'), shown)
        self.assertIn('server = (not set)', shown)

    def test_the_api_documents(self):
        operations = json.loads(self.station.berth('operations').stdout)
        self.assertTrue(operations)
        openapi = json.loads(self.station.berth('openapi').stdout)
        self.assertIn('/v1/hosts', openapi['paths'])
        asyncapi = json.loads(self.station.berth('asyncapi').stdout)
        self.assertTrue(asyncapi.get('asyncapi'))
        self.assertRegex(self.station.berth('--version').stdout, r'^berth \d+\.\d+\.\d+')
