"""What berth does with a host once it is set up: tasks, commands, sessions
and terminals, files, the channel and the host's own state."""
import json
import re
import unittest

from harness import AGENT, HOST, OPERATOR, b64, needs_e2e, provisioned, random_text, unb64, wait_for


@needs_e2e
class Commands(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.station = provisioned()

    def test_tasks_are_listed_by_group(self):
        listed = self.station.berth('tasks', HOST).stdout
        for line in ('System', 'Sync', 'Sign-In'):
            self.assertRegex(listed, rf'(?m)^{line}$')
        self.assertRegex(listed, r'(?m)^  secrets\s+Secrets and registry logins')

    def test_a_task_runs_and_its_exit_code_is_berths(self):
        self.assertEqual(self.station.berth('run', HOST, 'sign-in:status').returncode, 0)
        missing = self.station.berth('run', HOST, 'nothing-like-this', check=False)
        self.assertEqual(missing.returncode, 1)
        self.assertIn('has no task nothing-like-this', missing.stderr)

    def test_the_status_task_copes_without_systemd(self):
        done = self.station.berth('run', HOST, 'status', check=False, timeout=120)
        self.assertIn(f'Host        {HOST}', done.stdout)
        self.assertIn('Disk', done.stdout)

    def test_commands_run_as_each_account(self):
        for user, name in (('agent', AGENT), ('operator', OPERATOR), ('root', 'root')):
            done = self.station.berth('exec', HOST, '--user', user, '--', 'id', '-un')
            self.assertEqual(done.stdout.strip(), name, user)

    def test_a_command_gets_input_and_leaves_with_its_code(self):
        done = self.station.berth('exec', HOST, '--', 'sh', '-c', 'cat; echo oops >&2; exit 7',
                                  input='from the workstation\n', check=False)
        self.assertEqual(done.returncode, 7)
        self.assertEqual(done.stdout, 'from the workstation\n')
        self.assertIn('oops', done.stderr)

    def test_a_kept_session_is_listed_with_its_output(self):
        marker = random_text('kept')
        self.station.berth('exec', HOST, '--keep', '--', 'echo', marker)
        listed = self.station.berth('sessions', HOST).stdout
        line = next(line for line in listed.splitlines() if marker in line)
        self.assertIn('exit 0', line)
        session = line.split()[0]
        output = self.station.berth('output', HOST, session).stdout
        self.assertEqual(output.strip(), marker)
        self.assertEqual(self.station.berth('output', HOST, session, '--tail', '1').stdout.strip(), marker)

    def test_attach_is_a_terminal_on_the_host(self):
        terminal = self.station.terminal('attach', HOST)
        try:
            terminal.pump(2)
            terminal.send('echo "$(id -un) $((6 * 7))"\r')
            terminal.expect(rf'{AGENT} 42')
            terminal.send('exit\r')
            self.assertEqual(terminal.wait(), 0)
        finally:
            terminal.close()


@needs_e2e
class Berthd(unittest.TestCase):
    """berthd's API through the link berth uses: SSH, sudo and berthd connect."""

    @classmethod
    def setUpClass(cls):
        cls.station = provisioned()
        cls.link = cls.station.link()

    def test_a_process_session_has_its_streams_apart(self):
        started = self.link.post('/v1/sessions?wait=30s', {
            'kind': 'process', 'command': 'echo out; echo err >&2; exit 3', 'title': 'e2e streams'})
        self.assertEqual(started['exit_code'], 3)
        self.assertEqual((unb64(started['stdout_b64']), unb64(started['stderr_b64'])), ('out\n', 'err\n'))
        self.assertEqual(started['user'], 'agent')

    def test_a_terminal_session_keeps_its_screen(self):
        started = self.link.post('/v1/sessions', {'kind': 'terminal', 'command': ['bash', '--norc'],
                                                  'rows': 24, 'cols': 80, 'title': 'e2e terminal'})
        session = started['id']
        try:
            marker = random_text('screen')
            self.link.post(f'/v1/sessions/{session}/input', {'data_b64': b64(f'echo {marker}\r')})
            screen = wait_for('the marker on the screen', lambda: (
                found := self.link.get(f'/v1/sessions/{session}/screen')) and marker in found['text'] and found)
            self.assertEqual((screen['rows'], screen['cols']), (24, 80))
            self.link.post(f'/v1/sessions/{session}/resize', {'rows': 30, 'cols': 100})
            self.assertEqual(self.link.get(f'/v1/sessions/{session}/screen')['cols'], 100)
        finally:
            self.link.post(f'/v1/sessions/{session}/signal', {'signal': 'KILL'})
            wait_for('the terminal to end', lambda: self.link.get(f'/v1/sessions/{session}').get('ended'))
            status, _ = self.link.request('DELETE', f'/v1/sessions/{session}')
            self.assertEqual(status, 200)

    def test_files_are_written_and_read_as_the_agent(self):
        folder = f'/home/{AGENT}/e2e-files'
        content = random_text('content').encode()
        status, _ = self.link.request('PUT', f'/v1/files?path={folder}/note.txt', content)
        self.assertEqual(status, 200)
        status, read = self.link.request('GET', f'/v1/files?path={folder}/note.txt', raw=True)
        self.assertEqual((status, read), (200, content))
        listing = self.link.get(f'/v1/files?path={folder}')
        self.assertEqual([entry['name'] for entry in listing['entries']], ['note.txt'])
        owner = self.station.root_on_host(f'stat -c %U {folder}/note.txt').stdout.strip()
        self.assertEqual(owner, AGENT)
        # Written once, it is not replaced unless asked to.
        status, _ = self.link.request('PUT', f'/v1/files?path={folder}/note.txt', b'again')
        self.assertEqual(status, 409)
        status, _ = self.link.request('PUT', f'/v1/files?path={folder}/note.txt&overwrite=true', b'again')
        self.assertEqual(status, 200)
        status, _ = self.link.request('DELETE', f'/v1/files?path={folder}&recursive=true')
        self.assertEqual(status, 200)
        status, _ = self.link.request('GET', f'/v1/files?path={folder}')
        self.assertEqual(status, 404)

    def test_files_outside_the_roots_are_refused(self):
        for path in ('/etc/shadow', '/root', f'/home/{AGENT}/../{OPERATOR}/.ssh/authorized_keys'):
            status, answer = self.link.request('GET', f'/v1/files?path={path}')
            self.assertIn(status, (403, 404), (path, answer))
        status, _ = self.link.request('PUT', '/v1/files?path=/etc/berth/tasks.json&overwrite=true', b'[]')
        self.assertEqual(status, 403)

    def test_units_without_systemd_are_an_answer_not_a_crash(self):
        for method, path in (('GET', '/v1/units'), ('GET', '/v1/units/ssh.service'),
                             ('POST', '/v1/units/ssh.service/restart')):
            status, answer = self.link.request(method, path, {} if method == 'POST' else None)
            self.assertGreaterEqual(status, 500, (path, answer))
            self.assertTrue(answer['error']['message'], answer)
        # Without a journal there is nothing to read.
        status, answer = self.link.request('GET', '/v1/logs?limit=5')
        self.assertTrue(status == 200 and answer == [] or status >= 500, (status, answer))
        self.assertEqual(self.link.get('/v1/version')['mode'], 'system')

    def test_the_hosts_state(self):
        stats = self.link.get('/v1/stats')
        self.assertGreater(stats['memory']['total'], 0)
        self.assertTrue(any(fs['mount'] == '/' for fs in stats['filesystems']), stats['filesystems'])
        processes = self.link.get('/v1/processes')
        self.assertTrue(any('berthd' in process['command'] for process in processes))

    def test_a_task_runs_as_steps(self):
        tasks = {task['name']: task for task in self.link.get('/v1/tasks')}
        self.assertEqual(tasks['status']['user'], 'root')
        session = self.link.post('/v1/tasks/sign-in:status?wait=60s', {})
        self.assertEqual(session['exit_code'], 0)
        self.assertEqual(session['task'], 'sign-in:status')

    def test_the_channel_keeps_what_it_is_told_to(self):
        topic = random_text('e2e-host/notes')
        body = {'said': random_text('hello')}
        done = self.station.berth('exec', HOST, '--', 'box', 'publish', topic, json.dumps(body), '--keep', 'log')
        self.assertEqual(done.returncode, 0)
        read = self.link.get(f'/v1/channel?topic={topic}')
        messages = read['messages'] if isinstance(read, dict) else read
        self.assertEqual([message['body'] for message in messages], [body])

    def test_the_agent_sees_its_own_tasks(self):
        listed = self.station.berth('exec', HOST, '--', 'box', 'tasks').stdout
        self.assertIn('sign-in:status', listed)
        self.assertNotIn('reboot', re.findall(r'^\S+', listed, re.M))
        self.assertEqual(self.station.berth('exec', HOST, '--', 'box', 'run', 'sign-in:status').returncode, 0)
