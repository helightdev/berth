"""Joining a tailnet at a Headscale server: with a key berth creates when the
host needs one, through berth and through berth serve, and with a key pasted
into the profile. The test host runs the real tailscale client."""
import json
import time
import unittest

from harness import HOST, Workstation, changed_tasks, headscale, needs_e2e, provisioned, served, wait_for


def joining(profile, **values):
    """A profile whose host gets Tailscale and joins the tailnet."""
    profile = {**profile, **values}
    profile['ansible_vars'] = {**profile.get('ansible_vars', {}), 'devbox_install_tailscale': True}
    return profile


def leave_the_tailnet(station):
    """Start the host's tailscaled afresh, as a machine that never joined, and
    have Headscale forget it."""
    station.root_on_host('rm -rf /var/lib/tailscale; pkill -x tailscaled', check=False)
    wait_for('tailscaled answering again', lambda: station.root_on_host(
        'tailscale status --json', check=False).stdout.strip())
    headscale().forget_nodes()


def changed(output):
    """The tasks of the role that changed, without the role's name."""
    return [name.removeprefix('devbox : ') for name in changed_tasks(output)]


def on_the_tailnet(station):
    state = json.loads(station.root_on_host('tailscale status --json', check=False).stdout or '{}')
    return state.get('BackendState') == 'Running'


def seconds_until(text):
    """Seconds from now to an RFC 3339 time of Headscale's."""
    return time.mktime(time.strptime(text[:19], '%Y-%m-%dT%H:%M:%S')) - time.mktime(time.gmtime())


class Joined:
    """What every way of joining leaves behind."""

    def assert_joined(self, created_before, created_by=None):
        """That the host is on the tailnet with the one key made after the
        first id and up to the second."""
        names = [node['givenName'] for node in headscale().nodes()]
        self.assertEqual(names, [HOST])
        self.assertTrue(on_the_tailnet(self.station))
        made = [key for key in headscale().keys()
                if created_before < int(key['id']) <= (created_by or created_before + 10**6)]
        self.assertEqual(len(made), 1, made)
        self.assertTrue(made[0]['used'])
        self.assertFalse(made[0]['reusable'] or made[0]['ephemeral'])
        return made[0]


def last_key_id():
    return max([int(key['id']) for key in headscale().keys()], default=0)


@needs_e2e
class AutomaticKey(Joined, unittest.TestCase):
    """berth on a workstation, given the server and its API key."""

    @classmethod
    def setUpClass(cls):
        cls.station = provisioned()
        cls.kept = cls.station.read_profile()
        leave_the_tailnet(cls.station)
        cls.station.berth('config', 'tailnet_login_server', headscale().url)
        cls.station.berth('config', 'tailnet_user', headscale().user)
        cls.station.berth('secret', 'set', 'tailnet_api_key', input=headscale().api_key + '\n')
        cls.station.write_profile(joining(cls.kept, tailscale_auto_key=True))
        cls.before = last_key_id()
        cls.first = cls.station.berth('apply', HOST, timeout=900)
        cls.after_first = last_key_id()
        cls.second = cls.station.berth('apply', HOST, timeout=900)

    @classmethod
    def tearDownClass(cls):
        leave_the_tailnet(cls.station)
        cls.station.write_profile(cls.kept)
        for key in ('tailnet_login_server', 'tailnet_user'):
            cls.station.berth('config', key)
        cls.station.berth('secret', 'remove', 'tailnet_api_key')

    def test_the_host_joins_with_a_key_nobody_typed(self):
        key = self.assert_joined(self.before)
        # It was made for this apply, to be used straight away.
        self.assertLess(seconds_until(key['expiration']), 3600 + 60)
        self.assertNotIn(key['key'].removesuffix('***'), self.first.stdout + self.first.stderr)
        self.assertNotIn(headscale().api_key, self.first.stdout + self.first.stderr)
        self.assertIn('Consume enrollment key', changed(self.first.stdout))

    def test_applying_again_makes_no_key(self):
        self.assertEqual(last_key_id(), self.after_first)
        self.assertEqual(changed(self.second.stdout), [])

    def test_the_host_knows_its_name_on_the_tailnet(self):
        status = self.station.berth('run', HOST, 'status').stdout
        self.assertIn(f'{HOST}.tailnet.test', status)
        # Headscale offers no tailscale serve, and the host says how else.
        access = self.station.berth('run', HOST, 'access').stdout
        self.assertIn('does not offer tailscale serve', access)
        self.assertIn('ssh -L 6080:127.0.0.1:6080', access)


@needs_e2e
class PastedKey(Joined, unittest.TestCase):
    """A key made by hand and pasted into the profile, with the login server."""

    @classmethod
    def setUpClass(cls):
        cls.station = provisioned()
        cls.kept = cls.station.read_profile()
        leave_the_tailnet(cls.station)
        cls.before = last_key_id()
        cls.station.write_profile(joining(cls.kept, tailscale_auth_key=headscale().new_key(),
                                          tailscale_login_server=headscale().url))
        cls.first = cls.station.berth('apply', HOST, timeout=900)

    @classmethod
    def tearDownClass(cls):
        leave_the_tailnet(cls.station)
        cls.station.write_profile(cls.kept)

    def test_the_host_joins_with_the_pasted_key(self):
        self.assert_joined(self.before)
        self.assertEqual(last_key_id(), self.before + 1)


@needs_e2e
class ThroughTheServer(Joined, unittest.TestCase):
    """A terminal client of berth serve: the server keeps the API key and
    makes the keys, and the client sets it up with the usual commands."""

    @classmethod
    def setUpClass(cls):
        cls.serve = served()
        cls.station = provisioned()
        leave_the_tailnet(cls.station)
        cls.client = Workstation('tailnet-client', ssh_key=False)
        cls.client.berth('config', 'server', cls.serve.url)
        cls.client.berth('config', 'server_token', cls.serve.token)
        cls.client.berth('config', 'tailnet_login_server', headscale().url)
        cls.client.berth('config', 'tailnet_user', headscale().user)
        cls.client.berth('secret', 'set', 'tailnet_api_key', input=headscale().api_key + '\n')
        cls.kept = cls.serve.call('GET', f'/v1/hosts/{HOST}/profile')
        cls.serve.call('PUT', f'/v1/hosts/{HOST}/profile', joining(cls.kept, tailscale_auto_key=True))
        cls.before = last_key_id()
        cls.first = cls.client.berth('apply', HOST, timeout=900)
        cls.after_first = last_key_id()
        cls.second = cls.client.berth('apply', HOST, timeout=900)
        cls.after_second = last_key_id()

    @classmethod
    def tearDownClass(cls):
        leave_the_tailnet(cls.station)
        cls.serve.call('PUT', f'/v1/hosts/{HOST}/profile', cls.kept)
        for key in ('tailnet_login_server', 'tailnet_user'):
            cls.serve.call('PUT', f'/v1/settings/{key}', {'value': ''})
        cls.serve.request('DELETE', '/v1/secrets/tailnet_api_key')

    def test_the_settings_and_the_key_are_the_servers(self):
        settings = {setting['key']: setting['value'] for setting in self.serve.call('GET', '/v1/settings')}
        self.assertEqual(settings['tailnet_login_server'], headscale().url)
        self.assertEqual(settings['tailnet_user'], headscale().user)
        self.assertIn('tailnet_api_key', self.serve.call('GET', '/v1/secrets')['names'])
        kept = (self.serve.station.berth_dir / 'secrets.json').read_text()
        self.assertIn(headscale().api_key, kept)
        self.assertNotIn(headscale().api_key, ''.join(
            path.read_text() for path in self.client.home.rglob('*') if path.is_file()))

    def test_the_host_joins_and_applying_again_makes_no_key(self):
        self.assertIn('failed=0', self.first.stdout)
        self.assert_joined(self.before, self.after_first)
        self.assertEqual(self.after_second, self.after_first)
        self.assertNotIn('Consume enrollment key', changed(self.second.stdout))

    def test_a_seed_carries_a_key_made_for_it(self):
        draft = {**self.kept, 'hostname': 'cloud-5', 'builder': {}, 'tailscale_auto_key': True,
                 'operator_password_hash': '$6$e2e$' + 'x' * 86,
                 'release_url': 'https://192.0.2.30/berth-release.tar.gz'}
        self.serve.call('PUT', '/v1/hosts/cloud-5/profile', joining(draft))
        try:
            before = last_key_id()
            written = self.client.berth('seed', 'cloud-5').stdout.strip()
            seed = open(written).read()
            made = [key for key in headscale().keys() if int(key['id']) > before]
            self.assertEqual(len(made), 1)
            self.assertFalse(made[0]['used'])
            self.assertGreater(seconds_until(made[0]['expiration']), 23 * 3600)
            # Headscale lists only the start of a key.
            self.assertIn(made[0]['key'].removesuffix('***'), seed)
            self.assertIn(headscale().url, seed)
            self.assertNotIn(headscale().api_key, seed)
            # The profile keeps no key: the next seed gets a new one.
            self.assertEqual(self.serve.call('GET', '/v1/hosts/cloud-5/profile').get('tailscale_auth_key', ''), '')
        finally:
            self.serve.call('DELETE', '/v1/hosts/cloud-5')
