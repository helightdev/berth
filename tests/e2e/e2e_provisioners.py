"""Provisioners as subjects of their own, with the real berth: a made-up
provisioner of a plugin in the workstation's own plugin folder, its page, its
settings and its actions, and a deployment it makes as a resource, from the
command line and through berth serve, with the endpoints it reports and the
commands it runs. It needs no test host, and runs on `dock` beside the
modules there.

With BERTH_E2E_DOCKER_HOST set to a Docker engine the runner may use, such as
tcp://docker:2375, the Docker plugin's own actions are asked of it too, a
small Compose deployment is made, changed and removed there, and an instance
with a published port is reached through its endpoint and runs a command.
Without one those cases are skipped."""
import json
import os
import socket
import stat
import struct
import threading
import unittest

from harness import Serve, WebSocket, Workstation, needs_e2e

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
if verb == 'status' and request.get('id'):
    print(json.dumps({'state': 'running'}))
elif verb == 'status':
    print(json.dumps({'message': f'The lab at {place} answers',
                      'details': [{'label': 'Benches', 'value': '3'}],
                      'values': {'shelf': ['top', 'bottom']}}))
elif verb == 'list':
    print(json.dumps({'resources': [{'id': 'box-1', 'kind': 'machine', 'name': 'attic', 'state': 'running'}]}))
elif verb in ('create', 'change') and request.get('kind') == 'deployment':
    # The secret reaches the program, which says it, so berth masks it.
    print(f'deploying {request["name"]} with {request.get("environment", {}).get("TOKEN")}', file=sys.stderr)
    with open(f'spec.{request["name"]}', 'w') as spec:
        spec.write(request['spec'])
    answer = {'id': f'project-{request["name"]}', 'state': 'running'}
    # A spec that names a port of this machine is reached there.
    for line in request['spec'].splitlines():
        if line.startswith('listen: '):
            answer['endpoints'] = [{'name': 'bench', 'address': '127.0.0.1', 'port': int(line[8:]),
                                    'hint': 'echo', 'local': True}]
    print(json.dumps(answer))
elif verb == 'exec':
    print(f'ran {" ".join(request["command"])} reading {request.get("stdin", "nothing")!r}', file=sys.stderr)
    print(json.dumps({'exit_code': 4}))
elif verb == 'destroy':
    print(json.dumps({'state': 'gone'}))
elif verb == 'logs':
    print(f'{request["id"]} started, tail {request["tail"]}')
elif verb == 'act':
    action = request['action']
    if request.get('id'):
        print(json.dumps({'message': f'{request["kind"]} {request["id"]} has 1 service',
                          'details': [{'label': 'web', 'value': 'running'}]}))
    elif action['name'] == 'look':
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
        'kinds': [{'id': 'machine', 'setup': 'apply'},
                  {'id': 'deployment', 'label': 'Bench setup', 'verbs': ['change', 'logs', 'exec'],
                   'spec': {'format': 'text', 'label': 'Setup'},
                   'actions': [{'name': 'services', 'label': 'Services', 'changes': False}]}],
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
    """The plugin in the workstation's own plugin folder, registered as `box`
    and `old`, and that folder."""
    folder = station.berth_dir / 'plugins/lab'
    (folder / 'bin').mkdir(parents=True)
    (folder / 'package.json').write_text(json.dumps(MANIFEST))
    program = folder / 'bin/provision'
    program.write_text(PROGRAM)
    program.chmod(program.stat().st_mode | stat.S_IXUSR)
    # berth runs each type under a name of its own.
    station.berth('provisioner', 'add', 'box', 'lab/box')
    station.berth('provisioner', 'add', 'old', 'lab/old')
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
        listed = {found['id']: found for found in json.loads(self.berth('provisioner', 'list', '-o', 'json').stdout)}
        self.assertEqual(listed['box']['type'], 'lab/box')
        self.assertEqual(listed['box']['problem'], 'box needs place: berth provisioner set box place=VALUE')
        self.assertEqual(listed['old']['type'], 'lab/old')
        self.assertTrue(listed['old']['problem'].startswith('lab/old speaks version 1'), listed['old']['problem'])
        self.assertRegex(self.berth('provisioner', 'list').stdout, r'(?m)^box\s+lab/box\s+no: box needs place: .*\s+Lab box$')
        types = self.berth('provisioner', 'types').stdout
        self.assertRegex(types, r'(?m)^lab/box\s+Lab box\s+machine, deployment$')
        page = self.berth('provisioner', 'show', 'old').stdout
        self.assertIn('unusable', page)

    def test_its_settings_page_and_actions_from_the_command_line(self):
        self.berth('provisioner', 'run', 'box', 'settings', 'place=attic', '--yes')
        page = self.berth('provisioner', 'show', 'box').stdout
        for line in ('State     ready · The lab at attic answers', 'Benches   3', 'Settings  place=attic',
                     'Machines berth made with it', '  attic            box-1  running · berth has no record of it',
                     '    look'):
            self.assertIn(line, page)
        # For this once, another place.
        self.assertIn('The lab at cellar answers', self.berth('provisioner', 'show', 'box', 'place=cellar').stdout)
        looked = self.berth('provisioner', 'run', 'box', 'look').stdout
        self.assertEqual(looked.splitlines(), ['Looked around attic', '  Dust  little'])
        swept = self.berth('provisioner', 'run', 'box', 'sweep', 'shelf=bottom', '--yes')
        self.assertIn('sweeping bottom with \n', swept.stdout)
        self.assertIn('✓ Swept', swept.stdout)
        refused = self.berth('provisioner', 'run', 'box', 'sweep', 'shelf=middle', '--yes', check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn('shelf=top or bottom only', refused.stderr)
        self.assertEqual(requests(self.folder)[-1]['verb'], 'act')

    def test_through_berth_serve_an_action_that_changes_something_is_a_job(self):
        self.berth('provisioner', 'run', 'box', 'settings', 'place=attic', '--yes')
        serve = Serve(self.station)
        try:
            page = serve.call('GET', '/v1/provisioners/box')
            self.assertEqual((page['state'], page['message']), ('ready', 'The lab at attic answers'))
            self.assertEqual(page['values'], {'shelf': ['top', 'bottom']})
            actions = serve.call('GET', '/v1/provisioners/box/actions')
            self.assertEqual([action['name'] for action in actions],
                             ['new-deployment', 'settings', 'look', 'sweep', 'adopt', 'remove'])
            looked = serve.call('POST', '/v1/provisioners/box/actions/look')
            self.assertEqual(looked['result']['message'], 'Looked around attic')
            acted = serve.call('POST', '/v1/provisioners/box/actions/sweep',
                               {'arguments': {'code': 's3cret-code'}, 'token': 'sweep-1'}, expect=202)
            job = acted['job']
            self.assertEqual((job['subject'], job['action']), ('provisioner:box', 'sweep'))
            portal = serve.portal()
            texts, data, _ = portal.stream('jobs.follow', {'id': job['id']},
                                           until=lambda texts, data: any('exited' in t for t in texts if t))
            output = data.decode(errors='replace')
            self.assertIn('sweeping top with [secret]', output)
            self.assertNotIn('s3cret-code', output)
            # The same token again is the same job, and nothing runs again.
            before = len(requests(self.folder))
            again = serve.call('POST', '/v1/provisioners/box/actions/sweep',
                               {'arguments': {'code': 's3cret-code'}, 'token': 'sweep-1'}, expect=202)
            self.assertEqual(again['job']['id'], job['id'])
            self.assertEqual(len(requests(self.folder)), before)
            # And from a client of the server.
            client = Workstation('provisioners-client', ssh_key=False, trust=False)
            listed = client.berth('--server', serve.url, '--token-stdin', 'provisioner', 'list', input=serve.token).stdout
            self.assertRegex(listed, r'(?m)^box\s+lab/box\s')
            portal.close()
        finally:
            serve.stop()


@needs_e2e
class Deployments(unittest.TestCase):
    """A deployment of the lab provisioner, as a resource of its own."""

    @classmethod
    def setUpClass(cls):
        cls.station = Workstation('deployments', ssh_key=False, trust=False)
        cls.folder = lab(cls.station)
        cls.station.berth('provisioner', 'run', 'box', 'settings', 'place=attic', '--yes')
        cls.station.berth('secret', 'set', 'bench_token', input='b3nch-t0ken-made-up\n')

    def berth(self, *args, **kwargs):
        return self.station.berth(*args, **kwargs)

    def test_1_a_deployment_is_made_from_its_spec_with_a_secret_by_name(self):
        made = self.berth('resource', 'create', 'bench', '--provisioner', 'box', '--spec', '-', '--secret', 'bench_token:TOKEN', input='benches: 3\n')
        output = made.stdout + made.stderr
        self.assertIn('deploying bench with [secret]', output)
        self.assertNotIn('b3nch-t0ken', output)
        self.assertEqual((self.folder / 'spec.bench').read_text(), 'benches: 3\n')
        # The secret reaches the program in the request on its input, and the spec is kept without it.
        create = next(asked for asked in requests(self.folder) if asked['verb'] == 'create')
        self.assertEqual(list(create['environment']), ['TOKEN'])
        self.assertNotIn('b3nch-t0ken', create['spec'] + json.dumps(create['arguments']))
        listed = self.berth('resource', 'list').stdout
        self.assertRegex(listed, r'(?m)^r-\S+\s+deployment\s+bench\s+box\s+running\s*$')
        page = self.berth('provisioner', 'show', 'box').stdout
        self.assertIn('Bench setup berth made with it', page)
        self.assertRegex(page, r'  bench\s+project-bench  r-')

    def test_2_its_page_actions_log_and_change(self):
        shown = self.berth('resource', 'show', 'bench').stdout
        for line in ('State      running', 'Made as    project-bench', 'Secrets    bench_token as TOKEN',
                     '  services', '  change', '  remove'):
            self.assertIn(line, shown)
        said = self.berth('resource', 'run', 'bench', 'services').stdout
        self.assertEqual(said.splitlines(), ['deployment project-bench has 1 service', '  web  running'])
        log = self.berth('resource', 'logs', 'bench', '--tail', '5').stdout
        self.assertIn('project-bench started, tail 5', log)
        (self.station.root / 'bench.txt').write_text('benches: 4\n')
        self.berth('resource', 'create', 'bench', '--provisioner', 'box', '--spec', str(self.station.root / 'bench.txt'))
        self.assertEqual((self.folder / 'spec.bench').read_text(), 'benches: 4\n')
        self.assertEqual(requests(self.folder)[-1]['verb'], 'change')

    def test_3_through_berth_serve_with_a_token_asked_twice(self):
        serve = Serve(self.station)
        try:
            body = {'provisioner': 'box', 'kind': 'deployment', 'name': 'yard', 'spec': 'yard: 1\n',
                    'secrets': [{'name': 'bench_token', 'env': 'TOKEN'}], 'token': 'yard-1'}
            created = serve.call('POST', '/v1/resources', body, expect=202)
            again = serve.call('POST', '/v1/resources', body, expect=202)
            self.assertEqual((again['resource']['id'], again['job']['id']),
                             (created['resource']['id'], created['job']['id']))
            portal = serve.portal()
            texts, data, _ = portal.stream('jobs.follow', {'id': created['job']['id']},
                                           until=lambda texts, data: any('exited' in t for t in texts if t))
            self.assertNotIn('b3nch-t0ken', data.decode(errors='replace'))
            rid = created['resource']['id']
            self.assertEqual(serve.call('GET', f'/v1/resources/{rid}')['state'], 'running')
            listed = serve.call('GET', '/v1/resources?kind=deployment')
            self.assertEqual(sorted(found['name'] for found in listed), ['bench', 'yard'])
            removed = serve.call('DELETE', f'/v1/resources/{rid}', expect=202)
            portal.stream('jobs.follow', {'id': removed['job']['id']},
                          until=lambda texts, data: any('exited' in t for t in texts if t))
            serve.call('GET', f'/v1/resources/{rid}', expect=404)
            portal.close()
        finally:
            serve.stop()

    def test_4_removing_it_forgets_it(self):
        removed = self.berth('resource', 'remove', 'bench', '-y')
        self.assertIn('Removed the deployment bench', removed.stdout)
        self.assertNotIn('bench', self.berth('resource', 'list').stdout)

    def test_5_the_provisioners_own_action_makes_one_and_it_is_kept_while_it_holds_one(self):
        self.berth('provisioner', 'run', 'box', 'new-deployment', 'name=shed', 'spec=shed: 1\nbenches: 2\n', 'secrets=bench_token:TOKEN', '--yes')
        self.assertEqual((self.folder / 'spec.shed').read_text(), 'shed: 1\nbenches: 2\n')
        self.assertIn('shed', self.berth('resource', 'list').stdout)
        refused = self.berth('provisioner', 'remove', 'box', '-y', check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn('box still holds', refused.stderr)
        self.berth('resource', 'remove', 'shed', '-y')
        self.assertIn('Removed the provisioner box', self.berth('provisioner', 'remove', 'box', '-y').stdout)
        self.assertNotIn('Lab box', self.berth('provisioner', 'list').stdout)


def echo():
    """A listener on the loopback that sends back what it gets and ends its side after the
    caller has ended theirs: its port."""
    listener = socket.create_server(('127.0.0.1', 0))

    def serve():
        while True:
            connection, _ = listener.accept()
            with connection:
                while data := connection.recv(65536):
                    connection.sendall(data)
                connection.shutdown(socket.SHUT_WR)

    threading.Thread(target=serve, daemon=True).start()
    return listener.getsockname()[1]


def through_endpoint(serve, rid, endpoint, data):
    """Send `data` to an endpoint of a resource on its own stream, end, and read what comes back
    until the other side has ended too."""
    ws = WebSocket(serve.port, f'/v1/resources/{rid}/endpoints/{endpoint}', protocols=[serve.token])
    try:
        ws.send(data)
        ws.send({'eof': True})
        back = b''
        while True:
            frame = ws.recv()
            if frame is None or frame == {'eof': True}:
                return back
            back += frame
    finally:
        ws.close()


def through_portal(portal, rid, endpoint, data):
    """The same over the portal, as a stream of its own number."""
    opened = portal.result('resources.connect', {'id': rid, 'endpoint': endpoint})
    assert opened.get('open'), opened
    number = portal.next - 1
    tag = struct.pack('!I', number)
    portal.send(tag + data)
    portal.send({'sub': number, 'data': {'eof': True}})
    back = b''
    while True:
        frame = portal._take(lambda got: (got[:4] == tag) if isinstance(got, bytes) else got.get('sub') == number,
                             60)
        if isinstance(frame, bytes):
            back += frame[4:]
        elif frame.get('data') == {'eof': True} or frame.get('end'):
            portal.send({'sub': number, 'end': True})
            return back


def job_output(portal, job):
    """A job's output and the exit code it ended with."""
    texts, data, _ = portal.stream('jobs.follow', {'id': job['id']},
                                   until=lambda texts, data: any('exited' in t for t in texts if t))
    exited = next(t['exited'] for t in texts if t and 'exited' in t)
    return data.decode(errors='replace'), exited['exit_code']


@needs_e2e
class Endpoints(unittest.TestCase):
    """A deployment of the lab provisioner that says where it listens, reached through berth
    serve, and a command run in it."""

    @classmethod
    def setUpClass(cls):
        cls.station = Workstation('endpoints', ssh_key=False, trust=False)
        cls.folder = lab(cls.station)
        cls.station.berth('provisioner', 'set', 'box', 'place=attic')
        cls.serve = Serve(cls.station)
        cls.portal = cls.serve.portal()
        cls.port = echo()
        body = {'provisioner': 'box', 'kind': 'deployment', 'name': 'db', 'spec': f'listen: {cls.port}\n'}
        created = cls.serve.call('POST', '/v1/resources', body, expect=202)
        cls.rid = created['resource']['id']
        _, code = job_output(cls.portal, created['job'])
        assert code == 0, code

    @classmethod
    def tearDownClass(cls):
        cls.portal.close()
        cls.serve.stop()

    def test_its_endpoints_are_kept_with_it(self):
        seen = self.serve.call('GET', f'/v1/resources/{self.rid}')
        self.assertEqual(seen['endpoints'], [{'name': 'bench', 'address': '127.0.0.1', 'port': self.port,
                                              'protocol': 'tcp', 'hint': 'echo', 'local': True}])

    def test_an_endpoint_is_reached_on_its_own_stream_and_over_the_portal(self):
        data = os.urandom(1 << 20)
        self.assertEqual(through_endpoint(self.serve, self.rid, 'bench', b'hello'), b'hello')
        self.assertEqual(through_endpoint(self.serve, self.rid, str(self.port), data), data)
        self.assertEqual(through_portal(self.portal, self.rid, 'bench', data), data)
        # Only what the provisioner reported is opened.
        with self.assertRaisesRegex(PermissionError, '^404 '):
            WebSocket(self.serve.port, f'/v1/resources/{self.rid}/endpoints/22', protocols=[self.serve.token])
        refused = self.portal.call('resources.connect', {'id': self.rid, 'endpoint': '22'})
        self.assertEqual(refused['error']['code'], 'not_found')

    def test_a_command_runs_in_it_as_a_job_that_ends_as_the_command_did(self):
        acted = self.serve.call('POST', f'/v1/resources/{self.rid}/actions/exec',
                                {'arguments': {'command': 'count --all', 'stdin': 'benches'}}, expect=202)
        output, code = job_output(self.portal, acted['job'])
        self.assertEqual(code, 4, output)
        self.assertIn("ran count --all reading 'benches'", output)
        # The output is the command's alone.
        self.assertNotIn('exited with', output)

    def test_the_command_line_shows_its_endpoints(self):
        page = self.station.berth('resource', 'show', 'db').stdout
        self.assertIn(f'Endpoints (berth port forward {self.rid} ENDPOINT)', page)
        self.assertRegex(page, rf'(?m)^  bench  127\.0\.0\.1:{self.port}/tcp  echo · on the server\'s machine$')
        shown = json.loads(self.station.berth('resource', 'show', 'db', '-o', 'json').stdout)
        self.assertEqual([endpoint['name'] for endpoint in shown['endpoints']], ['bench'])

    def test_the_command_line_runs_a_command_in_it_with_its_exit_status(self):
        ran = self.station.berth('resource', 'exec', 'db', '--stdin', '--', 'count', '--all',
                                 input='benches', check=False)
        self.assertEqual(ran.returncode, 4, ran.output)
        self.assertIn("ran count --all reading 'benches'", ran.stdout)
        self.assertNotIn('exited with', ran.stdout)

    def test_port_forward_reaches_an_endpoint_by_its_name(self):
        free = socket.create_server(('127.0.0.1', 0))
        local = free.getsockname()[1]
        free.close()
        forward = self.station.spawn('port', 'forward', 'db', f'{local}:bench')
        try:
            said = forward.stdout.readline()
            self.assertIn(f'127.0.0.1:{local} → db:bench', said)
            with socket.create_connection(('127.0.0.1', local), timeout=10) as connection:
                connection.sendall(b'hello')
                connection.shutdown(socket.SHUT_WR)
                got = b''
                while data := connection.recv(65536):
                    got += data
            self.assertEqual(got, b'hello')
        finally:
            forward.terminate()
            forward.wait(timeout=10)
        # A name that is no host and no resource says so.
        refused = self.station.berth('port', 'forward', 'nowhere', '80', check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn('There is no host or resource nowhere', refused.stderr)


@needs_e2e
@unittest.skipUnless(os.environ.get('BERTH_E2E_DOCKER_HOST'), 'no Docker engine on the test network')
class DockerEngine(unittest.TestCase):
    """The Docker plugin against the engine of the test network."""

    def test_the_engine_answers_and_lists_its_runtimes(self):
        station = Workstation('provisioners-docker', ssh_key=False, trust=False)
        endpoint = os.environ['BERTH_E2E_DOCKER_HOST']
        station.berth('provisioner', 'add', 'docker', 'docker/container', f'host={endpoint}')
        page = station.berth('provisioner', 'show', 'docker').stdout
        self.assertIn(f'answers at {endpoint}', page)
        self.assertIn('Runtimes', page)
        volumes = station.berth('provisioner', 'run', 'docker', 'volumes').stdout
        self.assertIn(f'volumes at {endpoint}', volumes)

    def test_a_compose_deployment_is_made_changed_and_removed(self):
        station = Workstation('provisioners-compose', ssh_key=False, trust=False)
        endpoint = os.environ['BERTH_E2E_DOCKER_HOST']
        station.berth('provisioner', 'add', 'docker', 'docker/container', f'host={endpoint}')
        station.berth('secret', 'set', 'compose_token', input='c0mpose-t0ken-made-up\n')
        spec = ('services:\n  idle:\n    image: busybox:1.37\n'
                '    command: ["sh", "-c", "echo token-is-$$TOKEN; sleep 600"]\n'
                '    environment:\n      TOKEN: ${TOKEN}\n')
        name = 'berth-test-e2e'
        made = station.berth('resource', 'create', name, '--provisioner', 'docker', '--spec', '-', '--secret', 'compose_token:TOKEN', input=spec, timeout=300)
        self.assertNotIn('c0mpose-t0ken', made.stdout + made.stderr)
        try:
            self.assertIn('running', station.berth('resource', 'show', name).stdout)
            log = station.berth('resource', 'logs', name, '--tail', '20').stdout
            self.assertIn('token-is-[secret]', log)
            station.berth('resource', 'set', name, '--spec', '-', input=spec.replace('600', '601'), timeout=300)
            self.assertIn('running', station.berth('resource', 'show', name).stdout)
        finally:
            removed = station.berth('resource', 'remove', name, '-y', timeout=300)
        self.assertIn(f'Removed the deployment {name}', removed.stdout)

    def test_an_instance_is_reached_at_its_published_port_and_runs_a_command(self):
        station = Workstation('provisioners-instance', ssh_key=False, trust=False)
        endpoint = os.environ['BERTH_E2E_DOCKER_HOST']
        station.berth('provisioner', 'add', 'docker', 'docker/container', f'host={endpoint}')
        serve = Serve(station)
        portal = serve.portal()
        command = "sh -c 'mkdir -p /www && echo hello-from-box > /www/index.html && httpd -f -p 8080 -h /www'"
        body = {'provisioner': 'docker', 'kind': 'instance', 'name': 'berth-test-web',
                'arguments': {'image': 'busybox:1.37', 'command': command, 'ports': '8080'}}
        created = serve.call('POST', '/v1/resources', body, expect=202)
        rid = created['resource']['id']
        try:
            output, code = job_output(portal, created['job'])
            self.assertEqual(code, 0, output)
            # The engine is elsewhere, so the port Docker chose is at the engine's own name.
            [published] = serve.call('GET', f'/v1/resources/{rid}')['endpoints']
            host = endpoint.split('://', 1)[1].rsplit(':', 1)[0]
            self.assertEqual((published['address'], published['protocol']), (host, 'tcp'))
            self.assertNotIn('local', published)
            answer = through_endpoint(serve, rid, published['name'], b'GET / HTTP/1.0\r\n\r\n')
            self.assertIn(b'200 OK', answer)
            self.assertIn(b'hello-from-box', answer)
            answer = through_portal(portal, rid, str(published['port']), b'GET / HTTP/1.0\r\n\r\n')
            self.assertIn(b'hello-from-box', answer)
            acted = serve.call('POST', f'/v1/resources/{rid}/actions/exec',
                               {'arguments': {'command': '["sh", "-c", "cat /www/index.html; exit 5"]'}},
                               expect=202)
            output, code = job_output(portal, acted['job'])
            self.assertEqual(code, 5, output)
            self.assertIn('hello-from-box', output)
            # The same from the command line: its output and its exit status are the command's.
            ran = station.berth('resource', 'exec', rid, '--', 'sh', '-c', 'cat /www/index.html; exit 5',
                                check=False)
            self.assertEqual((ran.returncode, ran.stdout.strip()), (5, 'hello-from-box'), ran.output)
        finally:
            removed = serve.call('DELETE', f'/v1/resources/{rid}', expect=202)
            job_output(portal, removed['job'])
            portal.close()
            serve.stop()


if __name__ == '__main__':
    unittest.main()
