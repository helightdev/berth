"""Run with the Ansible venv's Python for template integration checks."""
import json
import re
import shlex
import shutil
import tempfile
from pathlib import Path
import subprocess
import tomllib
import unittest

try:
    import jinja2
    import yaml
except ImportError:
    jinja2 = yaml = None

ROLE = Path(__file__).resolve().parents[1] / 'provision/ansible/roles/devbox'


@unittest.skipUnless(jinja2 and yaml, 'Run with the Ansible venv Python for template checks')
class EnvironmentTests(unittest.TestCase):
    def setUp(self):
        self.defaults = yaml.safe_load((ROLE / 'defaults/main.yml').read_text())
        self.templates = jinja2.Environment(
            loader=jinja2.FileSystemLoader(ROLE / 'templates'), trim_blocks=True,
            undefined=jinja2.StrictUndefined)
        self.templates.filters['to_json'] = json.dumps
        self.templates.filters['bool'] = bool
        self.templates.tests['match'] = lambda value, pattern: re.match(pattern, value) is not None
        self.defaults['devbox_privileges'] = 'full'
        self.defaults['devbox_home'] = self.templates.from_string(self.defaults['devbox_home']).render(self.defaults)
        self.defaults['devbox_tool_path'] = self.templates.from_string(self.defaults['devbox_tool_path']).render(self.defaults)

    def test_shell_and_service_environment_agree_for_all_profiles(self):
        for profile in ['laptop', 'cloud', 'container']:
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
                    devbox_profile='cloud',devbox_uid='1003',devbox_bind='127.0.0.1',
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


    def feature(self, description, plugin='demo', label='Demo'):
        text = self.templates.get_template('berth-feature.json.j2').render(
            devbox_feature=description, devbox_plugin_id=plugin, devbox_plugin_label=label)
        return json.loads(text)

    def test_a_feature_becomes_tasks_services_and_topics(self):
        adds = self.feature({'id': 'main', 'actions': [
            {'name': 'hello', 'run': 'echo "$BERTH_ARG_WHO" \'x\'', 'agent': True,
             'arguments': [{'name': 'who', 'required': True}]},
            {'name': 'part', 'extends': 'status', 'user': 'root', 'run': 'true'},
            {'name': 'wipe', 'group': 'System', 'terminal': True, 'confirm': 'Sure?',
             'extends': ['a', 'b'], 'user': 'nobody', 'run': 'true'},
            {'name': 'Bad Name', 'run': 'true'}, {'name': 'no-command'}],
            'services': [{'name': 'manager', 'user': 'root', 'agent': True, 'run': '/opt/m',
                          'methods': [{'name': 'start'}]},
                         {'name': 'helper', 'run': 'box serve demo/helper -- /opt/h', 'timeout': 5},
                         {'name': 'nothing-to-run'}],
            'channels': [{'topic': 'progress', 'keep': 'none', 'max': 1000, 'days': 30}, {'topic': 'log'}]})
        tasks = {task['name']: task for task in adds['tasks']}
        self.assertEqual(list(tasks), ['demo:hello', 'demo:part', 'demo:wipe'])
        hello = tasks['demo:hello']
        self.assertEqual((hello['user'], hello['agent'], hello['group'], hello['extends'], hello['label']),
                         ('agent', True, 'Demo', [], 'hello'))
        self.assertEqual(hello['run'], 'echo "$BERTH_ARG_WHO" \'x\'')
        self.assertEqual(hello['arguments'], [{'name': 'who', 'required': True}])
        self.assertEqual((tasks['demo:part']['extends'], tasks['demo:part']['user']), (['status'], 'root'))
        wipe = tasks['demo:wipe']
        self.assertEqual((wipe['group'], wipe['terminal'], wipe['confirm'], wipe['extends'], wipe['user'], wipe['agent']),
                         ('System', True, 'Sure?', ['a', 'b'], 'agent', False))
        self.assertEqual([(s['name'], s['user'], s['agent'], s['timeout']) for s in adds['services']],
                         [('demo/manager', 'root', True, 30), ('demo/helper', 'agent', False, 5)])
        self.assertEqual(adds['channels'], [
            {'topic': 'demo/progress', 'keep': 'none', 'max': 1000, 'days': 30},
            {'topic': 'demo/log', 'keep': None, 'max': None, 'days': None}])
        self.assertEqual(self.feature({'id': 'bare'}), {'tasks': [], 'services': [], 'channels': []})

    def test_shipped_plugins_make_valid_tasks(self):
        plugins = ROLE.parents[3] / 'plugins'
        names = []
        for package in sorted(plugins.glob('*/package.json')):
            data = json.loads(package.read_text())
            for feature in data['berth'].get('features', []):
                adds = self.feature(feature, package.parent.name, data.get('displayName', package.parent.name))
                self.assertEqual(len(adds['tasks']), len(feature.get('actions', [])), package)
                names += [task['name'] for task in adds['tasks']]
        self.assertEqual(len(names), len(set(names)))
        self.assertIn('backup:restore', names)
        self.assertIn('claude:login', names)

    def test_core_tasks_name_installed_scripts(self):
        tasks = yaml.safe_load((ROLE / 'vars/main.yml').read_text())['devbox_core_tasks']
        names = [task['name'] for task in tasks]
        self.assertEqual(names, ['status', 'smoke', 'update', 'reboot', 'access', 'publish', 'pair',
                                 'secrets', 'sign-in:status'])
        for task in tasks:
            with self.subTest(task=task['name']):
                self.assertIn(task['user'], ['agent', 'root'])
                self.assertTrue(task['label'] and task['help'] and task['group'])
                if task['run'].startswith('{{ devbox_task_dir }}/'):
                    self.assertTrue((ROLE / 'files/tasks' / task['run'].rsplit('/', 1)[1]).is_file())
        by_name = {task['name']: task for task in tasks}
        self.assertEqual(by_name['secrets']['input'], 'secrets')
        self.assertTrue(by_name['reboot']['confirm'])
        # Only what runs as the agent is open to the agent.
        self.assertEqual({name for name, task in by_name.items() if task.get('agent')}, {'smoke', 'sign-in:status'})

    def test_sudo_rules_for_each_privilege_level(self):
        visudo = shutil.which('visudo') or ('/usr/sbin/visudo' if Path('/usr/sbin/visudo').exists() else None)
        for level in ['full', 'password', 'limited', 'none']:
            with self.subTest(level=level):
                text = self.templates.get_template('sudoers-agent.j2').render(
                    devbox_user='agent', devbox_privileges=level,
                    devbox_agent_units=['berth-svc-demo-helper.service'])
                rules = [line for line in text.splitlines() if line and not line.startswith(('#', 'Defaults'))]
                if level == 'full':
                    self.assertEqual(rules, ['agent ALL=(ALL:ALL) NOPASSWD: ALL'])
                elif level == 'password':
                    self.assertEqual(rules, ['agent ALL=(ALL:ALL) ALL'])
                elif level == 'none':
                    self.assertEqual(rules, [])
                else:
                    self.assertTrue(all(rule.startswith('agent ALL=(root) NOPASSWD: /usr/bin/') for rule in rules))
                    self.assertNotIn('ALL=(ALL', text)
                    self.assertIn('/usr/bin/apt-get install *', text)
                    self.assertIn('/usr/bin/systemctl restart berth-svc-demo-helper.service', text)
                    # Nothing but package commands and the named units.
                    commands = [command.strip() for rule in rules for command in rule.split('NOPASSWD:')[1].split(',')]
                    for command in commands:
                        self.assertRegex(command, r'^/usr/bin/(apt|apt-get) (-y )?(update|install \*|(upgrade|dist-upgrade|full-upgrade|autoremove)( -y)?)$'
                                                  r'|^/usr/bin/systemctl restart berth-svc-[a-z0-9_-]+\.service$')
                if visudo:
                    with tempfile.NamedTemporaryFile('w', suffix='.sudoers') as file:
                        file.write(text)
                        file.flush()
                        subprocess.run([visudo, '-cf', file.name], check=True, capture_output=True)

    def test_units_for_berthd_and_plugin_services(self):
        unit = self.templates.get_template('berthd.service.j2').render()
        self.assertIn('ExecStart=/usr/local/bin/berthd\n', unit)
        self.assertIn('ExecReload=/bin/kill -HUP $MAINPID', unit)
        values = dict(devbox_user='agent', devbox_home='/home/agent', devbox_service_dir='/usr/local/lib/berth/services')
        as_agent = self.templates.get_template('berth-service.service.j2').render(
            values, item={'name': 'demo/helper', 'user': 'agent'})
        self.assertIn('User=agent', as_agent)
        self.assertIn('ExecStart=/usr/local/bin/agent-env /bin/bash /usr/local/lib/berth/services/demo-helper.sh', as_agent)
        as_root = self.templates.get_template('berth-service.service.j2').render(
            values, item={'name': 'demo/manager', 'user': 'root'})
        self.assertNotIn('User=', as_root)
        self.assertIn('After=berthd.service', as_root)

def apparmor_glob(pattern):
    """A regular expression for an AppArmor attachment with * and {a,b}."""
    out = ''
    for part in re.split(r'(\*\*|\*|\{[^}]*\})', pattern):
        if part == '**':
            out += '.*'
        elif part == '*':
            out += '[^/]*'
        elif part.startswith('{'):
            out += '(' + '|'.join(re.escape(choice) for choice in part[1:-1].split(',')) + ')'
        else:
            out += re.escape(part)
    return re.compile(out + '$')


@unittest.skipUnless(jinja2 and yaml, 'Run with the Ansible venv Python for template checks')
class ChromiumSandboxTests(unittest.TestCase):
    CACHE = '.cache/ms-playwright'
    BROWSER = 'chromium-1254/chrome-linux64'
    HEADLESS = 'chromium_headless_shell-1254/chrome-headless-shell-linux64'

    def task(self, file, name):
        tasks = yaml.safe_load((ROLE / 'tasks' / file).read_text())
        return next(task for task in tasks if task['name'] == name)

    def test_the_apparmor_profile_covers_both_playwright_browsers(self):
        content = self.task('host.yml', "Allow Playwright's Chromium to sandbox itself")['ansible.builtin.copy']['content']
        content = jinja2.Template(content).render(devbox_home='/home/agent')
        attach = re.search(r'^profile \S+ (\S+) flags', content, re.M).group(1)
        matches = apparmor_glob(attach).match
        for binary in [f'{self.BROWSER}/chrome', f'{self.HEADLESS}/chrome-headless-shell',
                       'chromium_headless_shell-1148/chrome-linux/headless_shell']:
            self.assertTrue(matches(f'/home/agent/{self.CACHE}/{binary}'), binary)
        for other in [f'{self.BROWSER}/chrome_crashpad_handler', f'{self.BROWSER}/chrome-sandbox',
                      'firefox-1490/firefox/firefox']:
            self.assertFalse(matches(f'/home/agent/{self.CACHE}/{other}'), other)

    def test_the_profile_is_loaded_only_through_the_machines_own_apparmor(self):
        load = self.task('host.yml', 'Load the Chromium AppArmor profile')
        self.assertIn('devbox_apparmor_interface.stat.exists', load['when'])
        check = self.task('host.yml', "Check for an AppArmor interface of this machine's own")
        self.assertEqual(check['ansible.builtin.stat']['path'], '/sys/kernel/security/apparmor')

    def helper_script(self, home):
        task = self.task('applications.yml', "Give Playwright's Chromium its sandbox helper in a container")
        self.assertIn('devbox_container_kind.rc == 0', task['when'])
        templates = jinja2.Environment(undefined=jinja2.StrictUndefined)
        templates.filters['quote'] = shlex.quote
        return templates.from_string(task['ansible.builtin.shell']).render(devbox_home=str(home))

    def test_containers_get_the_setuid_helper_beside_both_browsers_once(self):
        with tempfile.TemporaryDirectory() as temp:
            temp = Path(temp)
            home, shims = temp / 'home', temp / 'bin'
            cache = home / self.CACHE
            for path, text in [(f'{self.BROWSER}/chrome', 'browser'), (f'{self.BROWSER}/chrome_sandbox', 'helper'),
                               (f'{self.HEADLESS}/chrome-headless-shell', 'shell'),
                               ('chromium_headless_shell-1200/chrome-headless-shell-linux64/chrome-headless-shell', 'old')]:
                (cache / path).parent.mkdir(parents=True, exist_ok=True)
                (cache / path).write_text(text)
            shims.mkdir()
            # Owner and mode are recorded beside the file, since the test does not run as root.
            (shims / 'install').write_text('#!/bin/bash\n[[ "${*:1:6}" == "-o root -g root -m 4755" ]] || exit 9\n'
                                           'cp "$7" "$8" && echo 0:0:4755 > "$8.owner"\n')
            (shims / 'stat').write_text('#!/bin/bash\ncat "$3.owner"\n')
            for shim in shims.iterdir():
                shim.chmod(0o755)
            script = self.helper_script(home)

            def run():
                return subprocess.run(['/bin/bash', '-c', script], env={'PATH': f'{shims}:/usr/bin:/bin'},
                                      capture_output=True, text=True, check=True).stdout.split()

            placed = [str(cache / self.BROWSER / 'chrome-sandbox'), str(cache / self.HEADLESS / 'chrome-sandbox')]
            self.assertEqual(run(), placed)
            for path in placed:
                self.assertEqual(Path(path).read_text(), 'helper')
            self.assertFalse((cache / 'chromium_headless_shell-1200/chrome-headless-shell-linux64/chrome-sandbox').exists())
            self.assertEqual(run(), [])
            Path(placed[1] + '.owner').write_text('1000:1000:755\n')
            self.assertEqual(run(), placed[1:])

    def test_a_sandbox_that_cannot_start_is_reported_in_plain_words(self):
        smoke = (ROLE / 'files/smoke.sh').read_text()
        functions = ''.join(re.search(rf'^{name}\(\) {{\n.*?^}}\n', smoke, re.M | re.S).group(0)
                            for name in ['machine_kind', 'browser_failed'])
        with tempfile.TemporaryDirectory() as temp:
            temp = Path(temp)
            (temp / 'systemd-detect-virt').write_text('#!/bin/bash\n[[ $1 == --container ]] && echo lxc\n')
            (temp / 'systemd-detect-virt').chmod(0o755)
            log = temp / 'browser-error.txt'

            def report(text):
                log.write_text(text)
                return subprocess.run(['/bin/bash', '-c', functions + f'browser_failed {log}'],
                                      env={'PATH': f'{temp}:/usr/bin:/bin'}, capture_output=True, text=True)

            failed = report('Traceback (most recent call last):\n  [FATAL:zygote_host_impl_linux.cc:144] No usable sandbox! If\n')
            self.assertEqual(failed.returncode, 1)
            self.assertEqual(failed.stderr.splitlines(), [
                'Chromium could not start its sandbox on this machine, a container (lxc).',
                f"The browser check failed. Chromium's output is in {log}"])
            other = report('AssertionError: the page lost its input\n')
            self.assertEqual(other.returncode, 1)
            self.assertEqual(other.stderr, 'AssertionError: the page lost its input\n')


@unittest.skipUnless(yaml, 'Run with the Ansible venv Python for YAML checks')
class TaskFiles(unittest.TestCase):
    def test_every_task_file_is_valid_yaml(self):
        # `ansible-playbook --syntax-check` does not read task files included at run time.
        for path in sorted(ROLE.parents[1].rglob('*.yml')):
            with self.subTest(path=path.name):
                self.assertIsNotNone(yaml.safe_load(path.read_text()))


if __name__ == '__main__':
    unittest.main()
