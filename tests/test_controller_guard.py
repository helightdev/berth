"""The guard that keeps installing the controller alone off a workstation.

It runs the role's guard tasks alone, against this machine with a
connection of its own and a scratch folder for berthd's files, so nothing
of the machine is read or changed. Skipped without ansible-playbook.
"""
import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROLE = Path(__file__).resolve().parents[1] / 'provision/ansible/roles/devbox'
PLAYBOOK = shutil.which('ansible-playbook')


@unittest.skipUnless(PLAYBOOK, 'ansible-playbook is not installed')
class ControllerGuardTests(unittest.TestCase):
    def run_guard(self, files):
        with tempfile.TemporaryDirectory() as scratch:
            scratch = Path(scratch)
            etc = scratch / 'etc'
            etc.mkdir()
            for name, value in files.items():
                (etc / f'{name}.json').write_text(json.dumps(value))
            before = {path.name: path.read_bytes() for path in etc.iterdir()}
            playbook = scratch / 'guard.yml'
            playbook.write_text(
                '- hosts: all\n'
                '  gather_facts: false\n'
                '  tasks:\n'
                f'    - ansible.builtin.import_tasks: {ROLE}/tasks/controller_guard.yml\n')
            env = dict(os.environ, ANSIBLE_LOCAL_TEMP=str(scratch / 'tmp'),
                       ANSIBLE_REMOTE_TEMP=str(scratch / 'tmp'), ANSIBLE_NOCOLOR='1',
                       ANSIBLE_HOME=str(scratch / 'home'), ANSIBLE_RETRY_FILES_ENABLED='0')
            done = subprocess.run(
                [PLAYBOOK, '-i', 'studio,', '-c', 'local', str(playbook),
                 '-e', json.dumps({'devbox_controller_etc': str(etc),
                                   'devbox_controller_user': 'ops',
                                   'ansible_python_interpreter': shutil.which('python3')})],
                capture_output=True, text=True, env=env, timeout=120)
            after = {path.name: path.read_bytes() for path in etc.iterdir()}
            self.assertEqual(before, after)
            return done.returncode, done.stdout + done.stderr

    def test_a_fresh_host_and_a_controller_go_on(self):
        code, said = self.run_guard({})
        self.assertEqual(code, 0, said)
        code, said = self.run_guard({'applied': {'berth': '0.5.0', 'plugins': [], 'level': 'controller'},
                                     'berthd': {'agent': 'ops', 'operator': 'ops'}})
        self.assertEqual(code, 0, said)

    def test_a_workstation_is_refused(self):
        code, said = self.run_guard({'applied': {'berth': '0.5.0', 'plugins': []},
                                     'berthd': {'agent': 'ops'}})
        self.assertNotEqual(code, 0)
        self.assertIn('is a workstation that berth set up', said)
        code, said = self.run_guard({'applied': {'berth': '0.5.0', 'plugins': [], 'level': 'workstation'}})
        self.assertNotEqual(code, 0)

    def test_a_controller_that_serves_another_account_is_refused(self):
        code, said = self.run_guard({'berthd': {'agent': 'agent', 'operator': 'ops'}})
        self.assertNotEqual(code, 0)
        self.assertIn('serves the agent account agent', said)


if __name__ == '__main__':
    unittest.main()
