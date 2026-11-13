"""berth on another machine, with its hosts those of a berth serve: the
commands go through the server, which reaches the host. ClientApply looks at
the apply of the shared state of berth serve (harness.served_extended())."""
import unittest

from harness import AGENT, HOST, Workstation, needs_e2e, served, served_extended


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

    def test_the_seed_is_rendered_by_the_server(self):
        profile = self.serve.call('GET', f'/v1/hosts/{HOST}/profile')
        draft = {**profile, 'hostname': 'cloud-3', 'builder': {}, 'access': {},
                 'operator_password_hash': '$6$e2e$' + 'x' * 86,
                 'release_url': 'https://192.0.2.30/berth-release.tar.gz'}
        self.serve.call('PUT', '/v1/hosts/cloud-3/profile', draft)
        try:
            written = self.station.berth('seed', 'cloud-3').stdout.strip()
            seed = open(written).read()
            self.assertTrue(seed.startswith('#cloud-config'))
            own = self.serve.station.own_public().split()[1]
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
class ClientApply(unittest.TestCase):
    """Applying from a terminal client: the first apply of the shared state
    of berth serve (harness.served_extended()), which a client of the server made."""

    @classmethod
    def setUpClass(cls):
        cls.applied = served_extended()

    def test_apply_is_a_job_of_the_server(self):
        done = self.applied.first
        self.assertIn(f'on the berth server at {self.applied.serve.url}', done.stderr)
        self.assertIn('PLAY RECAP', done.stdout)
        self.assertRegex(done.stdout, r'failed=0')


@needs_e2e
class AccessToken(unittest.TestCase):
    """berth given an access token of the issuer the server trusts, as BERTH_TOKEN."""

    @classmethod
    def setUpClass(cls):
        cls.serve = served()
        cls.station = Workstation('service', ssh_key=False)
        cls.station.berth('config', 'server', cls.serve.url)

    def test_the_token_gives_the_role_it_names(self):
        env = {'BERTH_TOKEN': self.serve.issuer.token('berth:operate', subject='berth-client')}
        self.assertEqual(self.station.berth('exec', HOST, '--', 'id', '-un', env=env).stdout.strip(), AGENT)
        # The role is not enough to set the server up.
        refused = self.station.berth('apply', HOST, env=env, check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn('admin', refused.stderr)

    def test_a_token_of_another_issuer_is_refused(self):
        env = {'BERTH_TOKEN': self.serve.issuer.token('berth:admin', issuer='http://192.0.2.1')}
        refused = self.station.berth('list', env=env, check=False)
        self.assertNotEqual(refused.returncode, 0)
