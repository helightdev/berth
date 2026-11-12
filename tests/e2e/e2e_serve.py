"""berth serve over TCP: its routes and the portal, the roles of access
tokens, setting the server up through the API, applying as a job, and the
page for a browser."""
import json
import unittest

from harness import (AGENT, HOST, Serve, Workstation, needs_e2e, provisioned, random_text, served, unb64,
                     wait_for)


def text_of(started):
    return unb64(started.get('stdout_b64'))


@needs_e2e
class Routes(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.serve = served()

    def test_the_server_and_its_hosts(self):
        version = self.serve.call('GET', '/v1/version')
        self.assertEqual(version['mode'], 'serve')
        self.assertEqual(self.serve.call('GET', '/v1/access'), {'role': 'admin', 'setup': True})
        hosts = wait_for('the host answering through the server', lambda: [
            host for host in self.serve.call('GET', '/v1/hosts') if host['name'] == HOST and host['reachable']])
        self.assertTrue(hosts[0]['version'])
        self.assertEqual(self.serve.call('GET', f'/v1/hosts/{HOST}')['address'], HOST)

    def test_nothing_without_the_token(self):
        for token in ('', 'not-the-token', self.serve.token[:-1]):
            status, answer = self.serve.request('GET', '/v1/hosts', token=token)
            self.assertEqual(status, 401, answer)
            self.assertEqual(answer['error']['code'], 'unauthorized')

    def test_a_hosts_operations_go_through(self):
        started = self.serve.call('POST', f'/v1/hosts/{HOST}/sessions?wait=30s',
                                  {'kind': 'process', 'command': ['id', '-un']})
        self.assertEqual((started['exit_code'], text_of(started)), (0, f'{AGENT}\n'))
        tasks = self.serve.call('GET', f'/v1/hosts/{HOST}/tasks')
        self.assertIn('secrets', [task['name'] for task in tasks])
        path = f'/v1/hosts/{HOST}/files?path=/home/{AGENT}/through-serve.txt&overwrite=true'
        status, _ = self.serve.request('PUT', path, b'written through berth serve',
                                       headers={'Content-Type': 'application/octet-stream'})
        self.assertEqual(status, 200)
        status, read, _ = self.serve.request('GET', path.split('&')[0], raw=True)
        self.assertEqual((status, read), (200, b'written through berth serve'))
        status, _ = self.serve.request('GET', '/v1/hosts/nowhere/sessions')
        self.assertEqual(status, 404)

    def test_the_key_value_store(self):
        key = random_text('key')
        first = self.serve.call('PUT', f'/v1/kv/e2e/{key}', {'colour': 'blue'})
        self.assertEqual(self.serve.call('GET', f'/v1/kv/e2e/{key}')['value'], {'colour': 'blue'})
        status, _ = self.serve.request('PUT', f'/v1/kv/e2e/{key}', {'colour': 'red'},
                                       headers={'If-Match': str(first['revision'] + 100)})
        self.assertIn(status, (409, 412))
        self.serve.call('PUT', f'/v1/kv/e2e/{key}', {'colour': 'red'})
        listed = self.serve.call('GET', '/v1/kv/e2e')
        entries = listed['entries'] if isinstance(listed, dict) else listed
        self.assertIn(key, [entry['key'] for entry in entries])
        self.serve.call('DELETE', f'/v1/kv/e2e/{key}')
        status, _ = self.serve.request('GET', f'/v1/kv/e2e/{key}')
        self.assertEqual(status, 404)

    def test_the_inbox_takes_in_what_the_host_keeps(self):
        body = {'note': random_text('inbox')}
        self.serve.call('POST', f'/v1/hosts/{HOST}/sessions?wait=30s', {
            'kind': 'process', 'command': ['box', 'publish', 'e2e/inbox', json.dumps(body), '--keep', 'log']})

        def arrived():
            listed = self.serve.call('GET', f'/v1/inbox?host={HOST}&topic=e2e/inbox')
            messages = listed['messages'] if isinstance(listed, dict) else listed
            return [item for item in messages if item['body'] == body]
        found = wait_for('the message in the inbox', arrived)[0]
        number = found['id']
        self.serve.call('POST', f'/v1/inbox/{HOST}/{number}/done')
        self.assertTrue(arrived()[0]['done'])
        self.serve.call('DELETE', f'/v1/inbox/{HOST}/{number}/done')
        self.assertFalse(arrived()[0]['done'])


@needs_e2e
class Portal(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.serve = served()

    def setUp(self):
        self.portal = self.serve.portal()

    def tearDown(self):
        self.portal.close()

    def test_calls_of_the_server_and_of_a_host(self):
        self.assertEqual(self.portal.result('version')['mode'], 'serve')
        self.assertIn(HOST, [host['name'] for host in self.portal.result('hosts.list')])
        started = self.portal.result('sessions.start', {'wait': '30s'},
                                     {'kind': 'process', 'command': 'echo through the portal'}, host=HOST)
        self.assertEqual(text_of(started), 'through the portal\n')
        missing = self.portal.call('nothing.here')
        self.assertEqual(missing['error']['status'], 404)

    def test_a_session_streams_its_output(self):
        marker = random_text('streamed')
        started = self.portal.result('sessions.start', data={
            'kind': 'process', 'command': f'sleep 1; echo {marker}'}, host=HOST)
        texts, data, _ = self.portal.stream('sessions.attach', {'id': started['id']}, host=HOST,
                                            until=lambda texts, data: marker.encode() in data)
        self.assertIn(marker.encode(), data)

    def test_the_store_over_the_portal(self):
        key = random_text('portal')
        self.portal.result('kv.set', {'namespace': 'e2e', 'key': key}, {'n': 1})
        self.assertEqual(self.portal.result('kv.get', {'namespace': 'e2e', 'key': key})['value'], {'n': 1})
        self.portal.result('kv.delete', {'namespace': 'e2e', 'key': key})


@needs_e2e
class Roles(unittest.TestCase):
    """Access tokens of the issuer berth serve trusts, with the role they name."""

    @classmethod
    def setUpClass(cls):
        cls.serve = served()
        cls.issuer = cls.serve.issuer

    def status(self, scope, method, path, body=None, **token):
        return self.serve.request(method, path, body, token=self.issuer.token(scope, **token))

    def test_each_role_may_do_what_it_names(self):
        process = {'kind': 'process', 'command': ['true']}
        asked = [
            ('GET', '/v1/hosts', None, 'view'),
            ('GET', f'/v1/hosts/{HOST}/sessions', None, 'view'),
            ('POST', f'/v1/hosts/{HOST}/sessions?wait=10s', process, 'operate'),
            ('GET', f'/v1/hosts/{HOST}/files?path=/home/{AGENT}', None, 'operate'),
            ('GET', '/v1/secrets', None, 'admin'),
            ('GET', f'/v1/hosts/{HOST}/profile', None, 'admin'),
        ]
        order = ['view', 'operate', 'admin']
        for role in order:
            for method, path, body, needed in asked:
                status, answer = self.status(f'openid berth:{role}', method, path, body)
                allowed = order.index(role) >= order.index(needed)
                self.assertEqual(status == 200, allowed, (role, method, path, status, answer))
                if not allowed:
                    self.assertEqual(status, 403, answer)

    def test_the_caller_is_told_its_role(self):
        status, access = self.status('berth:operate', 'GET', '/v1/access', subject='grace')
        self.assertEqual((status, access['role'], access['subject']), (200, 'operate', 'grace'))

    def test_the_portal_asks_for_each_operation(self):
        portal = self.serve.portal(self.issuer.token('berth:view'))
        try:
            self.assertTrue(portal.result('hosts.list'))
            refused = portal.call('sessions.start', data={'kind': 'process', 'command': 'true'}, host=HOST)
            self.assertEqual(refused['error']['status'], 403)
            self.assertEqual(portal.call('secrets.list')['error']['status'], 403)
        finally:
            portal.close()

    def test_tokens_that_are_not_good(self):
        from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey  # noqa: PLC0415
        bad = {
            # Past the minute of leeway for clocks that differ.
            'expired': {'expires': -120},
            'for another audience': {'audience': 'someone-else'},
            'from another issuer': {'issuer': 'http://192.0.2.50'},
            'signed with another key': {'key': Ed25519PrivateKey.generate()},
        }
        for what, token in bad.items():
            status, answer = self.status('berth:admin', 'GET', '/v1/hosts', **token)
            self.assertEqual(status, 401, (what, answer))
        status, answer = self.status('openid profile', 'GET', '/v1/hosts')
        self.assertEqual(status, 403, answer)


@needs_e2e
class SetUp(unittest.TestCase):
    """What an admin sets the server up with, over the API."""

    @classmethod
    def setUpClass(cls):
        cls.serve = served()

    def test_secrets_and_registries(self):
        name = random_text('token')
        self.serve.call('PUT', f'/v1/secrets/{name}', {'value': 'kept on the server'})
        self.assertIn(name, self.serve.call('GET', '/v1/secrets')['names'])
        self.serve.call('DELETE', f'/v1/secrets/{name}')
        self.assertNotIn(name, self.serve.call('GET', '/v1/secrets')['names'])
        registry = {'type': 'npm', 'url': 'http://192.0.2.60/npm/'}
        self.serve.call('PUT', '/v1/registries/e2e-npm', registry)
        self.assertEqual(self.serve.call('GET', '/v1/registries')['registries']['e2e-npm'], registry)
        self.serve.call('DELETE', '/v1/registries/e2e-npm')
        self.assertNotIn('e2e-npm', self.serve.call('GET', '/v1/registries')['registries'])

    def test_what_a_new_host_starts_from(self):
        catalogue = self.serve.call('GET', '/v1/setup')
        self.assertIn('operator_ssh_public_keys', catalogue['template'])
        self.assertIn('claude/cli', [feature['key'] for feature in catalogue['features']])
        own = self.serve.station.own_key.with_suffix('.pub').read_text().strip()
        self.assertEqual(catalogue['key'], own)
        name = self.serve.call('POST', '/v1/setup/generate', {'what': 'hostname'})['value']
        self.assertRegex(name, r'^devbox-')
        made = self.serve.call('POST', '/v1/setup/generate', {'what': 'password', 'host': 'draft', 'user': 'ops'})
        self.assertTrue(made['value'].startswith('$6$') and made['password'], made)
        hashed = self.serve.call('POST', '/v1/setup/generate', {'what': 'password', 'password': 'correct horse'})
        self.assertTrue(hashed['value'].startswith('$6$'))

    def test_a_host_is_made_seeded_and_removed(self):
        self.serve.call('PUT', '/v1/hosts/cloud-2/profile', {
            **provisioned().read_profile(), 'hostname': 'cloud-2', 'builder': {},
            'operator_password_hash': '$6$e2e$' + 'x' * 86,
            'release_url': 'https://192.0.2.30/berth-release.tar.gz'})
        self.assertFalse(self.serve.call('GET', '/v1/hosts/cloud-2')['reachable'])
        seed = self.serve.call('POST', '/v1/hosts/cloud-2/seed')['user_data']
        own = self.serve.station.own_key.with_suffix('.pub').read_text().split()[1]
        self.assertTrue(seed.startswith('#cloud-config'))
        self.assertIn(own, seed)
        self.serve.call('DELETE', '/v1/hosts/cloud-2')
        status, _ = self.serve.request('GET', '/v1/hosts/cloud-2')
        self.assertEqual(status, 404)

    def test_the_server_sends_its_secrets_to_the_host(self):
        value = random_text('from-the-server')
        self.serve.call('PUT', '/v1/secrets/server-token', {'value': value})
        profile = self.serve.call('GET', f'/v1/hosts/{HOST}/profile')
        kept = profile.get('secrets', [])
        profile['secrets'] = [{'name': 'server-token', 'env': 'SERVER_TOKEN'}]
        self.serve.call('PUT', f'/v1/hosts/{HOST}/profile', profile)
        try:
            session = self.serve.call('POST', f'/v1/hosts/{HOST}/secrets')
            wait_for('the secrets task to end', lambda: self.serve.call(
                'GET', f'/v1/hosts/{HOST}/sessions/{session["id"]}').get('exit_code') == 0)
            env = provisioned().root_on_host(f'cat /home/{AGENT}/.config/devbox/secrets.env').stdout
            self.assertIn(f'export SERVER_TOKEN={value}', env)
        finally:
            profile['secrets'] = kept
            self.serve.call('PUT', f'/v1/hosts/{HOST}/profile', profile)

    def test_settings_open_to_callers(self):
        self.serve.call('PUT', '/v1/settings/retention_days', {'value': '30'})
        settings = {setting['key']: setting for setting in self.serve.call('GET', '/v1/settings')}
        self.assertEqual(settings['retention_days']['value'], '30')
        status, _ = self.serve.request('PUT', '/v1/settings/oauth_issuer', {'value': 'http://192.0.2.1'})
        self.assertEqual(status, 403)

    def test_applying_is_a_job_of_the_server(self):
        job = self.serve.call('POST', f'/v1/hosts/{HOST}/apply')
        portal = self.serve.portal()
        try:
            texts, data, _ = portal.stream('jobs.follow', {'id': job['id']}, timeout=600,
                                           until=lambda texts, _: any('exited' in (text or {}) for text in texts))
        finally:
            portal.close()
        exited = next(text['exited'] for text in texts if 'exited' in (text or {}))
        self.assertEqual(exited['exit_code'], 0, data.decode(errors='replace')[-3000:])
        self.assertIn(b'PLAY RECAP', data)
        listed = self.serve.call('GET', '/v1/jobs')
        jobs = listed['jobs'] if isinstance(listed, dict) else listed
        self.assertIn(job['id'], [known['id'] for known in jobs])
        output = self.serve.call('GET', f'/v1/jobs/{job["id"]}/output')
        self.assertIn('failed=0', unb64(output['data_b64']) if isinstance(output, dict) else output)
        # The operator now takes the server's key as well.
        keys = provisioned().ssh('ops', 'cat ~/.ssh/authorized_keys').stdout
        own = self.serve.station.own_key.with_suffix('.pub').read_text().strip()
        self.assertIn(own, keys)


@needs_e2e
class ClosedSetUp(unittest.TestCase):
    def test_set_up_can_be_closed_to_the_network(self):
        station = Workstation('closed', ssh_key=False)
        closed = Serve(station, web=False, env={'BERTH_SERVE_SETUP': 'off'})
        try:
            self.assertEqual(closed.call('GET', '/v1/access'), {'role': 'operate', 'setup': False})
            status, answer = closed.request('PUT', '/v1/secrets/x', {'value': 'y'})
            self.assertEqual(status, 403)
            self.assertIn('set-up', answer['error']['message'])
            # The page is only there with --web.
            status, _ = closed.request('GET', '/', token='')
            self.assertNotEqual(status, 200)
        finally:
            closed.stop()


@needs_e2e
class Page(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.serve = served()

    def test_the_page_is_given_to_anyone(self):
        status, page, headers = self.serve.request('GET', '/', token='', raw=True)
        self.assertEqual(status, 200)
        self.assertTrue(headers['content-type'].startswith('text/html'), headers)
        self.assertIn(b'berth-web', page)
        status, script, headers = self.serve.request('GET', '/berth-web.js', token='', raw=True)
        self.assertEqual(status, 200)
        self.assertIn('javascript', headers['content-type'])
        status, wasm, headers = self.serve.request('GET', '/berth-web_bg.wasm', token='', raw=True)
        self.assertEqual((status, wasm[:4], headers['content-type']), (200, b'\0asm', 'application/wasm'))
        # What the page shows it fetches with a token.
        self.assertEqual(self.serve.request('GET', '/v1/hosts', token='')[0], 401)

