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
        self.pools = {}
        self.volumes = {}
        self.extensions = ['storage_pool_loop_resize']
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
    return {k: v for k, v in instance.items() if k not in ('files', 'request', 'console', 'logs')}


def pools():
    """The pools of a server that is set up: a loop-backed zfs pool, a dir pool and an lvm pool on a disk."""
    gib = 1024 ** 3
    return {
        'default': {'name': 'default', 'driver': 'zfs', 'description': '', 'used': 10 * gib,
                    'config': {'source': '/var/lib/incus/disks/default.img', 'size': '100GiB',
                               'zfs.pool_name': 'default'}},
        'fast': {'name': 'fast', 'driver': 'dir', 'description': '', 'used': 5 * gib, 'total': 200 * gib,
                 'config': {'source': '/var/lib/incus/storage-pools/fast'}},
        'disk': {'name': 'disk', 'driver': 'lvm', 'description': '', 'used': 0, 'total': 500 * gib,
                 'config': {'source': '/dev/sdb', 'lvm.vg_name': 'disk'}},
    }


SIZES = {'GiB': 1024 ** 3, 'GB': 1000 ** 3}


def size_in_bytes(text):
    for unit, factor in SIZES.items():
        if text.endswith(unit):
            return int(text[:-len(unit)]) * factor
    return int(text)


def public_pool(pool):
    return {k: v for k, v in pool.items() if k not in ('used', 'total')}


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
                                                     'storage_supported_drivers': [{'name': 'zfs'}, {'name': 'dir'}]},
                                     'api_extensions': server.extensions})
        if not server.trusted:
            return self.answer(403, 'not authorized', 'error')
        if parts == ['1.0', 'storage-pools'] and self.command == 'GET':
            return self.answer(200, [public_pool(pool) for pool in server.pools.values()])
        if parts == ['1.0', 'storage-pools'] and self.command == 'POST':
            data = json.loads(body)
            server.events.append(('pool', data))
            server.pools[data['name']] = {'name': data['name'], 'driver': data['driver'], 'description': '',
                                          'config': data.get('config') or {}, 'used': 0}
            return self.answer(200, {})
        if parts[:2] == ['1.0', 'storage-pools'] and len(parts) == 3 and self.command in ('GET', 'PATCH', 'PUT'):
            pool = server.pools.get(parts[2])
            if pool is None:
                return self.answer(404, 'Storage pool not found', 'error')
            if self.command == 'GET':
                return self.answer(200, public_pool(pool))
            data = json.loads(body)
            server.events.append(('pool-' + self.command.lower(), parts[2], data))
            if self.command == 'PUT':
                pool['config'] = data.get('config') or {}
            else:
                pool['config'].update(data.get('config') or {})
            if 'description' in data:
                pool['description'] = data['description']
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
            volume = server.volumes.get((pool, parts[5])) if len(parts) > 5 else None
            if len(parts) >= 6 and volume is None:
                return self.answer(404, 'Storage volume not found', 'error')
            if len(parts) == 6 and self.command == 'GET':
                return self.answer(200, volume)
            if len(parts) == 6 and self.command == 'PATCH':
                data = json.loads(body)
                server.events.append(('volume-patch', pool, parts[5], data))
                volume['config'].update(data.get('config') or {})
                return self.answer(200, {})
            if parts[6:] == ['state']:
                return self.answer(200, {'usage': {'used': volume.get('used', 0)}})
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
            pool = server.pools[parts[2]]
            if pool.get('unknown'):
                return self.answer(500, 'Failed getting the pool resources', 'error')
            total = pool.get('total') or size_in_bytes(pool['config'].get('size') or '0')
            return self.answer(200, {'space': {'used': pool['used'], 'total': total}})
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
                       # So does the role's dummy interface, once the role has run.
                       'devbox0': {'addresses': [{'family': 'inet', 'address': '192.0.2.2', 'scope': 'global'}]},
                       # One the guest made itself has no name on the server's side.
                       'dummy0': {'host_name': '',
                                  'addresses': [{'family': 'inet', 'address': '192.0.2.3', 'scope': 'global'}]},
                       'eth0': {'host_name': 'veth1a2b3c',
                                'addresses': [{'family': 'inet6', 'address': 'fe80::1', 'scope': 'link'},
                                              {'family': 'inet', 'address': '192.0.2.50', 'scope': 'global'}]}}
            return self.answer(200, {'status': instance['status'], 'network': network if running else None})
        if rest == ['state'] and self.command == 'PUT':
            action = json.loads(body)
            server.events.append((action['action'], instance['name'], action.get('force', False)))
            instance['status'] = {'start': 'Running', 'stop': 'Stopped', 'restart': 'Running'}[action['action']]
            return self.answer(202, server.operation(), 'async')
        if rest == ['exec'] and self.command == 'POST':
            server.events.append(('exec', instance['name'], json.loads(body)))
            base = f'/1.0/instances/{instance["name"]}/logs'
            instance['logs'] = {'exec_1.stdout': b'hi from web\n', 'exec_1.stderr': b'one warning\n'}
            key = server.operation()
            server.operations[key]['metadata'] = {'return': 2, 'output': {'1': f'{base}/exec_1.stdout',
                                                                         '2': f'{base}/exec_1.stderr'}}
            return self.answer(202, key, 'async')
        if rest[:1] == ['logs'] and len(rest) == 2:
            if self.command == 'DELETE':
                server.events.append(('log-delete', rest[1]))
                instance.get('logs', {}).pop(rest[1], None)
                return self.answer(200, {})
            return self.raw(instance.get('logs', {}).get(rest[1], b''))
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
        self.server.pools = pools()
        self.server.volumes.clear()
        self.server.extensions = ['storage_pool_loop_resize']

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
        self.assertEqual(answer, {'id': 'berth-studio-2', 'endpoints': [], 'address': '192.0.2.50'})
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
        self.assertEqual(self.run_verb('status', id='berth-studio-2')[1], {'state': 'running', 'endpoints': [], 'address': '192.0.2.50'})
        self.assertEqual(self.run_verb('stop', id='berth-studio-2')[:2], (0, {}))
        self.assertIn(('stop', 'berth-studio-2', True), self.server.events)
        self.assertEqual(self.run_verb('status', id='berth-studio-2')[1], {'state': 'stopped', 'endpoints': []})
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
        self.assertEqual(answer, {'id': 'web-1', 'state': 'running', 'endpoints': [], 'address': '192.0.2.50'})
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
        self.assertEqual(answer['values'], {'pool': ['default', 'disk', 'fast'], 'network': ['eth0', 'incusbr0'],
                                            'instance': []})
        rows = {row['label']: row['value'] for row in answer['details']}
        self.assertEqual((rows['Name'], rows['Storage'], rows['Kernel']), ('dock', 'zfs, dir', 'Linux 6.17.0'))

    def test_status_gives_the_items_of_each_collection_as_rows(self):
        gib = 1024 ** 3
        self.server.pools['fast']['used_by'] = ['/1.0/profiles/default', '/1.0/instances/web?project=default']
        self.server.volumes[('default', 'data')] = {'name': 'data', 'config': {'size': '10GiB'}, 'used': 2 * gib,
                                                    'content_type': 'filesystem',
                                                    'used_by': ['/1.0/instances/berth-studio?project=default']}
        self.server.volumes[('fast', 'disk')] = {'name': 'disk', 'config': {}, 'content_type': 'block',
                                                 'used_by': []}
        code, answer, err = self.run_verb('status')
        self.assertEqual(code, 0, err)
        collections = answer['collections']
        self.assertEqual(sorted(collections), ['images', 'pools', 'volumes'])
        pools = {row['name']: row for row in collections['pools']}
        self.assertEqual(pools['default']['columns'],
                         {'driver': 'zfs', 'source': '/var/lib/incus/disks/default.img', 'size': '100.0 GiB',
                          'used': '10.0 GiB', 'free': '90.0 GiB'})
        # Each row says which of its actions cannot run on it, and why.
        self.assertEqual(pools['default']['unavailable'], {})
        self.assertEqual(pools['fast']['unavailable']['resize-pool'],
                         'A dir pool has no size of its own, only the space of its file system')
        self.assertEqual(pools['fast']['unavailable']['remove-pool'],
                         'Incus removes only an empty pool, and profile default, instance web use it')
        self.assertIn('/dev/sdb', pools['disk']['unavailable']['resize-pool'])
        volumes = {(row['key']['pool'], row['key']['volume']): row for row in collections['volumes']}
        self.assertEqual(volumes[('default', 'data')]['columns'],
                         {'pool': 'default', 'type': 'filesystem', 'size': '10GiB', 'used': '2.0 GiB',
                          'used_by': 'berth-studio'})
        self.assertEqual(volumes[('default', 'data')]['unavailable'],
                         {'remove-volume': 'Incus refuses it while berth-studio uses it'})
        self.assertEqual(volumes[('fast', 'disk')]['columns']['size'], 'no set size')
        self.assertEqual(volumes[('fast', 'disk')]['unavailable'], {})
        self.assertEqual(collections['images'], [{
            'name': 'Ubuntu 26.04', 'key': {'image': 'abcdef123456'},
            'columns': {'fingerprint': 'abcdef123456', 'type': 'container', 'size': '2.0 GiB', 'cached': 'yes',
                        'uploaded': ''}}])
        # The key of an image removes it.
        code, answer, err = self.act('remove-image', image='abcdef123456')
        self.assertEqual(code, 0, err)
        self.assertIn(('image-delete', 'abcdef1234567890'), self.server.events)

    def test_the_manifest_declares_the_collections_status_gives(self):
        manifest = json.loads((PLUGIN / 'package.json').read_text())
        for provisioner in manifest['berth']['provisioners']:
            actions = {action['name']: action for action in provisioner['actions']}
            declared = {collection['id']: collection for collection in provisioner['collections']}
            self.assertEqual(sorted(declared), ['images', 'instances', 'pools', 'volumes'])
            for collection in declared.values():
                for sort in ('new', 'item', 'list'):
                    for used in collection.get(sort, []):
                        self.assertIn(used['action'], actions)
            # Grow root disk is on the machines, not loose on the page.
            self.assertEqual(declared['instances']['kinds'], ['machine', 'instance'])
            self.assertEqual(declared['volumes']['within'], 'pools')

    def test_volumes_and_pools_are_created_listed_and_removed(self):
        act = self.act
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

    def act(self, action, **arguments):
        return self.run_verb('act', action={'name': action, 'arguments': arguments})

    def test_pools_are_a_table_that_says_which_can_grow(self):
        self.server.pools['bare'] = {'name': 'bare', 'driver': 'btrfs', 'description': '', 'used': 0,
                                     'unknown': True, 'config': {'source': '/var/lib/incus/disks/bare.img',
                                                                 'size': '20GiB'}}
        code, answer, err = self.act('pools')
        self.assertEqual(code, 0, err)
        self.assertEqual(answer['message'], f'4 storage pools on {self.address}')
        rows = {row['label']: row['value'].split() for row in answer['details']}
        self.assertEqual(answer['details'][0]['label'], 'Pool')
        self.assertEqual(rows['Pool'], ['Driver', 'Source', 'Size', 'Used', 'Free', 'Can', 'grow'])
        self.assertEqual(rows['default'], ['zfs', '/var/lib/incus/disks/default.img', '100.0', 'GiB',
                                           '10.0', 'GiB', '90.0', 'GiB', 'yes'])
        self.assertEqual(rows['fast'][:3], ['dir', '/var/lib/incus/storage-pools/fast', '200.0'])
        fast = answer['details'][[r['label'] for r in answer['details']].index('fast')]['value']
        self.assertIn('no: a dir pool has no size of its own', fast)
        disk = answer['details'][[r['label'] for r in answer['details']].index('disk')]['value']
        self.assertIn('no: it is on /dev/sdb, given when it was made; grow that instead', disk)
        self.assertEqual(rows['bare'][:5], ['btrfs', '/var/lib/incus/disks/bare.img', '20GiB', '?', '?'])
        # The columns line up: each starts at the same place in every row.
        values = [row['value'] for row in answer['details']]
        at = values[0].index('Source')
        self.assertTrue(all(value[at - 2:at] == '  ' and value[at] != ' ' for value in values), values)

    def test_a_loop_backed_pool_grows_and_says_its_space_before_and_after(self):
        code, answer, err = self.act('resize-pool', pool='default', size='150GiB')
        self.assertEqual(code, 0, err)
        self.assertIn(('pool-patch', 'default', {'config': {'size': '150GiB'}}), self.server.events)
        self.assertEqual(answer['message'], f'The pool default on {self.address} is 150GiB now')
        self.assertEqual(answer['details'], [
            {'label': 'Before', 'value': '100.0 GiB, 10.0 GiB used, 90.0 GiB free'},
            {'label': 'After', 'value': '150.0 GiB, 10.0 GiB used, 140.0 GiB free'}])
        self.assertIn('Before: 100.0 GiB, 10.0 GiB used, 90.0 GiB free', err)
        self.assertIn('After: 150.0 GiB, 10.0 GiB used, 140.0 GiB free', err)
        # The same size again changes nothing.
        self.server.events.clear()
        code, answer, err = self.act('resize-pool', pool='default', size='150GiB')
        self.assertEqual(code, 0, err)
        self.assertIn('already', answer['message'])
        self.assertFalse([e for e in self.server.events if e[0] == 'pool-patch'])

    def test_a_pool_that_cannot_grow_is_refused_with_the_reason(self):
        cases = [
            ({'pool': 'default', 'size': '50GiB'}, 'Incus only grows a pool'),
            ({'pool': 'fast', 'size': '500GiB'}, 'a dir pool has no size of its own'),
            ({'pool': 'disk', 'size': '1000GiB'}, 'it is on /dev/sdb, given when it was made'),
            ({'pool': 'gone', 'size': '50GiB'}, 'no storage pool gone'),
        ]
        for arguments, said in cases:
            code, _, err = self.act('resize-pool', **arguments)
            self.assertEqual(code, 1, arguments)
            self.assertIn(said, err)
        self.server.extensions = []
        code, _, err = self.act('resize-pool', pool='default', size='200GiB')
        self.assertEqual(code, 1)
        self.assertIn('storage_pool_loop_resize', err)
        self.assertFalse([e for e in self.server.events if e[0] == 'pool-patch'])

    def test_sizes_are_checked_before_anything_is_sent(self):
        self.server.instances['web'] = {'name': 'web', 'type': 'container', 'status': 'Running', 'config': {},
                                        'devices': {'root': {'type': 'disk', 'path': '/', 'pool': 'default',
                                                             'size': '10GiB'}}, 'files': {}}
        for size in ('50 GiB', '50gb', '1.5GiB', '-5GiB', 'big', '0GiB', '50G'):
            for action, arguments in (('resize-pool', {'pool': 'default'}),
                                      ('resize-volume', {'pool': 'default', 'volume': 'data'}),
                                      ('resize-disk', {'instance': 'web'}),
                                      ('create-volume', {'pool': 'default', 'name': 'data'}),
                                      ('create-pool', {'name': 'new', 'driver': 'zfs'})):
                code, _, err = self.act(action, size=size, **arguments)
                self.assertEqual(code, 1, (action, size))
                self.assertIn(f'{size!r} is not a size Incus takes', err)
                self.assertIn('such as 50GiB or 200GB', err)
        code, _, err = self.act('resize-pool', pool='default', size='')
        self.assertEqual(code, 1)
        self.assertIn('Give the size', err)
        self.assertFalse([e for e in self.server.events if e[0] not in ('client-cert',)])
        # Bytes alone and decimal units are sizes too.
        self.assertEqual(self.act('resize-pool', pool='default', size='200GB')[0], 0)
        self.assertEqual(self.act('resize-pool', pool='default', size='300000000000')[0], 0)

    def test_set_pool_changes_the_settings_incus_allows_and_no_others(self):
        code, answer, err = self.act('set-pool', pool='default', key='volume.size', value='20GiB')
        self.assertEqual(code, 0, err)
        self.assertEqual(self.server.pools['default']['config']['volume.size'], '20GiB')
        self.assertEqual(answer['details'], [{'label': 'Before', 'value': '(unset)'},
                                             {'label': 'After', 'value': '20GiB'}])
        code, answer, err = self.act('set-pool', pool='default', key='description', value='The fast disks')
        self.assertEqual(code, 0, err)
        self.assertEqual(self.server.pools['default']['description'], 'The fast disks')
        self.assertEqual(self.act('set-pool', pool='default', key='zfs.clone_copy', value='rebase')[0], 0)
        self.assertEqual(self.act('set-pool', pool='fast', key='rsync.bwlimit', value='50MB')[0], 0)
        # An empty value unsets it, and the rest of the pool stays.
        code, answer, err = self.act('set-pool', pool='default', key='volume.size', value='')
        self.assertEqual(code, 0, err)
        self.assertNotIn('volume.size', self.server.pools['default']['config'])
        self.assertEqual(self.server.pools['default']['config']['zfs.clone_copy'], 'rebase')
        self.assertEqual(self.server.pools['default']['description'], 'The fast disks')
        self.server.events.clear()
        for arguments, said in (
                ({'pool': 'default', 'key': 'source', 'value': '/dev/sdc'},
                 'does not change source of a zfs pool. It changes description, volume.size'),
                ({'pool': 'default', 'key': 'zfs.pool_name', 'value': 'other'}, 'zfs.clone_copy, zfs.export'),
                ({'pool': 'fast', 'key': 'zfs.export', 'value': 'false'}, 'rsync.bwlimit, rsync.compression'),
                ({'pool': 'default', 'key': 'size', 'value': '200GiB'}, 'changed with resize-pool'),
                ({'pool': 'default', 'key': 'volume.size', 'value': '20 gigs'}, 'not a size Incus takes'),
                ({'pool': 'gone', 'key': 'description', 'value': 'x'}, 'no storage pool gone')):
            code, _, err = self.act('set-pool', **arguments)
            self.assertEqual(code, 1, arguments)
            self.assertIn(said, err)
        self.assertFalse([e for e in self.server.events if e[0].startswith('pool-')])

    def test_a_volume_is_resized_and_a_block_volume_does_not_shrink(self):
        self.server.volumes[('default', 'data')] = {'name': 'data', 'config': {'size': '10GiB'},
                                                    'content_type': 'filesystem', 'used_by': [], 'used': 2 * 1024 ** 3}
        self.server.volumes[('default', 'disk')] = {'name': 'disk', 'config': {'size': '10GiB'},
                                                    'content_type': 'block', 'used_by': []}
        code, answer, err = self.act('resize-volume', pool='default', volume='data', size='20GiB')
        self.assertEqual(code, 0, err)
        self.assertEqual(self.server.volumes[('default', 'data')]['config']['size'], '20GiB')
        self.assertEqual(answer['details'], [{'label': 'Before', 'value': '10GiB, 2.0 GiB used'},
                                             {'label': 'After', 'value': '20GiB'}])
        # A filesystem volume may shrink to what it holds; Incus says when it cannot.
        code, answer, err = self.act('resize-volume', pool='default', volume='data', size='5GiB')
        self.assertEqual(code, 0, err)
        self.assertIn('Incus refuses it if the volume holds more than 5GiB', err)
        self.assertEqual(self.act('resize-volume', pool='default', volume='disk', size='20GiB')[0], 0)
        code, _, err = self.act('resize-volume', pool='default', volume='disk', size='5GiB')
        self.assertEqual(code, 1)
        self.assertIn('Incus cannot shrink a block volume', err)
        self.assertEqual(self.server.volumes[('default', 'disk')]['config']['size'], '20GiB')
        code, _, err = self.act('resize-volume', pool='default', volume='gone', size='5GiB')
        self.assertEqual(code, 1)
        self.assertIn('no custom volume gone', err)

    def test_resize_disk_grows_the_root_disk_and_restarts_a_vm_only_when_asked(self):
        self.assertEqual(self.create('vm')[0], 0)
        self.server.events.clear()
        code, answer, err = self.act('resize-disk', instance='berth-studio-2', size='80GiB')
        self.assertEqual(code, 0, err)
        self.assertEqual(self.server.instances['berth-studio-2']['devices']['root']['size'], '80GiB')
        self.assertIn('sees the new size after a restart: run this again with restart=yes', answer['message'])
        self.assertEqual(answer['details'], [{'label': 'Before', 'value': '60GiB'}, {'label': 'After', 'value': '80GiB'}])
        self.assertFalse([e for e in self.server.events if e[0] in ('restart', 'stop', 'start')])
        code, answer, err = self.act('resize-disk', instance='berth-studio-2', size='100GiB', restart='yes')
        self.assertEqual(code, 0, err)
        self.assertIn(('restart', 'berth-studio-2', False), self.server.events)
        self.assertIn('was restarted', answer['message'])
        # It only grows.
        code, _, err = self.act('resize-disk', instance='berth-studio-2', size='50GiB')
        self.assertEqual(code, 1)
        self.assertIn('is 100GiB, and berth only grows it', err)
        code, _, err = self.act('resize-disk', instance='berth-studio-2', size='120GiB', restart='maybe')
        self.assertEqual(code, 1)
        self.assertIn("restart is yes or no, not 'maybe'", err)
        code, _, err = self.act('resize-disk', instance='nowhere', size='120GiB')
        self.assertEqual(code, 1)
        self.assertIn('no instance nowhere', err)

    def test_resize_disk_overrides_a_root_disk_from_the_profile(self):
        self.server.instances['web'] = {
            'name': 'web', 'type': 'container', 'status': 'Running', 'config': {}, 'files': {},
            'devices': {'data': {'type': 'disk', 'path': '/srv', 'source': 'data', 'pool': 'default'}},
            'expanded_devices': {'root': {'type': 'disk', 'path': '/', 'pool': 'default', 'size': '10GiB'},
                                 'data': {'type': 'disk', 'path': '/srv', 'source': 'data', 'pool': 'default'}}}
        code, answer, err = self.act('resize-disk', instance='web', size='30GiB', restart='yes')
        self.assertEqual(code, 0, err)
        [patch] = [e for e in self.server.events if e[0] == 'patch']
        self.assertEqual(patch[2]['devices'], {
            'data': {'type': 'disk', 'path': '/srv', 'source': 'data', 'pool': 'default'},
            'root': {'type': 'disk', 'path': '/', 'pool': 'default', 'size': '30GiB'}})
        self.assertIn('gets one of its own of 30GiB', err)
        self.assertIn('A container has the new size at once; it was not restarted.', answer['message'])
        self.assertFalse([e for e in self.server.events if e[0] == 'restart'])

    def test_change_leaves_a_disk_that_was_not_given_anew(self):
        self.assertEqual(self.create()[0], 0)
        self.assertEqual(self.act('resize-disk', instance='berth-studio-2', size='100GiB')[0], 0)
        self.server.events.clear()
        # berth's record still says 60GiB, which it sends with every change.
        code, _, err = self.run_verb('change', id='berth-studio-2', changed=['memory'],
                                     arguments={'memory': '4GiB', 'disk': '60GiB'})
        self.assertEqual(code, 0, err)
        self.assertEqual(self.server.instances['berth-studio-2']['devices']['root']['size'], '100GiB')
        code, _, err = self.run_verb('change', id='berth-studio-2', changed=['disk'], arguments={'disk': '80GiB'})
        self.assertEqual(code, 1)
        self.assertIn('berth only grows it', err)

    def test_change_sets_new_limits_and_grows_the_disk(self):
        self.create()
        code, answer, err = self.run_verb('change', id='berth-studio-2',
                                          arguments={'cpus': '2', 'memory': '4GiB', 'disk': '80GiB'})
        self.assertEqual(code, 0, err)
        self.assertEqual(answer, {'state': 'running', 'endpoints': []})
        [patch] = [e for e in self.server.events if e[0] == 'patch']
        self.assertEqual(patch[2]['config'], {'limits.cpu': '2', 'limits.memory': '4GiB'})
        self.assertEqual(patch[2]['devices']['root']['size'], '80GiB')
        self.server.instances['web'] = {'name': 'web', 'type': 'container', 'status': 'Running',
                                        'config': {}, 'files': {}}
        code, _, err = self.run_verb('change', id='web', arguments={'cpus': '1'})
        self.assertEqual(code, 1)
        self.assertIn('not made by berth', err)

    def test_proxy_devices_are_endpoints_on_the_server(self):
        self.create()
        self.server.instances['berth-studio-2']['expanded_devices'] = {
            'root': {'type': 'disk', 'path': '/', 'pool': 'default'},
            'web': {'type': 'proxy', 'listen': 'tcp:0.0.0.0:8080', 'connect': 'tcp:127.0.0.1:80'},
            'dns': {'type': 'proxy', 'listen': 'udp:192.0.2.1:5353', 'connect': 'udp:127.0.0.1:53'},
            'Socket': {'type': 'proxy', 'listen': 'unix:/run/web.sock', 'connect': 'tcp:127.0.0.1:80'},
        }
        code, answer, err = self.run_verb('status', id='berth-studio-2')
        self.assertEqual(code, 0, err)
        # The fake answers on this machine's loopback, so its addresses are this machine's.
        self.assertEqual(answer['endpoints'], [
            {'name': 'dns', 'address': '192.0.2.1', 'port': 5353, 'protocol': 'udp', 'local': True},
            {'name': 'web', 'address': '127.0.0.1', 'port': 8080, 'protocol': 'tcp', 'hint': 'http', 'local': True},
        ])

    def test_a_command_runs_in_the_instance_with_its_output_and_exit_status(self):
        self.create()
        code, answer, err = self.run_verb('exec', id='berth-studio-2', command=['sh', '-c', 'echo hi; exit 2'])
        self.assertEqual(code, 0, err)
        self.assertEqual(answer, {'exit_code': 2})
        self.assertIn('hi from web\none warning\n', err)
        [exec_] = [e for e in self.server.events if e[0] == 'exec']
        self.assertEqual(exec_[2]['command'], ['sh', '-c', 'echo hi; exit 2'])
        self.assertEqual((exec_[2]['record-output'], exec_[2]['interactive']), (True, False))
        self.assertEqual({e[1] for e in self.server.events if e[0] == 'log-delete'}, {'exec_1.stdout', 'exec_1.stderr'})
        for fields, said in [({'stdin': 'x'}, 'without standard input'), ({'command': 'ls'}, 'list of words')]:
            code, _, err = self.run_verb('exec', id='berth-studio-2', **{'command': ['ls'], **fields})
            self.assertEqual(code, 1)
            self.assertIn(said, err)

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
