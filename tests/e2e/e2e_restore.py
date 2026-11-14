"""An installation restored from its database and its key file alone.

A berth serve keeps its state in an encrypted SQLite store. The test gives it
the test host, a finished job, a secret, a setting and a key of the ring, stops
it and wipes its configuration, data and runtime directories. Only the two
files of the store are kept, and a berth serve started on them is the same
installation: the host, the token, berth's key, the secret, the setting and
the job are there. The job's output stays on the machine that ran it. Without
the key file, berth serve refuses and writes nothing.

It runs on `studio` after the other tests of berth serve and applies nothing:
the host is the one provisioned() set up, and the server is its own."""
import hashlib
import shutil
import subprocess
import unittest

from harness import (AGENT, HOST, OPERATOR, SCRATCH, Serve, Workstation, _provisioned, free_port, hand_over_operator_key,
                     light_vars, needs_e2e, provisioned, random_text, unb64, wait_for)


def wipe(station):
    """Remove everything under the station's configuration, data and runtime directories."""
    for folder in (station.config, station.data, station.run_dir):
        for entry in folder.iterdir():
            shutil.rmtree(entry) if entry.is_dir() and not entry.is_symlink() else entry.unlink()


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def public_of(key_file):
    """The public half of a private key file, as the two fields of an authorized_keys line."""
    shown = subprocess.run(['ssh-keygen', '-y', '-f', str(key_file)], capture_output=True, text=True, check=True)
    return shown.stdout.split()[:2]


def jobs_of(serve):
    listed = serve.call('GET', '/v1/jobs')
    return listed['jobs'] if isinstance(listed, dict) else listed


@needs_e2e
class Restore(unittest.TestCase):
    """The tests look at the restored server, set up once, in the order of their names."""

    @classmethod
    def setUpClass(cls):
        cls.station = station = Workstation('restore', ssh_key=False, trust=False)
        cls.worker = provisioned()
        keep = SCRATCH / 'restore-store'
        cls.moved = SCRATCH / 'restore-moved'
        cls.keyless = SCRATCH / 'restore-keyless'
        for folder in (keep, cls.moved, cls.keyless):
            folder.mkdir(mode=0o700)
        station.env.update(BERTH_STORE=str(keep / 'berth.db'), BERTH_STORE_KEY=str(keep / 'store.key'))
        # What the store says of itself when it is sealed.
        cls.encrypted = station.berth('store', 'encrypt')
        cls.own_public = station.own_public()
        cls.give_the_host_the_keys()
        key = station.own_key_file()
        wait_for('the operator taking the key of the server', lambda: station.ssh(
            OPERATOR, 'true', key=key, check=False).returncode == 0)
        serve = Serve(station, web=False)
        try:
            cls.fill(serve)
        finally:
            serve.stop()
        cls.status = station.berth('store').stdout
        cls.sealed_file = (keep / 'berth.db').read_bytes()
        # The database and the key file are all that is kept.
        for name in ('berth.db', 'store.key'):
            shutil.copy2(keep / name, cls.moved / name)
        shutil.copy2(keep / 'berth.db', cls.keyless / 'berth.db')
        wipe(station)
        shutil.rmtree(keep)
        station.env.update(BERTH_STORE=str(cls.moved / 'berth.db'), BERTH_STORE_KEY=str(cls.moved / 'store.key'))
        cls.serve = Serve(station, web=False)

    @classmethod
    def give_the_host_the_keys(cls):
        """The operator takes the server's key the way the first one did, besides the others'."""
        cls.others = [cls.worker.public]
        if 'serve' in _provisioned:
            cls.others.append(_provisioned['serve'].station.own_public())
        hand_over_operator_key(*cls.others, cls.own_public)

    @classmethod
    def fill(cls, serve):
        """Put a host, a job, a secret, a setting and a key in the first server, and note them."""
        station = cls.station
        host = cls.worker.read_profile()
        host['access']['auth'] = 'berth'
        host['ansible_vars'] = light_vars()
        for key in ('secrets', 'registries', 'tailscale_auto_key', 'tailscale_auth_key', 'tailscale_login_server'):
            host.pop(key, None)
        serve.call('PUT', f'/v1/hosts/{HOST}/profile', host)
        wait_for('the host answering through the server', lambda: [
            known for known in serve.call('GET', '/v1/hosts') if known['name'] == HOST and known['reachable']])
        started = serve.call('POST', f'/v1/hosts/{HOST}/actions/status', expect=202)
        cls.job = started['job']['id']

        def ended():
            job = serve.call('GET', f'/v1/jobs/{cls.job}')
            return job if job.get('ended') else None
        cls.finished = wait_for('the job to end', ended, timeout=120)
        output = serve.call('GET', f'/v1/jobs/{cls.job}/output')
        assert unb64(output['data_b64']).strip(), 'the job wrote nothing'
        cls.secret = random_text('restored-secret')
        cls.secret_value = random_text('value')
        serve.call('PUT', f'/v1/secrets/{cls.secret}', {'value': cls.secret_value})
        serve.call('PUT', '/v1/settings/retention_days', {'value': '37'})
        cls.ring_key = random_text('ring')
        serve.call('PUT', f'/v1/keys/{cls.ring_key}')
        cls.keys = {key['name']: key['public'].split()[:2] for key in serve.call('GET', '/v1/keys')['keys']}
        cls.token = serve.token
        cls.own_file_public = public_of(station.own_key)

    @classmethod
    def tearDownClass(cls):
        cls.serve.stop()
        hand_over_operator_key(*cls.others)

    def test_1_the_store_was_sealed_and_the_secret_value_is_not_in_the_file(self):
        self.assertIn('Encrypted  yes', self.status)
        self.assertTrue(self.secret_value.encode() not in self.sealed_file, 'the secret is in the file as it is')

    def test_2_the_host_is_listed_and_reachable(self):
        found = wait_for('the restored server reaching the host', lambda: [
            known for known in self.serve.call('GET', '/v1/hosts') if known['name'] == HOST and known['reachable']])
        self.assertTrue(found[0]['version'])
        self.assertEqual(self.serve.call('GET', f'/v1/hosts/{HOST}')['address'], HOST)

    def test_3_the_token_and_the_key_of_berth_are_the_same(self):
        # Compared without showing either: a failure must not print a token.
        self.assertTrue(self.serve.token == self.token, 'the restored server has another token')
        self.assertEqual(self.serve.request('GET', '/v1/hosts', token=self.token)[0], 200)
        self.assertEqual(self.serve.request('GET', '/v1/hosts', token='not-the-token')[0], 401)
        wait_for('berth writing its key file', lambda: self.station.own_key.is_file())
        self.assertEqual(self.station.own_key.stat().st_mode & 0o777, 0o600)
        self.assertEqual(public_of(self.station.own_key), self.own_file_public)
        self.assertEqual(self.own_file_public, self.own_public.split()[:2])
        keys = {key['name']: key['public'].split()[:2] for key in self.serve.call('GET', '/v1/keys')['keys']}
        self.assertEqual(keys, self.keys)
        self.assertIn(self.ring_key, keys)
        self.assertEqual(keys['berth'], self.own_file_public)

    def test_4_the_secret_and_the_setting_are_there(self):
        self.assertIn(self.secret, self.serve.call('GET', '/v1/secrets')['names'])
        settings = {setting['key']: setting for setting in self.serve.call('GET', '/v1/settings')}
        self.assertEqual(settings['retention_days']['value'], '37')
        # The secret's value opens too: it reaches the host the way a server's secrets do.
        profile = self.serve.call('GET', f'/v1/hosts/{HOST}/profile')
        profile['secrets'] = [{'name': self.secret, 'env': 'RESTORED_SECRET'}]
        self.serve.call('PUT', f'/v1/hosts/{HOST}/profile', profile)
        session = self.serve.call('POST', f'/v1/hosts/{HOST}/actions/secrets', expect=202)['session']
        wait_for('the secrets task to end', lambda: self.serve.call(
            'GET', f'/v1/hosts/{HOST}/sessions/{session["id"]}').get('exit_code') == 0)
        env = self.worker.root_on_host(f'cat /home/{AGENT}/.config/devbox/secrets.env').stdout
        self.assertIn(f'export RESTORED_SECRET={self.secret_value}', env)

    def test_5_the_old_job_is_listed_and_its_output_is_somewhere_else(self):
        self.assertIn(self.job, [job['id'] for job in jobs_of(self.serve)])
        job = self.serve.call('GET', f'/v1/jobs/{self.job}')
        self.assertEqual((job['action'], job['exit_code']), ('status', self.finished['exit_code']))
        status, answer = self.serve.request('GET', f'/v1/jobs/{self.job}/output')
        self.assertEqual(status, 404, answer)
        self.assertIn(f'The output of job {self.job} was kept on the machine that ran it', answer['error']['message'])

    def test_6_without_the_key_file_berth_serve_refuses_and_writes_nothing(self):
        self.serve.stop()
        station = self.station
        wipe(station)
        database = self.keyless / 'berth.db'
        before = digest(database)
        absent = self.keyless / 'store.key'
        station.env.update(BERTH_STORE=str(database), BERTH_STORE_KEY=str(absent))
        done = station.berth('serve', '--listen', f'127.0.0.1:{free_port()}', check=False, timeout=60)
        said = done.stdout + done.stderr
        self.assertNotEqual(done.returncode, 0, said)
        self.assertIn('the store is encrypted', said)
        self.assertIn(str(absent), said)
        self.assertNotIn('Listening on', said)
        self.assertTrue(digest(database) == before, 'berth serve changed the database')
        # SQLite's own files beside the database are no change; a key file or a new database would be.
        self.assertEqual({path.name for path in self.keyless.iterdir()} - {'berth.db-wal', 'berth.db-shm'}, {'berth.db'})
        self.assertFalse((station.data / 'berth/serve.token').exists())
        self.assertFalse((station.data / 'berth/keys').exists())
