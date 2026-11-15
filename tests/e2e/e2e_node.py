"""A node on the bare test host `dock`: berth applies it without a desktop,
with a made-up plugin's feature for a node that offers a provisioner, and
registers that provisioner, naming the node. Applying again changes nothing
of it; a feature that is taken out takes its provisioner along, but not while
it holds a deployment; and the node is not forgotten while one is there. The
endpoints of a deployment of that provisioner are reached through the node's
controller, on its loopback and on its own network.

The test host is a container without systemd and without the internet, so
tests/e2e/site.yml applies the node's tasks that need neither: the guard,
its SSH, the features, berthd and the record of the apply. The packages, the
firewall and Tailscale are left out, and so are the features incus/server and
docker/engine, whose engines do not run here: tests/test_node.py reads them,
and runs the firewall's helper and the server's upgrade against fakes.

It runs last on `dock`, as it leaves a node behind."""
import json
import stat
import unittest

from harness import (HOST, OPERATOR, Serve, WebSocket, Workstation, hand_over_operator_key, needs_e2e, shell_quote,
                     wait_for)

# The port of the listener on the node that sends back what it gets.
ECHO = 7071

# A provisioner that makes deployments and says nothing else of note.
# A deployment's endpoints are a listener on the node's loopback, the same
# one at the node's own address, and an address no network of the node holds.
PROGRAM = r'''#!/usr/bin/env python3
import json, socket, sys
verb = sys.argv[1]
request = json.load(sys.stdin)
with open(__file__ + '.node', 'w') as said:
    said.write(request.get('node', ''))
if verb == 'status' and not request.get('id'):
    print(json.dumps({'message': 'The crane at ' + request['arguments'].get('place', 'nowhere') + ' answers'}))
elif verb in ('create', 'change'):
    print(json.dumps({'id': 'load-' + request['name'], 'state': 'running'}))
elif verb == 'status':
    endpoints = [{'name': 'db', 'address': '127.0.0.1', 'port': ECHO, 'local': True},
                 {'name': 'lan', 'address': socket.gethostbyname(HOST), 'port': ECHO, 'local': True},
                 {'name': 'far', 'address': '192.0.2.1', 'port': ECHO, 'local': True}]
    print(json.dumps({'state': 'running', 'endpoints': endpoints}))
elif verb == 'destroy':
    print(json.dumps({'state': 'gone'}))
else:
    print(json.dumps({}))
'''

# The crane's tasks: what a feature of a node does to offer its provisioner.
TASKS = '''---
- name: Keep the certificate berth gave the crane to trust
  ansible.builtin.copy:
    dest: /etc/yard-crane.crt
    content: "{{ devbox_client_certificates[devbox_plugin_feature].certificate }}"
    mode: '0644'
- name: Offer the crane's provisioner
  ansible.builtin.set_fact:
    devbox_node_units: "{{ devbox_node_units + ['yard-crane'] }}"
    devbox_offer:
      settings:
        place: "https://{address}:9999"
      secrets:
        yard_server_cert: "PUBLIC-CERT-OF-THE-CRANE"
      fingerprint: "0123abcd"
'''

MANIFEST = {
    'name': 'berth-plugin-yard', 'version': '1.0.0', 'displayName': 'Yard', 'files': ['bin', 'tasks'],
    'berth': {
        'features': [
            {'id': 'crane', 'label': 'Crane', 'types': ['node'], 'offers': ['box'],
             'trusts': {'certificate': 'yard_client_cert', 'key': 'yard_client_key'},
             'tasks': 'tasks/crane.yml',
             'actions': [{'name': 'crane-status', 'label': 'Crane', 'user': 'root', 'extends': 'status',
                          'run': 'echo crane'}]},
            {'id': 'bench', 'label': 'Bench'},
        ],
        'provisioners': [{
            'id': 'box', 'label': 'Yard box', 'command': 'bin/provision', 'protocol': 2,
            'secrets': ['yard_client_cert', 'yard_client_key', {'name': 'yard_server_cert', 'required': False}],
            'arguments': [{'name': 'place', 'label': 'Place', 'required': True}],
            'kinds': [{'id': 'deployment', 'label': 'Load', 'spec': {'format': 'text', 'label': 'Load'}}],
        }],
    },
}


@needs_e2e
class Node(unittest.TestCase):
    """The tests build on each other, in the order of their names."""

    # What the workstations of the tests before this one left on `dock`: a node
    # is not applied over a workstation that berth set up, so it goes first.
    LEFT = ('pkill -x berthd; userdel -r agent; rm -rf /etc/berth /var/lib/berth /etc/devbox /var/lib/devbox '
            '/etc/sudoers.d/90-devbox-agent /etc/ssh/sshd_config.d/00-devbox.conf; true')

    @classmethod
    def setUpClass(cls):
        cls.station = Workstation('node', ssh_key=False)
        folder = cls.station.berth_dir / 'plugins/yard'
        (folder / 'bin').mkdir(parents=True)
        (folder / 'tasks').mkdir()
        (folder / 'package.json').write_text(json.dumps(MANIFEST))
        (folder / 'tasks/crane.yml').write_text(TASKS)
        program = folder / 'bin/provision'
        program.write_text(PROGRAM.replace('ECHO', str(ECHO)).replace('HOST', repr(HOST)))
        program.chmod(program.stat().st_mode | stat.S_IXUSR)
        hand_over_operator_key(cls.station.own_public())
        key = cls.station.own_key_file()
        wait_for('the operator taking the key of berth', lambda: cls.station.ssh(
            OPERATOR, 'true', key=key, check=False).returncode == 0, timeout=60)
        cls.station.ssh(OPERATOR, f'sudo -n sh -c {shell_quote(cls.LEFT)}', key=key, check=False)
        cls.station.berth('host', 'add', HOST, HOST, '--user', OPERATOR, '--type', 'node', input='')

    @classmethod
    def tearDownClass(cls):
        hand_over_operator_key()

    def berth(self, *args, **kwargs):
        kwargs.setdefault('input', '')
        return self.station.berth(*args, **kwargs)

    def root(self, command, check=True):
        return self.station.ssh(OPERATOR, f'sudo -n sh -c {shell_quote(command)}', key=self.station.own_key,
                                check=check)

    def profile(self, features):
        profile = self.station.read_profile()
        profile['ansible_vars'] = {'devbox_plugin_features': list(features), 'devbox_start_services': False,
                                   'devbox_install_tailscale': False, 'devbox_manage_firewall': False}
        self.station.write_profile(profile)
        return profile

    def provisioners(self):
        return [line.split()[0] for line in self.berth('provisioner').stdout.splitlines()
                if line.strip() and not line.startswith(' ')]

    def test_1_a_feature_for_a_workstation_is_refused_before_anything_runs(self):
        self.assertEqual(self.station.read_profile()['type'], 'node')
        self.profile(['yard/crane', 'yard/bench'])
        refused = self.berth('apply', HOST, check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn(f'{HOST} is a node, and yard/bench is for a workstation', refused.stderr)
        self.assertNotEqual(self.root('test -e /etc/berth/applied.json', check=False).returncode, 0)

    def test_2_applying_the_node_registers_what_its_feature_offers(self):
        self.profile(['yard/crane'])
        done = self.berth('apply', HOST, timeout=900)
        name = f'{HOST}-yard-box'
        self.assertIn(f'✓ Registered the provisioner {name} (yard/box) of {HOST}', done.stdout)
        self.assertIn('pins the server\'s certificate 0123abcd', done.stdout)
        page = self.berth('provisioner', name).stdout
        self.assertIn(f'place=https://{HOST}:9999', page)
        self.assertIn(f'The crane at https://{HOST}:9999 answers', page)
        self.assertIn(f'Node      {HOST}, whose feature offers it', page)
        # The host records what it is and what it offers, and says so.
        applied = json.loads(self.root('cat /etc/berth/applied.json').stdout)
        self.assertEqual(applied['level'], 'node')
        self.assertEqual(applied['offers'][0]['feature'], 'yard/crane')
        self.assertEqual(self.station.link().get('/v1/version')['type'], 'node')
        # Only the certificate berth made went to the node, never its key.
        trusted = self.root('cat /etc/yard-crane.crt').stdout
        self.assertTrue(trusted.startswith('-----BEGIN CERTIFICATE-----'))
        self.assertNotIn('PRIVATE KEY', self.root('cat /etc/berth/*.json /etc/yard-crane.crt').stdout)
        # Sessions run as the SSH user; nothing of a workstation is there.
        self.assertEqual(self.berth('exec', HOST, '--', 'id', '-un').stdout.strip(), OPERATOR)
        for absent in ('id agent', 'test -e /etc/devbox/AGENT.md', 'test -e /etc/sudoers.d/90-devbox-agent',
                       'command -v Xvnc'):
            self.assertNotEqual(self.root(absent, check=False).returncode, 0, absent)
        listed = self.berth('actions', HOST).stdout
        for action in ('status', 'update', 'secrets'):
            self.assertRegex(listed, rf'(?m)^  {action}\s+')
        # No berth server's container here, so no upgrade of one.
        self.assertNotRegex(listed, r'(?m)^  upgrade-server\s+')
        self.assertIn('crane', self.berth('run', HOST, 'status').stdout)
        # SSH takes keys only, and leaves root none, as berth logs in as the operator.
        sshd = self.root('cat /etc/ssh/sshd_config.d/00-devbox.conf').stdout
        self.assertIn('PasswordAuthentication no', sshd)
        self.assertIn('PermitRootLogin no', sshd)

    def test_3_applying_again_registers_nothing_new(self):
        done = self.berth('apply', HOST, timeout=900)
        self.assertNotIn('Registered the provisioner', done.stdout)
        self.assertEqual(self.provisioners().count(f'{HOST}-yard-box'), 1)

    def test_4_a_provisioner_that_holds_a_deployment_stays_with_its_node(self):
        name = f'{HOST}-yard-box'
        self.berth('deploy', 'crate', '--provisioner', name, '--spec', '-', input='one crate\n')
        self.profile([])
        refused = self.berth('apply', HOST, check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn(f'offers the provisioner {name} through yard/crane, which still holds', refused.stderr)
        removed = self.station.remove_host(check=False)
        self.assertNotEqual(removed.returncode, 0)
        self.assertIn(f'is the node of the provisioner {name}', removed.stderr)
        self.assertIn(name, self.provisioners())

    def test_5_a_deployments_endpoints_are_reached_through_the_nodes_controller(self):
        echo = ('import socket, threading\n'
                's = socket.create_server(("0.0.0.0", ECHO))\n'
                'def serve(c):\n'
                '    while True:\n'
                '        data = c.recv(65536)\n'
                '        if not data: break\n'
                '        c.sendall(data)\n'
                '    c.shutdown(socket.SHUT_WR)\n'
                'while True: threading.Thread(target=serve, args=(s.accept()[0],)).start()\n').replace(
                    'ECHO', str(ECHO))
        self.root(f'setsid python3 -c {shell_quote(echo)} berth-e2e-echo </dev/null >/dev/null 2>&1 &')
        serve = Serve(self.station)
        try:
            resources = serve.call('GET', '/v1/resources')
            [crate] = [resource for resource in resources if resource['name'] == 'crate']
            seen = serve.call('GET', f'/v1/resources/{crate["id"]}')
            self.assertEqual([endpoint['name'] for endpoint in seen['endpoints']], ['db', 'lan', 'far'], seen)
            node = (self.station.berth_dir / 'plugins/yard/bin/provision.node').read_text()
            self.assertEqual(node, HOST)
            path = f'/v1/resources/{crate["id"]}/endpoints'
            data = bytes(range(256)) * 4096
            for endpoint in ('db', 'lan'):
                back = wait_for(f'the endpoint {endpoint}', lambda endpoint=endpoint: self.through(serve, f'{path}/{endpoint}', data),
                                timeout=30)
                self.assertEqual(back, data, endpoint)
            # Off the node's networks, and the node's stream for the server alone, which no caller reaches.
            for refused in (f'{path}/far', f'/v1/hosts/{HOST}/controller/endpoints/127.0.0.1/{ECHO}'):
                with self.assertRaises(PermissionError) as raised:
                    WebSocket(serve.port, refused, protocols=[serve.token])
                self.assertTrue(str(raised.exception).startswith('403'), raised.exception)
        finally:
            serve.stop()
            self.root('pkill -f berth-e2e-echo', check=False)

    @staticmethod
    def through(serve, path, data):
        """What comes back from an endpoint for `data` and this side's end, or None while it is refused."""
        try:
            tunnel = WebSocket(serve.port, path, protocols=[serve.token])
        except PermissionError:
            return None
        try:
            for start in range(0, len(data), 65536):
                tunnel.send(data[start:start + 65536])
            tunnel.send({'eof': True})
            back = b''
            while True:
                frame = tunnel.recv()
                if frame is None or isinstance(frame, dict):
                    return back
                back += frame
        finally:
            tunnel.close()

    def test_6_without_its_feature_the_provisioner_goes(self):
        name = f'{HOST}-yard-box'
        self.berth('resource', 'crate', 'remove', '--yes')
        done = self.berth('apply', HOST, timeout=900)
        self.assertIn(f'✓ Forgot the provisioner {name}', done.stdout)
        self.assertNotIn(name, self.provisioners())
        self.assertNotIn('offers', json.loads(self.root('cat /etc/berth/applied.json').stdout))


if __name__ == '__main__':
    unittest.main()
