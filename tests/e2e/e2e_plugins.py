"""A plugin from a local npm-format package: installed into berth, its
feature applied to the host, used there, and removed again.

Plugin uses the extended state of the host (harness.extended()), which adds
this plugin and applies its feature, and PluginRemoved the plain state
(harness.plain()), which removes it and applies without it."""
import json
import unittest

from harness import AGENT, HOST, extended, needs_e2e, plain, random_text, wait_for

PACKAGE = 'berth-plugin-e2e'


@needs_e2e
class Plugin(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.station = extended().station
        cls.link = cls.station.link()

    def test_berth_lists_it_with_what_it_brings(self):
        listed = self.station.berth('plugins').stdout
        self.assertRegex(listed, rf'(?m)^e2e  {PACKAGE} 1\.0\.0  ')
        self.assertIn('e2e/hello  Hello (Testing)', listed)
        self.assertIn('task     e2e:hello  Say hello (as agent; the agent may run it; asks name)', listed)
        self.assertIn('service  e2e/echo  as agent: say', listed)
        self.assertIn('channel  e2e/notes', listed)

    def test_its_task_runs_with_an_argument(self):
        self.assertRegex(self.station.berth('tasks', HOST).stdout, r'e2e:hello\s+Say hello\s+name=…')
        done = self.station.berth('run', HOST, 'e2e:hello', 'name=ada')
        self.assertEqual(done.stdout.strip(), f'hello ada from {AGENT}')
        missing = self.station.berth('run', HOST, 'e2e:hello', check=False)
        self.assertIn('e2e:hello needs name=VALUE', missing.stderr)
        choices = self.link.get('/v1/tasks/e2e:hello/choices/name')
        self.assertEqual(choices['choices'], ['ada', 'grace'])
        # The agent may run it too.
        done = self.station.berth('exec', HOST, '--', 'box', 'run', 'e2e:hello', 'name=grace')
        self.assertIn(f'hello grace from {AGENT}', done.stdout)

    def test_a_step_joins_the_task_it_extends(self):
        done = self.station.berth('run', HOST, 'sign-in:status')
        self.assertIn('e2e plugin: signed in', done.stdout)

    def test_its_ansible_tasks_and_guide_are_applied(self):
        self.station.root_on_host('test -x /usr/local/bin/berth-e2e-echo')
        guide = self.station.root_on_host('cat /etc/devbox/guides/e2e-hello.md').stdout
        self.assertIn('box run e2e:hello', guide)

    def test_its_service_answers_once_something_serves_it(self):
        refused = self.station.berth('call', HOST, 'e2e/echo', 'say', '{}', check=False)
        self.assertNotEqual(refused.returncode, 0)
        # systemd would run the service; here the tests do, as the agent.
        served = self.link.post('/v1/sessions', {
            'kind': 'process', 'title': 'e2e service',
            'command': ['bash', '/usr/local/lib/berth/services/e2e-echo.sh']})
        try:
            def answered():
                done = self.station.berth('call', HOST, 'e2e/echo', 'say', '{"text": "hi"}', check=False)
                return done.returncode == 0 and json.loads(done.stdout)
            self.assertEqual(wait_for('the service answering', answered),
                             {'method': 'say', 'body': {'text': 'hi'}, 'level': 'host'})
            from_agent = self.station.berth('exec', HOST, '--', 'box', 'call', 'e2e/echo', 'say', '[1]').stdout
            self.assertEqual(json.loads(from_agent), {'method': 'say', 'body': [1], 'level': 'agent'})
            services = self.station.berth('exec', HOST, '--', 'box', 'services').stdout
            self.assertIn('e2e/echo', services)
        finally:
            self.link.post(f'/v1/sessions/{served["id"]}/signal', {'signal': 'TERM'})

    def test_its_topic_keeps_as_many_as_it_says(self):
        for number in range(7):
            body = json.dumps({'n': number, 'id': random_text('note')})
            self.station.berth('exec', HOST, '--', 'box', 'publish', 'e2e/notes', body)
        read = self.link.get('/v1/channel?topic=e2e/notes')
        messages = read['messages'] if isinstance(read, dict) else read
        kept = [message['body']['n'] for message in messages if message['topic'] == 'e2e/notes']
        self.assertEqual(kept, [2, 3, 4, 5, 6])


@needs_e2e
class PluginRemoved(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.station = plain().station

    def test_its_tasks_are_gone_from_berth_and_the_host(self):
        self.assertNotIn('e2e/hello', self.station.berth('plugins').stdout)
        self.assertNotIn('e2e:hello', self.station.berth('tasks', HOST).stdout)
        services = json.loads(self.station.root_on_host('cat /etc/berth/services.json').stdout)
        self.assertEqual(services, [])
