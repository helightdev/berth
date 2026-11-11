"""Run with the Ansible venv's Python for template integration checks."""
import json
from pathlib import Path
import subprocess
import tomllib
import unittest

try:
    import jinja2
    import yaml
except ImportError:
    jinja2 = yaml = None

ROLE = Path(__file__).resolve().parents[1] / 'ansible/roles/devbox'


@unittest.skipUnless(jinja2 and yaml, 'Run with the Ansible venv Python for template checks')
class EnvironmentTests(unittest.TestCase):
    def setUp(self):
        self.defaults = yaml.safe_load((ROLE / 'defaults/main.yml').read_text())
        self.templates = jinja2.Environment(
            loader=jinja2.FileSystemLoader(ROLE / 'templates'), trim_blocks=True,
            undefined=jinja2.StrictUndefined)
        self.templates.filters['to_json'] = json.dumps
        self.templates.filters['bool'] = bool
        self.defaults['devbox_home'] = self.templates.from_string(self.defaults['devbox_home']).render(self.defaults)
        self.defaults['devbox_tool_path'] = self.templates.from_string(self.defaults['devbox_tool_path']).render(self.defaults)

    def test_shell_and_service_environment_agree_for_all_profiles(self):
        for profile in ['laptop', 'cloud', 'qemu', 'container']:
            for software in [True, False]:
                with self.subTest(profile=profile, software=software):
                    values = dict(self.defaults, devbox_profile=profile,
                                  devbox_software_rendering=software,
                                  devbox_uid='1001', devbox_bind='127.0.0.1',
                                  devbox_lvp_icd='/usr/share/vulkan/icd.d/lvp_icd.json')
                    shell = self.templates.get_template('environment.j2').render(values)
                    result = subprocess.run(
                        ['/bin/bash', '-c', shell + '\n/usr/bin/env -0'],
                        env={'PATH': '/usr/bin:/bin'}, capture_output=True, check=True)
                    actual = dict(entry.split('=', 1) for entry in
                                  result.stdout.decode().split('\0') if entry)
                    service = dict(line.split('=', 1) for line in
                                   self.templates.get_template('t3.env.j2').render(values).splitlines()
                                   if line and not line.startswith('#'))
                    for key in ['PATH', 'DISPLAY', 'XAUTHORITY', 'DEVBOX_SOFTWARE_RENDERING']:
                        self.assertEqual(actual[key], service[key])
                    self.assertIn('/home/agent/.local/share/mise/shims', actual['PATH'])
                    if software:
                        for key in ['LIBGL_ALWAYS_SOFTWARE', 'GALLIUM_DRIVER',
                                    '__GLX_VENDOR_LIBRARY_NAME', 'VK_DRIVER_FILES']:
                            self.assertEqual(actual[key], service[key])
                        self.assertEqual(actual['GALLIUM_DRIVER'], 'llvmpipe')
                    else:
                        self.assertNotIn('LIBGL_ALWAYS_SOFTWARE', actual)
                        self.assertNotIn('VK_DRIVER_FILES', service)

    def test_custom_agent_paths_reach_shell_and_service(self):
        values=dict(self.defaults,devbox_user='robot',devbox_home='/home/robot',
                    devbox_profile='qemu',devbox_uid='1003',devbox_bind='127.0.0.1',
                    devbox_lvp_icd='/usr/share/vulkan/icd.d/lvp_icd.json')
        values['devbox_tool_path']=values['devbox_tool_path'].replace('/home/agent','/home/robot')
        shell=self.templates.get_template('environment.j2').render(values)
        service=self.templates.get_template('t3.env.j2').render(values)
        self.assertIn('export DEVBOX_USER=robot',shell)
        for rendered in [shell,service]:
            self.assertIn('/home/robot/.config/devbox.Xauthority',rendered)
            self.assertNotIn('/home/agent',rendered)

    def test_tool_categories_select_only_their_runtime_tools(self):
        import copy
        categories = self.defaults['devbox_tool_categories']
        for selected in [None, 'all'] + [category['name'] for category in categories]:
            with self.subTest(selected=selected):
                values = copy.deepcopy(self.defaults)
                expected = {'node'}
                for category in values['devbox_tool_categories']:
                    category['enabled'] = selected == 'all' or category['name'] == selected
                    if category['enabled']:
                        expected.update(category['tools'])
                manifest = tomllib.loads(self.templates.get_template('mise.toml.j2').render(values))
                self.assertEqual(set(manifest['tools']), expected)
                self.assertEqual(manifest['tools']['node'][0], self.defaults['devbox_node_versions'][0])


@unittest.skipUnless(yaml, 'Run with the Ansible venv Python for YAML checks')
class TaskFiles(unittest.TestCase):
    def test_every_task_file_is_valid_yaml(self):
        # `ansible-playbook --syntax-check` does not read task files included at run time.
        for path in sorted(ROLE.parents[1].rglob('*.yml')):
            with self.subTest(path=path.name):
                self.assertIsNotNone(yaml.safe_load(path.read_text()))


if __name__ == '__main__':
    unittest.main()
