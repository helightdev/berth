"""Provisioners as subjects of their own, with the real berth: a made-up
provisioner of a plugin in the workstation's own plugin folder, its page, its
settings and its actions, from the command line and through berth serve. It
needs no test host, and runs on `dock` beside the modules there.

With BERTH_E2E_DOCKER_HOST set to a Docker engine the runner may use, such as
tcp://docker:2375, the Docker plugin's own actions are asked of it too. The
test network of ci/compose.yml has no such engine yet, so that case is skipped
there."""
import json
import os
import stat
import unittest

from harness import Serve, Workstation, needs_e2e

# A provisioner that answers from what it is asked: it writes each request,
# secrets left out, to requests.jsonl of its folder.
PROGRAM = r'''#!/usr/bin/env python3
import json, sys, time
verb = sys.argv[1]
request = json.load(sys.stdin)
with open('requests.jsonl', 'a') as log:
    log.write(json.dumps({k: v for k, v in request.items() if k != 'secrets'}) + '\n')
if request.get('version') != 2:
    print('speaks version 2 only', file=sys.stderr)
    sys.exit(1)
place = request['arguments'].get('place') or 'nowhere'
if verb == 'status':
    print(json.dumps({'message': f'The lab at {place} answers',
                      'details': [{'label': 'Benches', 'value': '3'}],
                      'values': {'shelf': ['top', 'bottom']}}))
elif verb == 'list':
    print(json.dumps({'resources': [{'id': 'box-1', 'kind': 'machine', 'name': 'attic', 'state': 'running'}]}))
elif verb == 'act':
    action = request['action']
    if action['name'] == 'look':
        print(json.dumps({'message': f'Looked around {place}', 'details': [{'label': 'Dust', 'value': 'little'}]}))
    else:
        print(f'sweeping {action["arguments"].get("shelf")} with {action["arguments"].get("code")}', file=sys.stderr)
        time.sleep(0.5)
        print(json.dumps({'message': 'Swept'}))
else:
    print(f'no verb {verb}', file=sys.stderr)
    sys.exit(1)
'''

MANIFEST = {
    'name': 'berth-plugin-lab', 'version': '1.0.0', 'displayName': 'Lab',
    'berth': {'provisioners': [{
        'id': 'box', 'label': 'Lab box', 'command': 'bin/provision', 'protocol': 2, 'verbs': ['list'],
        'arguments': [{'name': 'place', 'label': 'Place', 'required': True}],
        'kinds': [{'id': 'machine', 'setup': 'apply'}],
        'actions': [
            {'name': 'look', 'label': 'Look around', 'changes': False},
            {'name': 'sweep', 'label': 'Sweep', 'confirm': 'Sweep the lab?',
             'arguments': [{'name': 'shelf', 'values': ['top', 'bottom'], 'default': 'top'},
                           {'name': 'code', 'secret': True}]},
        ],
    }, {
        'id': 'old', 'label': 'Old box', 'command': 'bin/provision', 'setup': 'apply',
    }]},
}


def lab(station):
    """The plugin in the workstation's own plugin folder, and that folder."""
    folder = station.berth_dir / 'plugins/lab'
    (folder / 'bin').mkdir(parents=True)
    (folder / 'package.json').write_text(json.dumps(MANIFEST))
    program = folder / 'bin/provision'
    program.write_text(PROGRAM)
    program.chmod(program.stat().st_mode | stat.S_IXUSR)
    return folder


def requests(folder):
    path = folder / 'requests.jsonl'
    return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


@needs_e2e
class Provisioners(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.station = Workstation('provisioners', ssh_key=False, trust=False)
        cls.folder = lab(cls.station)

    def berth(self, *args, **kwargs):
        return self.station.berth(*args, **kwargs)

    def test_a_provisioner_is_listed_with_why_it_cannot_be_used(self):
        listed = self.berth('provisioner').stdout
        self.assertIn('lab/box  Lab box  · not usable: lab/box needs place', listed)
        self.assertIn('lab/old  Old box  · not usable: lab/old speaks version 1', listed)
        page = self.berth('provisioner', 'lab/old').stdout
        self.assertIn('unusable', page)

    def test_its_settings_page_and_actions_from_the_command_line(self):
        self.berth('provisioner', 'lab/box', 'run', 'settings', 'place=attic', '--yes')
        page = self.berth('provisioner', 'lab/box').stdout
        for line in ('State     ready · The lab at attic answers', 'Benches   3', 'Settings  place=attic',
                     '  attic            box-1  running · berth has no record of it', '    look'):
            self.assertIn(line, page)
        # For this once, another place.
        self.assertIn('The lab at cellar answers', self.berth('provisioner', 'lab/box', 'place=cellar').stdout)
        looked = self.berth('provisioner', 'lab/box', 'run', 'look').stdout
        self.assertEqual(looked.splitlines(), ['Looked around attic', '  Dust  little'])
        swept = self.berth('provisioner', 'lab/box', 'run', 'sweep', 'shelf=bottom', '--yes')
        self.assertIn('sweeping bottom with \n', swept.stdout)
        self.assertIn('✓ Swept', swept.stdout)
        refused = self.berth('provisioner', 'lab/box', 'run', 'sweep', 'shelf=middle', '--yes', check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn('shelf=top or bottom only', refused.stderr)
        self.assertEqual(requests(self.folder)[-1]['verb'], 'act')

    def test_through_berth_serve_an_action_that_changes_something_is_a_job(self):
        self.berth('provisioner', 'lab/box', 'run', 'settings', 'place=attic', '--yes')
        serve = Serve(self.station, web=False)
        try:
            page = serve.call('GET', '/v1/provisioners/lab/box')
            self.assertEqual((page['state'], page['message']), ('ready', 'The lab at attic answers'))
            self.assertEqual(page['values'], {'shelf': ['top', 'bottom']})
            actions = serve.call('GET', '/v1/provisioners/lab/box/actions')
            self.assertEqual([action['name'] for action in actions], ['look', 'sweep', 'settings'])
            looked = serve.call('POST', '/v1/provisioners/lab/box/actions/look')
            self.assertEqual(looked['result']['message'], 'Looked around attic')
            acted = serve.call('POST', '/v1/provisioners/lab/box/actions/sweep',
                               {'arguments': {'code': 's3cret-code'}, 'token': 'sweep-1'}, expect=202)
            job = acted['job']
            self.assertEqual((job['subject'], job['action']), ('provisioner:lab/box', 'sweep'))
            portal = serve.portal()
            texts, data, _ = portal.stream('jobs.follow', {'id': job['id']},
                                           until=lambda texts, data: any('exited' in t for t in texts if t))
            output = data.decode(errors='replace')
            self.assertIn('sweeping top with [secret]', output)
            self.assertNotIn('s3cret-code', output)
            # The same token again is the same job, and nothing runs again.
            before = len(requests(self.folder))
            again = serve.call('POST', '/v1/provisioners/lab/box/actions/sweep',
                               {'arguments': {'code': 's3cret-code'}, 'token': 'sweep-1'}, expect=202)
            self.assertEqual(again['job']['id'], job['id'])
            self.assertEqual(len(requests(self.folder)), before)
            # And from a client of the server.
            client = Workstation('provisioners-client', ssh_key=False, trust=False)
            listed = client.berth('--server', serve.url, '--token', serve.token, 'provisioner').stdout
            self.assertIn('lab/box  Lab box', listed)
            portal.close()
        finally:
            serve.stop()


@needs_e2e
@unittest.skipUnless(os.environ.get('BERTH_E2E_DOCKER_HOST'), 'no Docker engine on the test network')
class DockerEngine(unittest.TestCase):
    """The Docker plugin's actions that only read, against the engine of the test network."""

    def test_the_engine_answers_and_lists_its_runtimes(self):
        station = Workstation('provisioners-docker', ssh_key=False, trust=False)
        endpoint = os.environ['BERTH_E2E_DOCKER_HOST']
        station.berth('provisioner', 'docker/container', 'run', 'settings', f'host={endpoint}', '--yes')
        page = station.berth('provisioner', 'docker/container').stdout
        self.assertIn(f'answers at {endpoint}', page)
        self.assertIn('Runtimes', page)
        volumes = station.berth('provisioner', 'docker/container', 'run', 'volumes').stdout
        self.assertIn('berth made at', volumes)


if __name__ == '__main__':
    unittest.main()
