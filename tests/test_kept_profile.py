"""The profile a host keeps: what the seed builder hands the first boot, and
the role's tasks that write it, run with Ansible on a scratch file."""
import importlib.util
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
TASKS = ROOT / 'provision/ansible/roles/devbox/tasks/applied.yml'
KEPT = '/etc/berth/profile.json'


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def playbook():
    """ansible-playbook of the Python that runs the tests, or on the PATH."""
    beside = Path(sys.executable).parent / 'ansible-playbook'
    return str(beside) if beside.is_file() else shutil.which('ansible-playbook')


DOCUMENT = {
    'format': 1, 'berth': '0.0.0', 'host': 'studio',
    'profile': {'hostname': 'studio', 'profile': 'cloud', 'operator_password_hash': '$6$salt$hash',
                'builder': {'ask_pass': False}},
}


class SeedTests(unittest.TestCase):
    def test_the_seed_builder_hands_the_kept_profile_to_the_first_boot(self):
        builder = load('builder', 'provision/bin/devbox.py')
        c = {'profile': 'cloud', 'tailscale_auth_key': 'tskey-once', 'berth_profile': DOCUMENT}
        variables = builder.deployment(c, 'a' * 64)['ansible_vars']
        self.assertEqual(variables['devbox_berth_profile'], DOCUMENT)
        # A profile written without berth keeps nothing.
        self.assertNotIn('devbox_berth_profile', builder.deployment({'profile': 'cloud'}, 'a' * 64)['ansible_vars'])
        # The first boot reads these variables from host.json and strips only the key.
        self.assertNotIn('tskey', json.dumps(variables['devbox_berth_profile']))


@unittest.skipUnless(yaml and playbook(), 'Run with the Ansible venv Python for the role tasks')
class RoleTests(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.dir)
        self.kept = self.dir / 'profile.json'
        tasks = [task for task in yaml.safe_load(TASKS.read_text())
                 if 'devbox_berth_profile' in json.dumps(task.get('when', ''))]
        self.assertEqual(len(tasks), 2)
        for task in tasks:
            module = task.get('ansible.builtin.slurp') or task['ansible.builtin.copy']
            for key in ('src', 'dest'):
                if module.get(key) == KEPT:
                    module[key] = str(self.kept)
            # Root's own file on a host; here the user's.
            module.pop('owner', None)
            module.pop('group', None)
            self.assertTrue(task['no_log'], task['name'])
        (self.dir / 'tasks.yml').write_text(yaml.safe_dump(tasks))
        (self.dir / 'play.yml').write_text(yaml.safe_dump([{
            'hosts': 'localhost', 'gather_facts': False,
            'tasks': [{'ansible.builtin.import_tasks': str(self.dir / 'tasks.yml')}]}]))

    def apply(self, document):
        variables = self.dir / 'vars.json'
        variables.write_text(json.dumps({'devbox_berth_profile': document}))
        done = subprocess.run(
            [playbook(), '-i', 'localhost,', '-c', 'local', str(self.dir / 'play.yml'), '-e', f'@{variables}'],
            capture_output=True, text=True, env=dict(os.environ, ANSIBLE_NOCOLOR='1'))
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        changed = int(done.stdout.split('changed=')[1].split()[0])
        return changed

    def test_the_profile_is_written_privately_and_only_when_it_changes(self):
        self.assertEqual(self.apply(DOCUMENT), 1)
        kept = json.loads(self.kept.read_text())
        written = kept.pop('written')
        self.assertRegex(written, r'^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$')
        self.assertEqual(kept, DOCUMENT)
        self.assertEqual(self.kept.stat().st_mode & 0o777, 0o600)
        # The same profile again leaves the file and its time alone.
        self.assertEqual(self.apply(DOCUMENT), 0)
        self.assertEqual(json.loads(self.kept.read_text())['written'], written)
        changed = json.loads(json.dumps(DOCUMENT))
        changed['profile']['hostname'] = 'atelier'
        self.assertEqual(self.apply(changed), 1)
        self.assertEqual(json.loads(self.kept.read_text())['profile']['hostname'], 'atelier')

    def test_a_broken_file_is_replaced(self):
        self.kept.write_text('{"format": ')
        self.assertEqual(self.apply(DOCUMENT), 1)
        self.assertEqual(json.loads(self.kept.read_text())['host'], 'studio')

    def test_without_a_profile_from_berth_nothing_is_written(self):
        variables = self.dir / 'none.json'
        variables.write_text('{}')
        done = subprocess.run(
            [playbook(), '-i', 'localhost,', '-c', 'local', str(self.dir / 'play.yml'), '-e', f'@{variables}'],
            capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertFalse(self.kept.exists())


if __name__ == '__main__':
    unittest.main()
