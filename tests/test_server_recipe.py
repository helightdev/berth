"""The berth server recipe's container, run end to end by ansible-playbook
against a fake docker: a first run, a run over a container that the
incus-host recipe of berth 0.5.2 made, and a second run that changes nothing."""
import json
import os
import shutil
import socket
import subprocess
import tempfile
import unittest
from pathlib import Path

from test_node import FAKE_DOCKER, FOUND_052, PLAYBOOK, UPGRADE, image, write_program, yaml

SRC = Path(__file__).resolve().parents[1]
RECIPE = SRC / 'recipes/berth-server'
SHARED = SRC / 'recipes/shared/berth-server.yml'

# The tasks of site.yml that run the server, in their order.
SERVER_TASKS = ['See whether this machine runs Incus', "Find the group of Docker's socket", 'Run the berth server']

# A listener on the server's port for the recipe to wait for, as the server in a new container is.
LISTENER = '''
import socket, sys, time
server = socket.socket(); server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
server.bind(("127.0.0.1", int(sys.argv[1]))); server.listen(); server.settimeout(1)
end = time.time() + 30
while time.time() < end:
    try:
        server.accept()[0].close()
    except OSError:
        pass
'''


def free_port():
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        return probe.getsockname()[1]


@unittest.skipUnless(yaml and PLAYBOOK, 'Run with the Ansible venv Python for the playbook')
class ServerRecipe(unittest.TestCase):
    def setUp(self):
        self.scratch = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.scratch)
        bin_dir = self.scratch / 'bin'
        bin_dir.mkdir()
        # Each `docker run` also opens the port, so the recipe finds the server answering.
        write_program(bin_dir / 'docker', FAKE_DOCKER + f'''
if args[:1] == ['run']:
    import subprocess
    subprocess.Popen([sys.executable, '-c', {LISTENER!r}, os.environ['FAKE_PORT']], start_new_session=True,
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
''')
        self.state = self.scratch / 'docker.json'
        self.port = free_port()
        self.incus = self.scratch / 'incus-server.crt'
        (self.scratch / 'docker.sock').write_text('')
        self.env = dict(os.environ, PATH=f'{bin_dir}:{os.environ["PATH"]}', FAKE_DOCKER=str(self.state),
                        FAKE_PORT=str(self.port), DEVBOX_DOCKER=str(bin_dir / 'docker'),
                        DEVBOX_UPGRADE_SETTLE='0', ANSIBLE_NOCOLOR='1', ANSIBLE_LOCALHOST_WARNING='0')
        self.playbook = self.write_playbook()

    def write_playbook(self):
        """The recipe's play with only the tasks that run the server, the
        paths of this machine put in scratch."""
        play = yaml.safe_load((RECIPE / 'site.yml').read_text())[0]
        tasks = [task for task in play['tasks'] if task['name'] in SERVER_TASKS]
        self.assertEqual([task['name'] for task in tasks], SERVER_TASKS)
        tasks[0]['ansible.builtin.stat']['path'] = str(self.incus)
        tasks[1]['ansible.builtin.command'] = f'stat --format %g {self.scratch / "docker.sock"}'
        shared = yaml.safe_load(SHARED.read_text())
        for task in shared:
            # The folders belong to the user 1000, which only root or that user can make them.
            if 'ansible.builtin.file' in task and os.getuid() not in (0, 1000):
                task['ansible.builtin.file'].pop('owner')
                task['ansible.builtin.file'].pop('group')
        (self.scratch / 'shared.yml').write_text(yaml.safe_dump(shared, sort_keys=False))
        tasks[2]['ansible.builtin.include_tasks'] = str(self.scratch / 'shared.yml')
        tasks[2]['vars']['berth_server_helper'] = str(UPGRADE)
        variables = yaml.safe_load((RECIPE / 'vars.yml').read_text())
        variables.update(berth_server_dir=str(self.scratch / 'srv'), berth_server_port=self.port,
                         berth_server_tailscale_serve=False, tailscale_hostname='pier')
        playbook = self.scratch / 'site.yml'
        playbook.write_text(yaml.safe_dump([{
            'name': play['name'], 'hosts': 'all', 'gather_facts': False, 'vars': variables, 'tasks': tasks}],
            sort_keys=False))
        return playbook

    def docker(self, found=None):
        new = image('0.6.0', 'sha256:new')
        images = {'new': new, 'sha256:new': new, 'ghcr.io/helightdev/berth': new}
        containers = {}
        if found:
            images.update({found['Image']: FOUND_052['image'], found['Config']['Image']: FOUND_052['image']})
            containers['berth'] = found
        self.state.write_text(json.dumps({'containers': containers, 'images': images}))

    def apply(self, *extra):
        data = json.loads(self.state.read_text())
        data['calls'] = []
        self.state.write_text(json.dumps(data))
        done = subprocess.run(
            [PLAYBOOK, '--connection', 'local', '--inventory', 'localhost,', str(self.playbook), *extra],
            capture_output=True, text=True, env=self.env, timeout=300)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        return done, json.loads(self.state.read_text())

    @staticmethod
    def runs(data):
        return [call for call in data['calls'] if call[0] == 'run']

    def test_an_update_over_the_incus_host_recipes_container_keeps_what_it_reached(self):
        self.docker(found=FOUND_052['container'])
        done, data = self.apply('-e', 'berth_server_node=true', '-e', 'berth_server_update=true')
        [run] = self.runs(data)
        words = ' '.join(run)
        for kept in ('--add-host incus-host:host-gateway', '--add-host node:host-gateway',
                     '--volume /var/run/docker.sock:/var/run/docker.sock', '--group-add 989',
                     '--log-opt max-size=10m', '--restart unless-stopped',
                     f'--publish 127.0.0.1:{self.port}:7777', f'--volume {self.scratch}/srv/data:/data'):
            self.assertIn(kept, words)
        self.assertNotIn('/srv/berth/data:/data', words)
        self.assertIn(['pull', 'ghcr.io/helightdev/berth'], data['calls'])
        self.assertEqual(run[-1], 'ghcr.io/helightdev/berth')
        # The server keeps its token, so no setup key is made for a first user.
        self.assertNotIn(['exec', 'berth', 'berth', 'server', 'setup-key'], data['calls'])

        # The same run again finds everything in place.
        done, data = self.apply('-e', 'berth_server_node=true', '-e', 'berth_server_update=true')
        self.assertEqual(self.runs(data), [])
        self.assertRegex(done.stdout, r'localhost\s*: ok=\d+\s+changed=0\b')

    def test_a_run_without_an_update_keeps_the_image(self):
        self.docker(found=FOUND_052['container'])
        done, data = self.apply()
        self.assertNotIn('pull', [call[0] for call in data['calls']])
        [run] = self.runs(data)
        self.assertEqual(run[-1], 'ghcr.io/helightdev/berth:0.5.2')
        self.assertIn('--add-host incus-host:host-gateway', ' '.join(run))

    def test_a_first_run_on_a_machine_with_incus_gives_the_name_incus_host(self):
        self.incus.write_text('made-up certificate')
        self.docker()
        done, data = self.apply()
        [run] = self.runs(data)
        words = ' '.join(run)
        for given in ('--name berth', '--add-host incus-host:host-gateway',
                      '--volume /var/run/docker.sock:/var/run/docker.sock', f'--publish 127.0.0.1:{self.port}:7777'):
            self.assertIn(given, words)
        self.assertNotIn('node:host-gateway', words)
        self.assertIn(['exec', 'berth', 'berth', 'server', 'setup-key'], data['calls'])
        # Then it is in place.
        done, data = self.apply()
        self.assertEqual(self.runs(data), [])
        self.assertRegex(done.stdout, r'localhost\s*: ok=\d+\s+changed=0\b')


if __name__ == '__main__':
    unittest.main()
