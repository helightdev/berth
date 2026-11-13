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
        # The store keeps both halves, so a lost file comes back as it was.
        station.own_key.with_suffix('.pub').unlink()
        self.assertEqual(station.berth('key').stdout.strip(), first)
        self.assertTrue(station.own_key.with_suffix('.pub').exists())


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
class Profiles(unittest.TestCase):
    """Profiles live in berth's store, and come in and out as JSON."""

    def test_a_profile_goes_out_and_in_and_is_removed_with_what_is_kept_of_it(self):
        station = Workstation('profiles', ssh_key=False, trust=False)
        kept = profile(hostname='yard', builder={'ssh_host': '192.0.2.50', 'ssh_user': OPERATOR})
        station.write_profile(kept, name='yard')
        self.assertEqual(station.read_profile('yard'), kept)
        self.assertRegex(station.berth('list').stdout, r'(?m)^yard\s+192\.0\.2\.50$')
        self.assertFalse((station.berth_dir / 'hosts').exists())
        # What is not a profile is refused, and the saved one stays.
        refused = station.berth('profile', 'yard', '--set', '-', input='[]', check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertEqual(station.read_profile('yard'), kept)
        # Passwords go to a terminal, or to a pipe only when asked to.
        piped = station.berth('credentials', 'yard', check=False)
        self.assertNotEqual(piped.returncode, 0)
        self.assertIn('--show', piped.stderr)
        self.assertIn('no passwords', station.berth('credentials', 'yard', '--show').stderr)
        station.remove_host('yard')
        self.assertNotIn('yard', station.berth('list').stdout)
        self.assertNotEqual(station.berth('profile', 'yard', check=False).returncode, 0)

    def test_the_hosts_of_an_older_berth_move_into_the_store(self):
        station = Workstation('older-hosts', ssh_key=False, trust=False)
        hosts = station.berth_dir / 'hosts'
        hosts.mkdir(parents=True, mode=0o700)
        old = profile(hostname='attic', builder={'ssh_host': '192.0.2.60'})
        (hosts / 'attic.json').write_text(json.dumps(old))
        (hosts / 'attic.credentials.json').write_text(json.dumps({OPERATOR: 'made-up-password'}))
        (hosts / 'attic.instance.lock').write_text('')
        mirrored = station.data / 'berth/served/http---192-0-2-7-7777/hosts'
        mirrored.mkdir(parents=True)
        (mirrored / 'yard.json').write_text('{}')
        listed = station.berth('list')
        self.assertRegex(listed.stdout, r'(?m)^attic\s+192\.0\.2\.60$')
        self.assertIn('moved 1 host into berth', listed.stderr)
        self.assertFalse(hosts.exists())
        self.assertTrue((station.berth_dir / 'migrated/hosts/attic.json').is_file())
        self.assertFalse((station.data / 'berth/served').exists())
        self.assertEqual(station.read_profile('attic'), old)
        shown = station.berth('credentials', 'attic', '--show').stdout
        self.assertEqual(shown, f'{OPERATOR} made-up-password\n')
        # Once is enough.
        self.assertNotIn('moved', station.berth('list').stderr)


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
        self.assertIn('Hosts      in the store', shown)
        self.assertIn('server = (not set)', shown)
        self.assertIn(f"Store      SQLite file {self.station.data / 'berth/berth.db'}", shown)

    def test_the_api_documents(self):
        operations = json.loads(self.station.berth('operations').stdout)
        self.assertTrue(operations)
        openapi = json.loads(self.station.berth('openapi').stdout)
        self.assertIn('/v1/hosts', openapi['paths'])
        asyncapi = json.loads(self.station.berth('asyncapi').stdout)
        self.assertTrue(asyncapi.get('asyncapi'))
        self.assertRegex(self.station.berth('--version').stdout, r'^berth \d+\.\d+\.\d+')
