"""A variable a task registers outranks the vars of a later task with the
same name, so a task's vars never take the name of a registered result."""
from pathlib import Path
import unittest

try:
    import yaml
except ImportError:
    yaml = None

ROOT = Path(__file__).resolve().parents[1]


def tasks(node):
    """Every task in a list of tasks, blocks opened."""
    for task in node or []:
        if not isinstance(task, dict):
            continue
        yield task
        for key in ('block', 'rescue', 'always'):
            yield from tasks(task.get(key))


@unittest.skipUnless(yaml, 'needs PyYAML: install provision/requirements.txt')
class TaskNames(unittest.TestCase):
    def test_no_task_vars_take_the_name_of_a_registered_result(self):
        files = list(ROOT.glob('plugins/*/tasks/*.yml')) + list(ROOT.glob('provision/ansible/roles/*/tasks/*.yml'))
        self.assertTrue(files)
        clashes = []
        for path in files:
            listed = list(tasks(yaml.safe_load(path.read_text())))
            registered = {task['register'] for task in listed if 'register' in task}
            for task in listed:
                for name in (task.get('vars') or {}):
                    if name in registered:
                        clashes.append(f"{path.relative_to(ROOT)}: {task.get('name')}: {name}")
        self.assertEqual(clashes, [])
