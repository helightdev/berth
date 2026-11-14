"""Secrets and registry logins, kept on the workstation and sent to the
agent's files on the host."""
import json
import unittest

from harness import AGENT, HOST, NpmRegistry, b64, needs_e2e, provisioned, random_text


@needs_e2e
class Secrets(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.station = provisioned()
        cls.token = random_text('github_pat_e2e')
        cls.key = random_text('deploy-key') + '\nsecond line\n'
        cls.password = random_text('npm-password')
        cls.registry = NpmRegistry('ci-user', cls.password)
        berth = cls.station.berth
        berth('secret', 'set', 'github', input=cls.token + '\n')
        berth('secret', 'set', 'deploy-key', input=cls.key)
        berth('host', HOST, 'secret', 'github', 'env', 'GH_TOKEN')
        berth('host', HOST, 'secret', 'deploy-key', 'file', '~/.config/e2e/deploy.key', '0640')
        berth('registry', 'add', 'npm-e2e', 'npm', cls.registry.url, 'ci-user')
        berth('registry', 'login', 'npm-e2e', input=cls.password + '\n')
        berth('registry', 'add', 'images', 'docker', 'https://192.0.2.20:5000', 'pusher')
        berth('registry', 'login', 'images', input='image-password\n')
        for name in ('npm-e2e', 'images'):
            berth('host', HOST, 'registry', name, 'on')

    @classmethod
    def tearDownClass(cls):
        cls.registry.stop()
        profile = cls.station.read_profile()
        profile['secrets'], profile['registries'] = [], []
        cls.station.write_profile(profile)

    def agent_file(self, path):
        return self.station.root_on_host(f'cat /home/{AGENT}/{path}').stdout

    def test_secret_values_stay_out_of_listings_and_the_profile(self):
        listed = self.station.berth('secret').stdout
        self.assertIn('github', listed)
        self.assertNotIn(self.token, listed)
        self.assertNotIn(self.token, self.station.berth('profile', HOST).stdout)
        given = self.station.berth('host', HOST).stdout
        self.assertRegex(given, r'secret\s+github\s+env GH_TOKEN')
        self.assertRegex(given, r'registry\s+npm-e2e')

    def test_a_registry_login_is_checked(self):
        checked = self.station.berth('registry', 'check', 'npm-e2e').stdout
        self.assertIn('✓ Registry: readable with the stored login', checked)
        self.assertIn('✓ Login: accepted', checked)
        self.station.berth('registry', 'login', 'npm-e2e', input='not-the-password\n')
        try:
            checked = self.station.berth('registry', 'check', 'npm-e2e').stdout
            self.assertIn('login not accepted', checked)
        finally:
            self.station.berth('registry', 'login', 'npm-e2e', input=self.password + '\n')

    def test_the_secrets_task_installs_them_for_the_agent(self):
        sent = self.station.berth('run', HOST, 'secrets')
        for value in (self.token, self.password, 'image-password'):
            self.assertNotIn(value, sent.stdout + sent.stderr)
        self.assertIn('Environment: GH_TOKEN', sent.stdout)
        self.assertIn(f"export GH_TOKEN={self.token}", self.agent_file('.config/devbox/secrets.env'))
        # What is read from a pipe loses its last line break, as a typed value does.
        self.assertEqual(self.agent_file('.config/e2e/deploy.key'), self.key.rstrip('\n'))
        mode = self.station.root_on_host(f'stat -c "%a %U" /home/{AGENT}/.config/e2e/deploy.key').stdout.split()
        self.assertEqual(mode, ['640', AGENT])
        npmrc = self.agent_file('.npmrc')
        self.assertIn(f'registry={self.registry.url}', npmrc)
        self.assertIn('_auth=' + b64(f'ci-user:{self.password}'), npmrc)
        docker = json.loads(self.agent_file('.docker/config.json'))
        self.assertEqual(docker['auths']['192.0.2.20:5000']['auth'], b64('pusher:image-password'))
        # A login shell of the agent has the variable.
        shown = self.station.berth('exec', HOST, '--', 'bash', '-lc', 'echo "${GH_TOKEN:0:15}"').stdout
        self.assertEqual(shown.strip(), self.token[:15])

    def test_a_secret_turned_off_is_gone_after_the_next_send(self):
        self.station.berth('host', HOST, 'secret', 'github', 'off')
        try:
            self.station.berth('run', HOST, 'secrets')
            self.assertNotIn('GH_TOKEN', self.agent_file('.config/devbox/secrets.env'))
        finally:
            self.station.berth('host', HOST, 'secret', 'github', 'env', 'GH_TOKEN')
            self.station.berth('run', HOST, 'secrets')
