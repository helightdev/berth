"""Updating a plugin from a registry, telling that the host is behind, and
bringing it up to date again without losing what was made on it: from this
machine, through berth serve with a terminal client, and for a plugin that
comes with berth and changed with it."""
import copy
import json
import shutil
import time
import unittest

from harness import (AGENT, HOST, SCRATCH, SRC, Workstation, light_vars, needs_e2e, package_registry,
                     provisioned, random_text, served, source_with_plugins, unb64, wait_for)

FIXTURE = SRC / 'tests/e2e/fixtures/plugin'
PACKAGE = 'berth-plugin-up'


def plugin_version(version, name=PACKAGE):
    """The fixture plugin as `up` at a version. From 2.0.0 its greeting and
    service changed, its Ansible task installs another file, the task `old`
    is gone and `added` is new."""
    folder = SCRATCH / f'{name}-{version}'
    if folder.exists():
        shutil.rmtree(folder)
    shutil.copytree(FIXTURE, folder)
    manifest = json.loads((folder / 'package.json').read_text())
    manifest['name'], manifest['version'] = name, version
    feature = manifest['berth']['features'][0]
    feature['id'] = 'main'
    feature['guide'] = ''
    newer = version != '1.0.0'
    feature['actions'] = [
        {'name': 'hello', 'label': 'Say hello', 'run': f'echo "hello from {version}"'},
        {'name': 'added' if newer else 'old', 'label': 'Added' if newer else 'Old',
         'run': f'echo "{"added" if newer else "old"} task"'},
    ]
    feature['services'][0]['run'] = f'exec box serve up/echo -- /usr/local/bin/berth-e2e-echo {version}'
    feature['channels'] = [{'topic': 'notes', 'keep': 'log', 'max': 50}]
    (folder / 'package.json').write_text(json.dumps(manifest, indent=2))
    (folder / 'tasks/install.yml').write_text(f'''---
- name: Create the folder of what the plugin brings
  ansible.builtin.file:
    path: /usr/local/lib/berth-up
    state: directory
    mode: '0755'
- name: Install what this version of the plugin brings
  ansible.builtin.copy:
    dest: /usr/local/lib/berth-up/{"two" if newer else "one"}
    content: "{version}\\n"
    mode: '0644'
''')
    return folder


def set_features(station, features):
    profile = station.read_profile()
    profile['ansible_vars'] = light_vars(features)
    # Opened by berth, a profile without this gets the agents' features.
    profile['builder']['shipped_plugins'] = True
    station.write_profile(profile)


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
        cls.station = provisioned()
        cls.registry = package_registry()
        cls.npm = cls.registry.env()
        cls.registry.publish(plugin_version('1.0.0'))
        cls.station.berth('plugins', 'add', PACKAGE, env=cls.npm)
        set_features(cls.station, ['up/main'])
        cls.station.berth('apply', HOST, timeout=900)
        cls.link = cls.station.link()

    @classmethod
    def tearDownClass(cls):
        for name in ('alpha',):
            cls.station.profile_path(name).unlink(missing_ok=True)
        cls.station.root_on_host('rm -f /var/run/reboot-required', check=False)
        cls.station.berth('host', HOST, 'secret', 'up-key', 'off', check=False)
        cls.station.berth('secret', 'remove', 'up-key', check=False)
        cls.station.berth('plugins', 'remove', PACKAGE, check=False)
        set_features(cls.station, [])
        cls.station.berth('apply', HOST, timeout=900)

    def run_as_agent(self, command):
        return self.station.berth('exec', HOST, '--', 'bash', '-c', command).stdout

    def applied(self):
        return self.link.get('/v1/version').get('applied')

    def test_an_update_reaches_the_host_and_keeps_what_was_there(self):
        station = self.station
        # What the host says it was applied from.
        applied = self.applied()
        self.assertEqual([(plugin['id'], plugin['version']) for plugin in applied['plugins']], [('up', '1.0.0')])
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
        alpha['builder']['shipped_plugins'] = True
        station.write_profile(alpha, name='alpha')
        stopped = station.berth('apply', '--behind', check=False, timeout=900)
        self.assertEqual(stopped.returncode, 1, stopped.stdout)
        self.assertRegex(stopped.stdout, r'(?m)^alpha: failed: applying failed')
        self.assertRegex(stopped.stdout, rf'(?m)^{HOST}: not started$')
        self.assertEqual(self.applied()['plugins'][0]['version'], '1.0.0')
        station.profile_path('alpha').unlink()

        # Brought up to date.
        rolled = station.berth('apply', '--behind', timeout=900).stdout
        self.assertIn(f'▸ {HOST}: applying (up 1.0.0 → 2.0.0)', rolled)
        self.assertIn(f'✓ {HOST} is up to date', rolled)
        self.assertEqual(self.applied()['plugins'][0]['version'], '2.0.0')
        self.assertIn(f'✓ {HOST} is current', station.berth('apply', '--behind').stdout)

        # What changed is there, and what went is gone.
        self.assertEqual(station.berth('run', HOST, 'up:hello').stdout.strip(), 'hello from 2.0.0')
        self.assertEqual(station.berth('run', HOST, 'up:added').stdout.strip(), 'added task')
        tasks = station.berth('tasks', HOST).stdout
        self.assertNotIn('up:old', tasks)
        services = json.loads(station.root_on_host('cat /etc/berth/services.json').stdout)
        self.assertIn('2.0.0', services[0]['run'])
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

    def test_a_reboot_waits_for_running_sessions(self):
        station = self.station
        station.root_on_host('touch /var/run/reboot-required')
        sleeper = self.link.post('/v1/sessions', {'kind': 'process', 'title': 'busy', 'command': ['sleep', '600']})
        try:
            refused = station.berth('apply', '--all', '--reboot', check=False, timeout=900)
            self.assertEqual(refused.returncode, 1, refused.stdout)
            self.assertRegex(refused.stdout, rf'(?m)^{HOST}: failed: needs a reboot, but 1 session is running there')
        finally:
            self.link.post(f'/v1/sessions/{sleeper["id"]}/signal', {'signal': 'KILL'})
            station.root_on_host('rm -f /var/run/reboot-required')


@needs_e2e
class ShippedChanged(unittest.TestCase):
    """A plugin that comes with berth changes with berth, keeping its version."""

    def test_the_host_is_behind_until_applied(self):
        station = provisioned()
        source = source_with_plugins('source-shipped', {'shipit': plugin_version('1.0.0', 'berth-plugin-shipit')})
        manifest = json.loads((source / 'plugins/shipit/package.json').read_text())
        env = {'BERTH_SOURCE': str(source)}
        before = station.read_profile()
        try:
            set_features(station, ['shipit/main'])
            station.berth('apply', HOST, timeout=900, env=env)
            self.assertIn(f'✓ {HOST} is current', station.berth('apply', '--behind', env=env).stdout)
            self.assertIn('comes with berth', station.berth('plugins', 'update', 'shipit', check=False, env=env).stderr)
            # The same version, another task.
            manifest['berth']['features'][0]['actions'][0]['run'] = 'echo "hello from the new berth"'
            (source / 'plugins/shipit/package.json').write_text(json.dumps(manifest))
            rolled = station.berth('apply', '--behind', env=env, timeout=900).stdout
            self.assertIn(f'▸ {HOST}: applying (shipit 1.0.0 (changed))', rolled)
            self.assertEqual(station.berth('run', HOST, 'shipit:hello').stdout.strip(), 'hello from the new berth')
        finally:
            station.write_profile(before)


@needs_e2e
class UpdateServed(unittest.TestCase):
    """Through berth serve, used from a terminal client."""

    @classmethod
    def setUpClass(cls):
        cls.serve = served()
        cls.registry = package_registry()
        cls.registry.publish(plugin_version('1.0.0', 'berth-plugin-served'))
        cls.serve.call('POST', '/v1/plugins', {'spec': 'berth-plugin-served'})
        cls.profile = cls.serve.call('GET', f'/v1/hosts/{HOST}/profile')
        changed = copy.deepcopy(cls.profile)
        changed['ansible_vars'] = light_vars(['served/main'])
        cls.serve.call('PUT', f'/v1/hosts/{HOST}/profile', changed)
        cls.wait_job(cls.serve.call('POST', f'/v1/hosts/{HOST}/apply')['id'])
        cls.station = Workstation('update-client', ssh_key=False)
        cls.station.berth('config', 'server', cls.serve.url)
        cls.station.berth('config', 'server_token', cls.serve.token)

    @classmethod
    def tearDownClass(cls):
        cls.serve.call('PUT', f'/v1/hosts/{HOST}/profile', cls.profile)
        cls.serve.call('DELETE', '/v1/plugins/served')
        cls.wait_job(cls.serve.call('POST', f'/v1/hosts/{HOST}/apply')['id'])

    @classmethod
    def wait_job(cls, job):
        ended = wait_for('the job ending', lambda: (lambda got: got if got['ended'] else None)(
            cls.serve.call('GET', f'/v1/jobs/{job}')), timeout=900, every=1)
        assert ended['exit_code'] == 0, unb64(cls.serve.call('GET', f'/v1/jobs/{job}/output')['data_b64'])[-3000:]
        return ended

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
        rollout = next(job for job in jobs if job['what'] == 'rollout')
        self.assertEqual(rollout['report']['hosts'][0]['state'], 'done')
        self.assertTrue(rollout['report']['ok'])
        time.sleep(11)  # what the server last heard from the host is kept a few seconds
        self.assertNotIn('behind', self.host())
        self.assertEqual(station.berth('run', HOST, 'served:hello').stdout.strip(), 'hello from 2.0.0')
        kept.check(self, run)
        self.assertEqual(self.serve.call('GET', '/v1/kv/update/mark')['value'], {'mark': mark})
        self.assertEqual(run('id -un').strip(), AGENT)
