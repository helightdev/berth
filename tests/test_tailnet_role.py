"""How the role joins a host to the tailnet: only with a key berth made,
never by hand, and never again for a host that is on it. The check and the
join run with Ansible against a stand-in for the tailscale client."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

try:
    import yaml
except ImportError:
    yaml = None

ROOT = Path(__file__).resolve().parents[1]
ROLE = ROOT / 'provision/ansible/roles/devbox'
RUNNING = '{"BackendState": "Running", "Self": {"DNSName": "studio.tailnet.test."}}'
LOGGED_OUT = '{"BackendState": "NeedsLogin"}'


def playbook():
    """ansible-playbook of the Python that runs the tests, or on the PATH."""
    beside = Path(sys.executable).parent / 'ansible-playbook'
    return str(beside) if beside.is_file() else shutil.which('ansible-playbook')


@unittest.skipUnless(yaml, 'needs PyYAML: install provision/requirements.txt')
class RoleTextTests(unittest.TestCase):
    def tasks(self, name):
        return yaml.safe_load((ROLE / 'tasks' / name).read_text())

    def test_there_is_no_way_to_join_by_hand(self):
        text = ''.join((ROLE / 'tasks' / name).read_text() for name in ('tailnet.yml', 'tailnet_check.yml'))
        for gone in ('tailscale logout', 'tailscale login', 'needs-enrollment', 'TS_API_KEY'):
            self.assertNotIn(gone, text)
        defaults = yaml.safe_load((ROLE / 'defaults/main.yml').read_text())
        for name in ('devbox_tailscale_auth_key', 'devbox_tailscale_login_server', 'devbox_tailscale_key_command',
                     'devbox_tailnet_problem'):
            self.assertIn(name, defaults)

    def test_the_key_never_reaches_argv_or_the_log(self):
        tasks = self.tasks('tailnet.yml')
        named = {task['name']: task for task in tasks}
        self.assertTrue(named['Create a one-use enrollment key with berth']['no_log'])
        self.assertTrue(named['Choose the enrollment key']['no_log'])
        enroll = named['Enroll Tailscale only when not already enrolled']['block']
        stored = next(task for task in enroll if 'ansible.builtin.copy' in task)
        self.assertTrue(stored['no_log'])
        self.assertEqual(stored['ansible.builtin.copy']['mode'], '0600')
        up = next(task for task in enroll if task['name'] == 'Consume enrollment key')
        argv = up['ansible.builtin.command']['argv']
        self.assertIn('--auth-key=file:/run/devbox-tailscale-key', argv)
        self.assertNotIn('devbox_tailnet_key', argv)
        cleanup = named['Enroll Tailscale only when not already enrolled']['always'][0]
        self.assertEqual(cleanup['ansible.builtin.file']['state'], 'absent')

    def test_the_check_comes_before_any_change(self):
        main = self.tasks('main.yml')
        self.assertIn('ansible.builtin.assert', main[0])
        self.assertEqual(main[1]['ansible.builtin.include_tasks'], 'tailnet_check.yml')
        self.assertIn('devbox_install_tailscale', main[1]['when'])
        e2e = yaml.safe_load((ROOT / 'tests/e2e/site.yml').read_text())[0]['tasks']
        self.assertEqual(e2e[1]['ansible.builtin.include_role']['tasks_from'], 'tailnet_check.yml')


@unittest.skipUnless(yaml and playbook(), 'Run with the Ansible venv Python for the role tasks')
class TailnetTaskTests(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.dir)
        bin_dir = self.dir / 'bin'
        bin_dir.mkdir()
        self.calls = self.dir / 'calls'
        self.status = self.dir / 'status.json'
        # The client says what status.json holds and notes every call.
        tailscale = bin_dir / 'tailscale'
        tailscale.write_text(f'#!/bin/sh\necho "$*" >> {self.calls}\n'
                             f'[ "$1" = status ] && cat {self.status}\nexit 0\n')
        tailscale.chmod(0o755)
        self.path = f'{bin_dir}:{os.environ["PATH"]}'
        self.made = self.dir / 'made'

    def run_tasks(self, name, status, **variables):
        self.status.write_text(status)
        (self.dir / 'play.yml').write_text(yaml.safe_dump([{
            'hosts': 'localhost', 'gather_facts': False,
            'vars': {'devbox_tailscale_auth_key': '', 'devbox_tailscale_login_server': '',
                     'devbox_tailscale_key_command': [], 'devbox_tailnet_problem': '', **variables},
            'tasks': [{'ansible.builtin.include_tasks': str(ROLE / 'tasks' / name)}]}]))
        return subprocess.run(
            [playbook(), '-i', 'localhost,', '-c', 'local', str(self.dir / 'play.yml')],
            capture_output=True, text=True, env=dict(os.environ, PATH=self.path, ANSIBLE_NOCOLOR='1'))

    def calls_made(self):
        return self.calls.read_text().splitlines() if self.calls.exists() else []

    def test_a_host_off_the_tailnet_without_a_way_to_a_key_stops_with_berths_words(self):
        problem = 'studio joins the tailnet at Tailscale, and berth cannot make its key. berth secret set tailnet_api_key'
        done = self.run_tasks('tailnet_check.yml', LOGGED_OUT, devbox_tailnet_problem=problem)
        self.assertNotEqual(done.returncode, 0)
        self.assertIn(problem, done.stdout)
        done = self.run_tasks('tailnet_check.yml', '')
        self.assertNotEqual(done.returncode, 0)
        self.assertIn('secret tailnet_api_key', done.stdout)
        self.assertIn('devbox_install_tailscale to false', done.stdout)

    def test_a_way_to_a_key_lets_the_play_go_on(self):
        command = ['touch', str(self.made)]
        self.assertEqual(self.run_tasks('tailnet_check.yml', LOGGED_OUT,
                                        devbox_tailscale_key_command=command).returncode, 0)
        self.assertEqual(self.run_tasks('tailnet_check.yml', LOGGED_OUT,
                                        devbox_tailscale_auth_key='tskey-auth-kFAKE-0000').returncode, 0)
        # The check makes no key.
        self.assertFalse(self.made.exists())

    def test_a_host_on_the_tailnet_is_left_alone_even_without_a_key(self):
        problem = 'berth cannot make its key'
        done = self.run_tasks('tailnet.yml', RUNNING, devbox_tailnet_problem=problem)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        done = self.run_tasks('tailnet.yml', RUNNING, devbox_tailscale_key_command=['touch', str(self.made)])
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        # No key is made, and the client is only asked how it is.
        self.assertFalse(self.made.exists())
        self.assertEqual(set(self.calls_made()), {'status --json'})


if __name__ == '__main__':
    unittest.main()
