"""berth on another machine, with its hosts those of a berth serve: the
commands go through the server, which reaches the host."""
import json
import unittest

from harness import AGENT, HOST, Workstation, needs_e2e, served


@needs_e2e
class Client(unittest.TestCase):
    """A terminal client given the server's address and token."""

    @classmethod
    def setUpClass(cls):
        cls.serve = served()
        cls.station = Workstation('client', ssh_key=False)
        cls.station.berth('config', 'server', cls.serve.url)
        cls.station.berth('config', 'server_token', cls.serve.token)

    def test_the_hosts_are_the_servers(self):
        self.assertRegex(self.station.berth('list').stdout, rf'(?m)^{HOST}\s+{HOST}$')
        shown = self.station.berth('config').stdout
        self.assertIn(f'server = {self.serve.url}', shown)
        self.assertIn('server_token = (set)', shown)
        self.assertNotIn(self.serve.token, shown)

    def test_commands_go_through_the_server(self):
        self.assertIn('secrets', self.station.berth('tasks', HOST).stdout)
        self.assertEqual(self.station.berth('exec', HOST, '--', 'id', '-un').stdout.strip(), AGENT)
        done = self.station.berth('exec', HOST, '--', 'sh', '-c', 'cat; exit 5', input='piped\n', check=False)
        self.assertEqual((done.returncode, done.stdout), (5, 'piped\n'))
        self.assertEqual(self.station.berth('run', HOST, 'sign-in:status').returncode, 0)
        self.station.berth('exec', HOST, '--keep', '--', 'echo', 'kept through the server')
        self.assertIn('echo kept through the server', self.station.berth('sessions', HOST).stdout)

    def test_attach_through_the_server(self):
        terminal = self.station.terminal('attach', HOST)
        try:
            terminal.pump(2)
            terminal.send('echo "via $((40 + 2))"\r')
            terminal.expect('via 42')
            terminal.send('exit\r')
            self.assertEqual(terminal.wait(), 0)
        finally:
            terminal.close()

    def test_apply_is_a_job_of_the_server(self):
        done = self.station.berth('apply', HOST, timeout=900)
        self.assertIn(f'on the berth server at {self.serve.url}', done.stderr)
        self.assertIn('PLAY RECAP', done.stdout)
        self.assertRegex(done.stdout, r'failed=0')

    def test_the_seed_is_rendered_by_the_server(self):
        profile = self.serve.call('GET', f'/v1/hosts/{HOST}/profile')
        draft = {**profile, 'hostname': 'cloud-3', 'builder': {},
                 'operator_password_hash': '$6$e2e$' + 'x' * 86,
                 'release_url': 'https://192.0.2.30/berth-release.tar.gz'}
        self.serve.call('PUT', '/v1/hosts/cloud-3/profile', draft)
        try:
            written = self.station.berth('seed', 'cloud-3').stdout.strip()
            seed = open(written).read()
            self.assertTrue(seed.startswith('#cloud-config'))
            own = self.serve.station.own_key.with_suffix('.pub').read_text().split()[1]
            self.assertIn(own, seed)
        finally:
            self.serve.call('DELETE', '/v1/hosts/cloud-3')

    def test_once_with_a_server_given_on_the_command_line(self):
        once = Workstation('once', ssh_key=False)
        listed = once.berth('--server', self.serve.url, '--token', self.serve.token, 'list').stdout
        self.assertRegex(listed, rf'(?m)^{HOST}\s')
        refused = once.berth('--server', self.serve.url, '--token', 'wrong', 'list', check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn(self.serve.url, refused.stderr)


@needs_e2e
class Login(unittest.TestCase):
    """berth as a client of the issuer the server trusts, with a secret of its own."""

    @classmethod
    def setUpClass(cls):
        cls.serve = served()
        cls.station = Workstation('service', ssh_key=False)
        for key, value in (('server', cls.serve.url), ('oauth_client_id', 'berth-client'),
                           ('oauth_client_secret', 'client-secret')):
            cls.station.berth('config', key, value)

    def test_berth_logs_in_and_gets_the_role_its_client_has(self):
        done = self.station.berth('login')
        self.assertIn(f'Logged in to {self.serve.url} with the role operate', done.stdout)
        kept = json.loads((self.station.data / 'berth/oauth.json').read_text())
        self.assertTrue(kept)
        self.assertEqual((self.station.data / 'berth/oauth.json').stat().st_mode & 0o077, 0)
        self.assertEqual(self.station.berth('exec', HOST, '--', 'id', '-un').stdout.strip(), AGENT)
        # The role is not enough to set the server up.
        refused = self.station.berth('apply', HOST, check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn('admin', refused.stderr)
        self.station.berth('logout')
        kept = json.loads((self.station.data / 'berth/oauth.json').read_text())
        self.assertNotIn(self.serve.url, kept)

    def test_a_wrong_secret_gets_no_token(self):
        refused = self.station.berth('login', env={'BERTH_OAUTH_CLIENT_SECRET': 'wrong'}, check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn('invalid_client', refused.stderr)
