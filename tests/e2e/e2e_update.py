"""Updating a plugin from a registry, telling that the host is behind, and
bringing it up to date again without losing what was made on it: from this
machine, through berth serve with a terminal client, and for a plugin that
comes with berth and changed with it.

Update and ShippedChanged use the extended state of the host
(harness.extended()), which applied `up` 1.0.0 from the registry and the
shipped `shipit`; Update leaves `up` at 2.0.0 and the host current, which
ShippedChanged starts from. UpdateServed uses the shared state of berth serve
(harness.served_extended()), which applied `served` 1.0.0."""
import json
import unittest

from harness import (AGENT, HOST, SCRATCH, Workstation, extended, light_vars, needs_e2e, package_registry,
                     plugin_version, random_text, served_extended, unb64, wait_for)

PACKAGE = 'berth-plugin-up'


def version_of(applied, plugin):
    """The version of a plugin the host says it was applied with."""
    return next(found['version'] for found in applied['plugins'] if found['id'] == plugin)


class KeepsState:
    """What the agent made on the host before an update, checked afterwards."""

    def __init__(self, topic):
        self.topic = topic

    def plant(self, station, run):
        """Make the things with `run`, a command on the host as the agent."""
        self.mark = random_text('kept')
        run(f'echo {self.mark} > ~/kept-by-agent.txt')
        run(f'mkdir -p /srv/agent/workspaces/kept && echo {self.mark} > /srv/agent/workspaces/kept/notes.md')
        run(f"box publish {self.topic} '{json.dumps({'mark': self.mark})}'")

    def check(self, test, run):
        test.assertEqual(run('cat ~/kept-by-agent.txt').strip(), self.mark)
        test.assertEqual(run('cat /srv/agent/workspaces/kept/notes.md').strip(), self.mark)


@needs_e2e
class Update(unittest.TestCase):
    """From this machine, with the plugin from the registry."""

    @classmethod
    def setUpClass(cls):
        cls.station = extended().station
        cls.registry = package_registry()
        cls.npm = cls.registry.env()
        cls.link = cls.station.link()

    @classmethod
    def tearDownClass(cls):
        cls.station.remove_host('alpha', check=False)
        cls.station.berth('host', HOST, 'secret', 'up-key', 'off', check=False)
        cls.station.berth('secret', 'remove', 'up-key', check=False)

    def run_as_agent(self, command):
        return self.station.berth('exec', HOST, '--', 'bash', '-c', command).stdout

    def applied(self):
        return self.link.get('/v1/version').get('applied')

    def test_an_update_reaches_the_host_and_keeps_what_was_there(self):
        station = self.station
        # What the host says it was applied from.
        applied = self.applied()
        self.assertEqual(version_of(applied, 'up'), '1.0.0')
        self.assertEqual(station.berth('run', HOST, 'up:old').stdout.strip(), 'old task')

        # What the agent has before the update: files, a topic, a sent secret
        # and a session that was kept.
        kept = KeepsState('up/notes')
        kept.plant(station, self.run_as_agent)
        secret = random_text('up-secret')
        station.berth('secret', 'set', 'up-key', input=secret + '\n')
        station.berth('host', HOST, 'secret', 'up-key', 'file', '~/.config/e2e/up.key', '0600')
        station.berth('run', HOST, 'secrets')
        session = station.berth('exec', HOST, '--keep', '--', 'echo', f'session {kept.mark}')
        self.assertIn(kept.mark, session.stdout)

        # A newer version in the registry.
        self.registry.publish(plugin_version('2.0.0'))
        checked = station.berth('plugins', 'update', '--check', env=self.npm).stdout
        self.assertRegex(checked, r'(?m)^up\s+1\.0\.0 → 2\.0\.0$')
        updated = station.berth('plugins', 'update', 'up', env=self.npm).stdout
        self.assertIn('✓ up 1.0.0 → 2.0.0', updated)
        self.assertIn(f'{HOST} needs applying: up 1.0.0 → 2.0.0', updated)
        self.assertRegex(station.berth('plugins').stdout, rf'(?m)^up  {PACKAGE} 2\.0\.0  ')

        # The overview says so too.
        overview = station.terminal(HOST, cols=160)
        try:
            overview.expect(r'Applied before these changed: up 1\.0\.0 → 2\.0\.0', timeout=60)
        finally:
            overview.close()

        # A roll-out that fails at its first host leaves the next as it was.
        alpha = station.read_profile()
        alpha['ansible_vars'] = light_vars(['up/missing'])
        alpha.pop('tailscale_auto_key', None)
        station.write_profile(alpha, name='alpha')
        stopped = station.berth('apply', '--behind', check=False, timeout=900)
        self.assertEqual(stopped.returncode, 1, stopped.stdout)
        self.assertRegex(stopped.stdout, r'(?m)^alpha: failed: applying failed')
        self.assertRegex(stopped.stdout, rf'(?m)^{HOST}: not started$')
        self.assertEqual(version_of(self.applied(), 'up'), '1.0.0')
        station.remove_host('alpha')

        # Brought up to date.
        rolled = station.berth('apply', '--behind', timeout=900).stdout
        self.assertIn(f'▸ {HOST}: applying (up 1.0.0 → 2.0.0)', rolled)
        self.assertIn(f'✓ {HOST} is up to date', rolled)
        self.assertEqual(version_of(self.applied(), 'up'), '2.0.0')
        self.assertIn(f'✓ {HOST} is current', station.berth('apply', '--behind').stdout)

        # What changed is there, and what went is gone.
        self.assertEqual(station.berth('run', HOST, 'up:hello').stdout.strip(), 'hello from 2.0.0')
        self.assertEqual(station.berth('run', HOST, 'up:added').stdout.strip(), 'added task')
        tasks = station.berth('tasks', HOST).stdout
        self.assertNotIn('up:old', tasks)
        services = json.loads(station.root_on_host('cat /etc/berth/services.json').stdout)
        ours = [service['run'] for service in services if 'box serve up/echo' in service['run']]
        self.assertEqual(len(ours), 1, services)
        self.assertIn('2.0.0', ours[0])
        script = station.root_on_host('cat /usr/local/lib/berth/services/up-echo.sh').stdout
        self.assertIn('berth-e2e-echo 2.0.0', script)
        self.assertEqual(station.root_on_host('cat /usr/local/lib/berth-up/two').stdout.strip(), '2.0.0')

        # And what the agent had is still there.
        kept.check(self, self.run_as_agent)
        self.assertEqual(self.run_as_agent('cat ~/.config/e2e/up.key').strip(), secret)
        sessions = station.berth('sessions', HOST).stdout
        self.assertIn(f'session {kept.mark}', sessions)
        notes = self.link.get('/v1/channel?topic=up/notes')
        messages = notes['messages'] if isinstance(notes, dict) else notes
        self.assertIn(kept.mark, [message['body'].get('mark') for message in messages])


@needs_e2e
class ShippedChanged(unittest.TestCase):
    """A plugin that comes with berth changes with berth, keeping its version.
    The roll-out that applies the change is asked to reboot the host too,
    which says it needs one while a session runs there."""

    @classmethod
    def setUpClass(cls):
        cls.station = station = extended().station
        cls.link = station.link()
        cls.current = station.berth('apply', '--behind').stdout
        cls.refused = station.berth('plugins', 'update', 'shipit', check=False).stderr
        # The same version, another task.
        source = SCRATCH / 'source-shipped/plugins/shipit/package.json'
        manifest = json.loads(source.read_text())
        manifest['berth']['features'][0]['actions'][0]['run'] = 'echo "hello from the new berth"'
        source.write_text(json.dumps(manifest))
        station.root_on_host('touch /var/run/reboot-required')
        sleeper = cls.link.post('/v1/sessions', {'kind': 'process', 'title': 'busy', 'command': ['sleep', '600']})
        try:
            cls.rolled = station.berth('apply', '--behind', '--reboot', check=False, timeout=900)
        finally:
            cls.link.post(f'/v1/sessions/{sleeper["id"]}/signal', {'signal': 'KILL'})
            station.root_on_host('rm -f /var/run/reboot-required')

    def test_the_host_is_behind_until_applied(self):
        self.assertIn(f'✓ {HOST} is current', self.current)
        self.assertIn('comes with berth', self.refused)
        self.assertIn(f'▸ {HOST}: applying (shipit 1.0.0 (changed))', self.rolled.stdout)
        self.assertEqual(self.station.berth('run', HOST, 'shipit:hello').stdout.strip(), 'hello from the new berth')

    def test_a_reboot_waits_for_running_sessions(self):
        self.assertEqual(self.rolled.returncode, 1, self.rolled.stdout)
        self.assertRegex(self.rolled.stdout, rf'(?m)^{HOST}: failed: needs a reboot, but 1 session is running there')


@needs_e2e
class UpdateServed(unittest.TestCase):
    """Through berth serve, used from a terminal client."""

    @classmethod
    def setUpClass(cls):
        applied = served_extended()
        cls.serve = applied.serve
        cls.registry = package_registry()
        # The job that applied the plugin ended well.
        ended = cls.serve.call('GET', f'/v1/jobs/{applied.job["id"]}')
        assert ended['exit_code'] == 0, unb64(
            cls.serve.call('GET', f'/v1/jobs/{applied.job["id"]}/output')['data_b64'])[-3000:]
        cls.station = Workstation('update-client', ssh_key=False)
        cls.station.berth('config', 'server', cls.serve.url)
        cls.station.berth('config', 'server_token', cls.serve.token)

    def host(self):
        return self.serve.call('GET', f'/v1/hosts/{HOST}')

    def test_an_update_on_the_server_is_rolled_out(self):
        station = self.station
        self.assertNotIn('behind', self.host())
        mark = random_text('kv')
        self.serve.call('PUT', '/v1/kv/update/mark', {'mark': mark})
        kept = KeepsState('served/notes')

        def run(command):
            return station.berth('exec', HOST, '--', 'bash', '-c', command).stdout
        kept.plant(station, run)

        self.registry.publish(plugin_version('2.0.0', 'berth-plugin-served'))
        # The tab of the terminal client asks the server. It lists every plugin
        # with its features before the row that adds one, so it is tall.
        home = station.terminal(cols=160, rows=46)
        try:
            home.expect(HOST)
            home.send('5')
            home.expect('Add plugin')
            home.send('c')
            home.expect(r'update available: 1\.0\.0 → 2\.0\.0', timeout=60)
        finally:
            home.close()
        updated = station.berth('plugins', 'update').stdout
        self.assertIn('✓ served 1.0.0 → 2.0.0', updated)
        self.assertIn(f'{HOST} needs applying: served 1.0.0 → 2.0.0', updated)
        behind = self.host()['behind']
        self.assertEqual(behind['changes'], [{'id': 'served', 'from': '1.0.0', 'to': '2.0.0'}])
        listed = self.serve.call('GET', '/v1/hosts')
        self.assertTrue(any(host['name'] == HOST and 'behind' in host for host in listed))

        rolled = station.berth('apply', '--behind', timeout=900)
        self.assertIn(f'on the berth server at {self.serve.url}', rolled.stdout)
        self.assertIn(f'✓ {HOST} is up to date', rolled.stdout)
        self.assertRegex(rolled.stdout, rf'(?m)^{HOST}: up to date$')
        jobs = self.serve.call('GET', '/v1/jobs')
        rollout = next(job for job in jobs if job['action'] == 'rollout')
        self.assertEqual(rollout['report']['hosts'][0]['state'], 'done')
        self.assertTrue(rollout['report']['ok'])
        # What the server last heard from the host is kept a few seconds.
        wait_for('the server hearing that the host is current', lambda: 'behind' not in self.host(), timeout=30)
        self.assertEqual(station.berth('run', HOST, 'served:hello').stdout.strip(), 'hello from 2.0.0')
        kept.check(self, run)
        self.assertEqual(self.serve.call('GET', '/v1/kv/update/mark')['value'], {'mark': mark})
        self.assertEqual(run('id -un').strip(), AGENT)
