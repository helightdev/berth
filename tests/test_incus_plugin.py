"""The Incus plugin's provisioners against a small Incus look-alike served over HTTPS."""
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import shutil
import ssl
import subprocess
import tempfile
import threading
import unittest
import urllib.parse
import uuid

ROOT = Path(__file__).resolve().parents[1]
PLUGIN = ROOT / 'plugins/incus'


def certificate(directory, name, extensions):
    subprocess.run(['openssl', 'req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1',
                    '-nodes', '-days', '1', '-subj', f'/CN={name}', '-keyout', str(directory / f'{name}.key'),
                    '-out', str(directory / f'{name}.crt')] + [x for e in extensions for x in ('-addext', e)],
                   check=True, capture_output=True)
    return (directory / f'{name}.crt').read_text(), (directory / f'{name}.key').read_text()


class FakeIncus(ThreadingHTTPServer):
    """Enough of the Incus API for the plugin: instances, their state, files and operations."""
    daemon_threads = True

    def __init__(self, context):
        super().__init__(('127.0.0.1', 0), Handler)
        self.socket = context.wrap_socket(self.socket, server_side=True)
        self.trusted = True
        self.instances = {}
        self.pools = {'default': 'zfs', 'fast': 'dir'}
        self.volumes = {}
        self.operations = {}
        self.events = []
        self.lock = threading.Lock()

    def operation(self, error=''):
        key = str(uuid.uuid4())
        self.operations[key] = {'id': key, 'status': 'Failure' if error else 'Success', 'err': error, 'metadata': {}}
        return key

    def handle_error(self, request, client_address):
        # Clients that reject the certificate hang up mid-handshake; that is expected here.
        pass


def public(instance):
    return {k: v for k, v in instance.items() if k not in ('files', 'request', 'console')}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def answer(self, status, body=None, kind='sync'):
        if kind == 'error':
            body = {'type': 'error', 'error': body, 'error_code': status}
        elif kind == 'async':
            body = {'type': 'async', 'status': 'Operation created', 'status_code': 100,
                    'operation': f'/1.0/operations/{body}', 'metadata': {'id': body}}
        else:
            body = {'type': 'sync', 'status': 'Success', 'status_code': 200, 'metadata': body}
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(raw)))
        self.end_headers()
        if self.command != 'HEAD':
            self.wfile.write(raw)

    def raw(self, data):
        self.send_response(200)
        self.send_header('Content-Type', 'application/octet-stream')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def route(self):
        server = self.server
        url = urllib.parse.urlsplit(self.path)
        query = dict(urllib.parse.parse_qsl(url.query))
        length = int(self.headers.get('Content-Length') or 0)
        body = self.rfile.read(length) if length else b''
        parts = url.path.strip('/').split('/')
        server.events.append(('client-cert', bool(self.connection.getpeercert())))
        if url.path == '/1.0':
            return self.answer(200, {'auth': 'trusted' if server.trusted else 'untrusted', 'api_version': '1.0',
                                     'environment': {'server_version': '6.18', 'server_name': 'dock',
                                                     'kernel': 'Linux', 'kernel_version': '6.17.0',
                                                     'storage_supported_drivers': [{'name': 'zfs'}, {'name': 'dir'}]}})
        if not server.trusted:
            return self.answer(403, 'not authorized', 'error')
        if parts == ['1.0', 'storage-pools'] and self.command == 'GET':
            return self.answer(200, [{'name': name, 'driver': driver} for name, driver in server.pools.items()])
        if parts == ['1.0', 'storage-pools'] and self.command == 'POST':
            data = json.loads(body)
            server.events.append(('pool', data))
            server.pools[data['name']] = data['driver']
            return self.answer(200, {})
        if parts[:2] == ['1.0', 'storage-pools'] and len(parts) == 3 and self.command == 'DELETE':
            if any(pool == parts[2] for pool, _ in server.volumes):
                return self.answer(400, 'The storage pool is currently in use', 'error')
            server.pools.pop(parts[2], None)
            return self.answer(200, {})
        if parts[:2] == ['1.0', 'storage-pools'] and parts[3:5] == ['volumes', 'custom']:
            pool = parts[2]
            if len(parts) == 5 and self.command == 'GET':
                return self.answer(200, [v for (p, _), v in server.volumes.items() if p == pool])
            if len(parts) == 5 and self.command == 'POST':
                data = json.loads(body)
                server.volumes[(pool, data['name'])] = {'name': data['name'], 'config': data.get('config') or {},
                                                        'content_type': 'filesystem', 'used_by': []}
                return self.answer(200, {})
            if len(parts) == 6 and self.command == 'DELETE':
                if server.volumes.pop((pool, parts[5]), None) is None:
                    return self.answer(404, 'Storage volume not found', 'error')
                return self.answer(200, {})
        if parts[:3] == ['1.0', 'images', 'aliases']:
            return self.answer(404, 'not found', 'error')
        if parts[:2] == ['1.0', 'images'] and len(parts) == 3 and self.command == 'DELETE':
            server.events.append(('image-delete', parts[2]))
            return self.answer(202, server.operation(), 'async')
        if parts[:2] == ['1.0', 'storage-pools'] and parts[3:] == ['resources']:
            if parts[2] == 'fast':
                return self.answer(500, 'no resources for dir', 'error')
            return self.answer(200, {'space': {'used': 10 * 1024 ** 3, 'total': 100 * 1024 ** 3}})
        if parts == ['1.0', 'networks']:
            return self.answer(200, [{'name': 'incusbr0', 'type': 'bridge', 'managed': True},
                                     {'name': 'eth0', 'type': 'physical', 'managed': False}])
        if parts == ['1.0', 'images']:
            return self.answer(200, [{'fingerprint': 'abcdef1234567890', 'type': 'container', 'size': 2 * 1024 ** 3,
                                      'cached': True, 'aliases': [], 'properties': {'description': 'Ubuntu 26.04'}}])
        if parts[:2] == ['1.0', 'operations'] and parts[3:] == ['wait']:
            return self.answer(200, server.operations[parts[2]])
        if parts == ['1.0', 'instances'] and self.command == 'GET':
            return self.answer(200, [public(i) for i in server.instances.values()])
        if parts == ['1.0', 'instances'] and self.command == 'POST':
            data = json.loads(body)
            server.events.append(('create', data['name'], data['type']))
            if data['source']['alias'] == 'ubuntu/missing/cloud':
                return self.answer(202, server.operation(
                    "Failed getting remote image info: The requested image couldn't be found"), 'async')
            server.instances[data['name']] = {'name': data['name'], 'type': data['type'],
                                              'status': 'Running' if data.get('start') else 'Stopped',
                                              'config': data['config'], 'devices': data['devices'],
                                              'request': data, 'files': {}}
            return self.answer(202, server.operation(), 'async')
        instance = server.instances.get(parts[2]) if len(parts) > 2 else None
        if instance is None:
            return self.answer(404, 'Instance not found', 'error')
        rest = parts[3:]
        if rest == [] and self.command == 'GET':
            return self.answer(200, public(instance))
        if rest == [] and self.command == 'PATCH':
            data = json.loads(body)
            server.events.append(('patch', instance['name'], data))
            instance['config'].update(data.get('config') or {})
            instance['devices'] = data.get('devices') or instance.get('devices')
            return self.answer(200, {})
        if rest == [] and self.command == 'DELETE':
            server.events.append(('delete', instance['name'], instance['status']))
            del server.instances[instance['name']]
            return self.answer(202, server.operation(), 'async')
        if rest == ['state'] and self.command == 'GET':
            running = instance['status'] == 'Running'
            network = {'lo': {'addresses': [{'family': 'inet', 'address': '127.0.0.1', 'scope': 'local'}]},
                       # A guest that runs Docker has its bridge too, which sorts first.
                       'docker0': {'addresses': [{'family': 'inet', 'address': '172.17.0.1', 'scope': 'global'}]},
                       'eth0': {'addresses': [{'family': 'inet6', 'address': 'fe80::1', 'scope': 'link'},
                                              {'family': 'inet', 'address': '192.0.2.50', 'scope': 'global'}]}}
            return self.answer(200, {'status': instance['status'], 'network': network if running else None})
        if rest == ['state'] and self.command == 'PUT':
            action = json.loads(body)
            server.events.append((action['action'], instance['name'], action.get('force', False)))
            instance['status'] = {'start': 'Running', 'stop': 'Stopped'}[action['action']]
            return self.answer(202, server.operation(), 'async')
        if rest == ['console'] and self.command == 'GET':
            server.events.append(('console', query.get('type')))
            return self.raw(instance.get('console', b''))
        if rest == ['files']:
            if instance['type'] == 'virtual-machine' and instance['status'] != 'Running':
                return self.answer(500, "VM agent isn't currently running", 'error')
            path = query['path']
            if self.command == 'GET':
                known = instance['files'].get(path)
                return self.raw(known) if isinstance(known, bytes) else self.answer(404, 'not found', 'error')
            if self.command == 'HEAD':
                known = path == '/' or path in instance['files']
                return self.answer(200 if known else 404, {} if known else 'not found', 'sync' if known else 'error')
            kind = self.headers.get('X-Incus-type', 'file')
            server.events.append(('push', path, kind, instance['status'], self.headers.get('X-Incus-mode')))
            instance['files'][path] = kind if kind == 'directory' else body
            return self.answer(200, {})
        return self.answer(400, f'not in the fake: {self.command} {url.path}', 'error')

    def do_GET(self):
        with self.server.lock:
            self.route()

    do_POST = do_PUT = do_DELETE = do_HEAD = do_PATCH = do_GET


@unittest.skipUnless(shutil.which('openssl'), 'openssl makes the certificates for the fake server')
class IncusPluginTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory()
        directory = Path(cls.temp.name)
        cls.server_cert, _ = certificate(directory, 'server', ['subjectAltName=IP:127.0.0.1'])
        cls.client_cert, cls.client_key = certificate(
            directory, 'client', ['basicConstraints=critical,CA:TRUE', 'keyUsage=digitalSignature,keyCertSign'])
        cls.other_cert, _ = certificate(directory, 'other', [])
        cls.archive = directory / 'devbox-release.tar.gz'
        cls.archive.write_bytes(b'release archive ' * 1000)
        cls.sha = hashlib.sha256(cls.archive.read_bytes()).hexdigest()
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.minimum_version = ssl.TLSVersion.TLSv1_2
        context.load_cert_chain(directory / 'server.crt', directory / 'server.key')
        # Incus asks every client for a certificate and decides trust afterwards.
        context.verify_mode = ssl.CERT_OPTIONAL
        context.load_verify_locations(directory / 'client.crt')
        cls.server = FakeIncus(context)
        threading.Thread(target=cls.server.serve_forever, daemon=True).start()
        cls.address = f'https://127.0.0.1:{cls.server.server_address[1]}'

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.temp.cleanup()

    def setUp(self):
        self.server.trusted = True
        self.server.instances.clear()
        self.server.events.clear()

    def run_verb(self, verb, provisioner='container', secrets=None, **fields):
        request = {'version': 2, 'verb': verb, 'provisioner': provisioner,
                   'arguments': {'server': self.address, 'cpus': '4', 'memory': '8GiB', 'disk': '60GiB',
                                 'image': 'ubuntu/26.04/cloud', 'pool': 'default', 'network': '',
                                 'project': 'default', **fields.pop('arguments', {})},
                   'secrets': secrets or {'incus_client_cert': self.client_cert, 'incus_client_key': self.client_key,
                                          'incus_server_cert': self.server_cert}, **fields}
        result = subprocess.run([str(PLUGIN / 'bin/provision'), verb], cwd=PLUGIN, input=json.dumps(request),
                                capture_output=True, text=True, timeout=60)
        return result.returncode, json.loads(result.stdout) if result.returncode == 0 else None, result.stderr

    def create(self, provisioner='container', host='studio-2', token='0b6d', **fields):
        return self.run_verb('create', provisioner, host=host, token=token,
                             labels={'berth-host': host, 'berth-instance': token},
                             seed={'user_data': '#cloud-config\n{}\n', 'meta_data': f'instance-id: {token}\n',
                                   'archive': {'path': str(self.archive), 'sha256': self.sha}}, **fields)

    def actions(self):
        return [e for e in self.server.events if e[0] in ('create', 'push', 'start', 'stop', 'delete')]

    def test_container_takes_the_release_before_its_first_start(self):
        code, answer, err = self.create()
        self.assertEqual(code, 0, err)
        self.assertEqual(answer, {'id': 'berth-studio-2', 'address': '192.0.2.50'})
        self.assertEqual(self.actions(), [
            ('create', 'berth-studio-2', 'container'),
            ('push', '/opt/devbox-bootstrap', 'directory', 'Stopped', '0700'),
            ('push', '/opt/devbox-bootstrap/release.tar.gz', 'file', 'Stopped', '0600'),
            ('start', 'berth-studio-2', False)])
        instance = self.server.instances['berth-studio-2']
        self.assertEqual(instance['files']['/opt/devbox-bootstrap/release.tar.gz'], self.archive.read_bytes())
        self.assertEqual(instance['config'], {
            'limits.cpu': '4', 'limits.memory': '8GiB', 'cloud-init.user-data': '#cloud-config\n{}\n',
            'user.berth-host': 'studio-2', 'user.berth-instance': '0b6d', 'security.nesting': 'true'})
        self.assertEqual(instance['devices'], {'root': {'type': 'disk', 'path': '/', 'pool': 'default', 'size': '60GiB'}})
        source = instance['request']['source']
        self.assertEqual((source['server'], source['protocol'], source['alias']),
                         ('https://images.linuxcontainers.org', 'simplestreams', 'ubuntu/26.04/cloud'))
        self.assertFalse(instance['request']['start'])
        self.assertIn(('client-cert', True), self.server.events)
        self.assertNotIn(('client-cert', False), self.server.events)
        for line in ['Creating berth-studio-2…', 'Copying the release…', 'Starting…']:
            self.assertIn(line, err)

    def test_vm_starts_before_the_release_is_copied(self):
        code, answer, err = self.create('vm', arguments={'network': 'lan', 'cpus': '8'})
        self.assertEqual(code, 0, err)
        self.assertEqual(answer['id'], 'berth-studio-2')
        self.assertEqual(self.actions(), [
            ('create', 'berth-studio-2', 'virtual-machine'),
            ('start', 'berth-studio-2', False),
            ('push', '/opt/devbox-bootstrap', 'directory', 'Running', '0700'),
            ('push', '/opt/devbox-bootstrap/release.tar.gz', 'file', 'Running', '0600')])
        instance = self.server.instances['berth-studio-2']
        self.assertNotIn('security.nesting', instance['config'])
        self.assertEqual(instance['config']['limits.cpu'], '8')
        self.assertEqual(instance['devices']['eth0'], {'type': 'nic', 'name': 'eth0', 'network': 'lan'})

    def test_create_again_with_the_same_token_returns_the_instance(self):
        self.assertEqual(self.create()[0], 0)
        self.server.events.clear()
        code, answer, err = self.create(host='studio-2')
        self.assertEqual(code, 0, err)
        self.assertEqual(answer['id'], 'berth-studio-2')
        self.assertEqual(self.actions(), [])
        self.assertEqual(len(self.server.instances), 1)

    def test_a_name_taken_by_someone_else_is_refused(self):
        self.server.instances['berth-studio-2'] = {'name': 'berth-studio-2', 'type': 'container',
                                                   'status': 'Running', 'config': {}, 'files': {}}
        code, _, err = self.create()
        self.assertEqual(code, 1)
        self.assertIn('already has an instance named berth-studio-2', err)
        self.assertIn('other than berth', err)

    def test_a_missing_image_is_named(self):
        code, _, err = self.create(arguments={'image': 'ubuntu/missing/cloud'})
        self.assertEqual(code, 1)
        self.assertIn('There is no image ubuntu/missing/cloud', err)

    def test_status_follows_the_instance(self):
        self.create()
        self.assertEqual(self.run_verb('status', id='berth-studio-2')[1], {'state': 'running', 'address': '192.0.2.50'})
        self.assertEqual(self.run_verb('stop', id='berth-studio-2')[:2], (0, {}))
        self.assertIn(('stop', 'berth-studio-2', True), self.server.events)
        self.assertEqual(self.run_verb('status', id='berth-studio-2')[1], {'state': 'stopped'})
        self.assertEqual(self.run_verb('start', id='berth-studio-2')[:2], (0, {}))
        self.assertEqual(self.run_verb('status', id='berth-studio-2')[1]['state'], 'running')
        self.assertEqual(self.run_verb('status', id='berth-gone')[1], {'state': 'gone'})

    def logs(self, **fields):
        """What logs writes, which is not JSON."""
        request = {'version': 2, 'verb': 'logs', 'provisioner': 'container',
                   'arguments': {'server': self.address, 'project': 'default'},
                   'secrets': {'incus_client_cert': self.client_cert, 'incus_client_key': self.client_key,
                               'incus_server_cert': self.server_cert}, **fields}
        result = subprocess.run([str(PLUGIN / 'bin/provision'), 'logs'], cwd=PLUGIN, capture_output=True, text=True,
                                timeout=60, input=json.dumps(request))
        return result.returncode, result.stdout, result.stderr

    def test_logs_are_the_latest_console_lines(self):
        self.create()
        console = ''.join(f'[  OK  ] Started unit-{n}.service.\r\n' for n in range(150))
        self.server.instances['berth-studio-2']['console'] = console.encode()
        code, out, err = self.logs(id='berth-studio-2')
        self.assertEqual(code, 0, err)
        self.assertEqual(len(out.splitlines()), 100)
        self.assertEqual(out.splitlines()[-1], '[  OK  ] Started unit-149.service.')
        self.assertIn(('console', 'log'), self.server.events)

    def test_logs_read_cloud_init_output_when_the_console_is_empty(self):
        self.create()
        self.server.instances['berth-studio-2']['files']['/var/log/cloud-init-output.log'] = \
            b'Cloud-init v. 25.1 running modules:config\nCloud-init v. 25.1 finished\n'
        code, out, err = self.logs(id='berth-studio-2')
        self.assertEqual(code, 0, err)
        self.assertEqual(out, 'Cloud-init v. 25.1 running modules:config\nCloud-init v. 25.1 finished\n')

    def test_logs_of_a_vm_without_its_agent_are_empty_and_of_one_gone_fail(self):
        self.create('vm')
        self.server.instances['berth-studio-2']['status'] = 'Stopped'
        self.assertEqual(self.logs(id='berth-studio-2')[:2], (0, ''))
        code, _, err = self.logs(id='berth-gone')
        self.assertEqual(code, 1)
        self.assertIn('no longer', err)

    def test_destroy_stops_and_deletes_and_is_fine_when_gone(self):
        self.create()
        self.server.events.clear()
        self.assertEqual(self.run_verb('destroy', id='berth-studio-2')[:2], (0, {}))
        self.assertEqual(self.actions(), [('stop', 'berth-studio-2', True), ('delete', 'berth-studio-2', 'Stopped')])
        self.assertEqual(self.run_verb('destroy', id='berth-studio-2')[:2], (0, {}))

    def test_destroy_leaves_an_instance_berth_did_not_make(self):
        self.server.instances['web'] = {'name': 'web', 'type': 'container', 'status': 'Running',
                                        'config': {'user.owner': 'someone'}, 'files': {}}
        code, _, err = self.run_verb('destroy', id='web')
        self.assertEqual(code, 1)
        self.assertIn('not made by berth', err)
        self.assertIn('web', self.server.instances)
        self.assertEqual(self.actions(), [])

    def test_list_shows_only_berths_instances(self):
        self.create()
        self.server.instances['web'] = {'name': 'web', 'type': 'container', 'status': 'Stopped',
                                        'config': {}, 'files': {}}
        code, answer, err = self.run_verb('list')
        self.assertEqual(code, 0, err)
        self.assertEqual(answer, {'resources': [{'id': 'berth-studio-2', 'kind': 'machine', 'name': 'studio-2',
                                                 'host': 'studio-2', 'state': 'running'}]})
        # A provisioner lists the instances of its own type.
        self.assertEqual(self.run_verb('list', 'vm')[1], {'resources': []})

    def test_an_instance_is_any_image_with_its_own_user_data_and_no_host(self):
        def make(name='web_1', token='7f3a', **arguments):
            return self.run_verb('create', 'container', kind='instance', name=name, token=token,
                                 labels={'berth-resource': 'r1', 'berth-token': token},
                                 arguments={'image': 'debian/12', 'user_data': '#cloud-config\npackages: [nginx]\n',
                                            **arguments})
        code, answer, err = make()
        self.assertEqual(code, 0, err)
        self.assertEqual(answer, {'id': 'web-1', 'state': 'running', 'address': '192.0.2.50'})
        made = self.server.instances['web-1']['request']
        self.assertTrue(made['start'])
        self.assertEqual(made['source']['alias'], 'debian/12')
        self.assertEqual(made['config']['cloud-init.user-data'], '#cloud-config\npackages: [nginx]\n')
        self.assertEqual(made['config']['user.berth-resource'], 'r1')
        self.assertNotIn('user.berth-host', made['config'])
        # Nothing of berth is copied into it.
        self.assertEqual([e for e in self.server.events if e[0] == 'push'], [])
        # Asked again with its token, it is the same one.
        self.assertEqual(make()[1]['id'], 'web-1')
        self.assertEqual(len([e for e in self.server.events if e[0] == 'create']), 1)
        # A name that is taken is refused.
        code, _, err = make(token='other')
        self.assertEqual(code, 1)
        self.assertIn('already has an instance named web-1', err)
        # It is listed as an instance, beside the machines.
        self.create()
        listed = self.run_verb('list')[1]['resources']
        self.assertEqual(sorted((r['id'], r['kind'], r['host']) for r in listed),
                         [('berth-studio-2', 'machine', 'studio-2'), ('web-1', 'instance', '')])
        self.assertEqual([r['id'] for r in self.run_verb('list', kind='instance')[1]['resources']], ['web-1'])
        # berth changes and removes it as one of its own.
        self.assertEqual(self.run_verb('change', id='web-1', arguments={'cpus': '2', 'memory': '1GiB'})[0], 0)
        self.assertEqual(self.server.instances['web-1']['config']['limits.memory'], '1GiB')
        # What is fixed when it is made is refused before anything is changed.
        code, _, err = self.run_verb('change', kind='instance', id='web-1', changed=['memory', 'image', 'pool'],
                                     arguments={'memory': '4GiB', 'image': 'debian/13', 'pool': 'fast'})
        self.assertEqual(code, 1)
        self.assertIn('cannot change its image, pool in place: make a new instance instead', err)
        self.assertEqual(self.server.instances['web-1']['config']['limits.memory'], '1GiB')
        self.assertEqual(self.run_verb('destroy', id='web-1')[:2], (0, {}))
        self.assertNotIn('web-1', self.server.instances)

    def test_status_without_an_id_is_the_server(self):
        code, answer, err = self.run_verb('status')
        self.assertEqual(code, 0, err)
        self.assertEqual(answer['state'], 'ready')
        self.assertEqual(answer['message'], f'Incus 6.18 answers at {self.address} and trusts berth')
        self.assertEqual(answer['values'], {'pool': ['default', 'fast'], 'network': ['eth0', 'incusbr0']})
        rows = {row['label']: row['value'] for row in answer['details']}
        self.assertEqual((rows['Name'], rows['Storage'], rows['Kernel']), ('dock', 'zfs, dir', 'Linux 6.17.0'))

    def test_volumes_and_pools_are_created_listed_and_removed(self):
        def act(action, **arguments):
            return self.run_verb('act', action={'name': action, 'arguments': arguments})
        code, answer, err = act('create-pool', name='bulk', driver='zfs', size='50GiB')
        self.assertEqual(code, 0, err)
        self.assertIn(('pool', {'name': 'bulk', 'driver': 'zfs', 'config': {'size': '50GiB'}}), self.server.events)
        code, answer, err = act('create-volume', pool='bulk', name='wiki-data', size='10GiB')
        self.assertEqual(code, 0, err)
        code, answer, err = act('volumes')
        self.assertEqual(code, 0, err)
        self.assertEqual(answer['details'], [{'label': 'bulk/wiki-data', 'value': 'filesystem · 10GiB · unused'}])
        # A pool that holds a volume is refused, and Incus says why.
        code, _, err = act('remove-pool', name='bulk')
        self.assertEqual(code, 1)
        self.assertIn('in use', err)
        self.assertEqual(act('remove-volume', pool='bulk', name='wiki-data')[0], 0)
        self.assertEqual(act('remove-pool', name='bulk')[0], 0)
        self.assertNotIn('bulk', self.server.pools)
        self.assertEqual(act('create-volume', name='bad name!')[0], 1)
        code, answer, err = act('remove-image', image='abcdef')
        self.assertEqual(code, 0, err)
        self.assertIn(('image-delete', 'abcdef1234567890'), self.server.events)

    def test_actions_answer_with_rows(self):
        code, answer, err = self.run_verb('act', action={'name': 'pools', 'arguments': {}})
        self.assertEqual(code, 0, err)
        self.assertEqual(answer['details'], [{'label': 'default', 'value': 'zfs · 10.0 GiB of 100.0 GiB used'},
                                             {'label': 'fast', 'value': 'dir · its use is not known'}])
        code, answer, err = self.run_verb('act', action={'name': 'images'})
        self.assertEqual(code, 0, err)
        self.assertEqual(answer['details'], [{'label': 'Ubuntu 26.04', 'value': 'container · 2.0 GiB · cached'}])
        code, answer, err = self.run_verb('act', action={'name': 'networks'})
        self.assertEqual(answer['details'][0], {'label': 'incusbr0', 'value': 'bridge · managed by Incus'})
        code, answer, err = self.run_verb('act', action={'name': 'server'})
        self.assertIn('trusts berth', answer['message'])
        code, _, err = self.run_verb('act', action={'name': 'wipe'})
        self.assertEqual(code, 1)
        self.assertIn("no action 'wipe'", err)

    def test_change_sets_new_limits_and_grows_the_disk(self):
        self.create()
        code, answer, err = self.run_verb('change', id='berth-studio-2',
                                          arguments={'cpus': '2', 'memory': '4GiB', 'disk': '80GiB'})
        self.assertEqual(code, 0, err)
        self.assertEqual(answer, {'state': 'running'})
        [patch] = [e for e in self.server.events if e[0] == 'patch']
        self.assertEqual(patch[2]['config'], {'limits.cpu': '2', 'limits.memory': '4GiB'})
        self.assertEqual(patch[2]['devices']['root']['size'], '80GiB')
        self.server.instances['web'] = {'name': 'web', 'type': 'container', 'status': 'Running',
                                        'config': {}, 'files': {}}
        code, _, err = self.run_verb('change', id='web', arguments={'cpus': '1'})
        self.assertEqual(code, 1)
        self.assertIn('not made by berth', err)

    def test_logs_are_the_last_lines_of_the_console(self):
        self.create()
        self.server.instances['berth-studio-2']['console'] = ''.join(f'line {n}\n' for n in range(1, 6)).encode()
        code, out, err = self.logs(id='berth-studio-2', tail=2)
        self.assertEqual(code, 0, err)
        self.assertEqual(out, 'line 4\nline 5\n')

    def test_another_version_of_the_protocol_is_refused(self):
        result = subprocess.run([str(PLUGIN / 'bin/provision'), 'status'], cwd=PLUGIN, capture_output=True,
                                text=True, timeout=60, input=json.dumps({'version': 1, 'provisioner': 'vm'}))
        self.assertEqual(result.returncode, 1)
        self.assertIn('version 2', result.stderr)

    def test_an_untrusted_client_is_told_what_to_do(self):
        self.server.trusted = False
        code, answer, err = self.run_verb('list')
        self.assertEqual((code, answer), (1, None))
        self.assertIn('does not trust this client certificate', err)
        self.assertIn('incus config trust add-certificate', err)

    def test_the_server_certificate_is_pinned_or_checked(self):
        secrets = {'incus_client_cert': self.client_cert, 'incus_client_key': self.client_key}
        code, _, err = self.run_verb('list', secrets={**secrets, 'incus_server_cert': self.other_cert})
        self.assertEqual(code, 1)
        self.assertIn('certificate other than the one in incus_server_cert', err)
        code, _, err = self.run_verb('list', secrets=secrets)
        self.assertEqual(code, 1)
        self.assertIn('is not trusted here', err)

    def test_an_unreachable_server_is_said_plainly(self):
        code, _, err = self.run_verb('status', id='berth-studio-2', arguments={'server': 'https://127.0.0.1:1'})
        self.assertEqual(code, 1)
        self.assertIn('Cannot reach the Incus server at https://127.0.0.1:1', err)

    def test_names_are_what_incus_accepts(self):
        code, answer, err = self.create(host='lab_box' + 'x' * 70, token='t2')
        self.assertEqual(code, 0, err)
        self.assertRegex(answer['id'], r'^berth-lab-boxx+$')
        self.assertEqual(len(answer['id']), 63)


if __name__ == '__main__':
    unittest.main()
