"""What a booted install shows and a container cannot: berthd and a plugin's
service as systemd units, units and the journal through berthd, the firewall
and SSH as the role sets them, applying again, and a roll-out that reboots the
host. Run by tests/e2e/boot.py after the first boot."""
import json
import os
import socket
import subprocess
import unittest

import vm
from harness import AGENT, HOST, OPERATOR, ansible_recap, changed_tasks, provisioned, random_text, wait_for

SERVICE = 'berth-svc-e2e-echo.service'


def needs_boot(cls):
    reason = 'the boot tests run after a boot in QEMU: ci/run.sh boot'
    return unittest.skipUnless(os.environ.get('BERTH_BOOT'), reason)(cls)


def kind():
    return os.environ.get('BERTH_BOOT')


def show(station, unit, *properties):
    """systemctl show's PROPERTIES of a unit, as a dict."""
    done = station.root_on_host(f'systemctl show {unit} ' + ' '.join(f'-p {name}' for name in properties))
    return dict(line.split('=', 1) for line in done.stdout.splitlines() if '=' in line)


def probe(port, timeout=8):
    """What a request to a forwarded port of the guest gets back: QEMU takes
    the connection either way and closes it when the guest does not answer."""
    with socket.create_connection(('127.0.0.1', port), timeout=timeout) as sock:
        sock.sendall(b'GET / HTTP/1.0\r\n\r\n')
        try:
            return sock.recv(64)
        except (TimeoutError, ConnectionError):
            return b''


@needs_boot
class FirstBoot(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.station = provisioned()
        cls.link = cls.station.link()

    def root(self, command):
        return self.station.root_on_host(command).stdout

    def test_the_first_boot_applied_the_release_it_was_given(self):
        self.assertEqual(self.root('cat /var/lib/devbox/provision-state').strip(), 'configured-needs-enrollment')
        deployment = json.loads(self.root('cat /opt/devbox-bootstrap/deployment.json'))
        self.assertEqual(self.root('cat /var/lib/devbox/ready').strip(), deployment['release_sha256'])
        self.assertEqual(self.root('readlink /opt/devbox/current').strip(),
                         f'/opt/devbox/releases/{deployment["release_sha256"]}')
        self.assertEqual(show(self.station, 'devbox-provision.service', 'Result')['Result'], 'success')
        # The enrolment key is not kept once it was used.
        self.assertNotIn('devbox_tailscale_auth_key', json.dumps(deployment))

    @unittest.skipUnless(kind() == 'cloud', 'cloud-init sets up the cloud image only')
    def test_cloud_init_finished_without_errors(self):
        status = json.loads(self.station.root_on_host('cloud-init status --format json', check=False).stdout)
        self.assertEqual(status['status'], 'done', status)
        self.assertEqual(status.get('errors'), [], status)

    def test_berthd_is_a_unit_that_starts_at_boot(self):
        unit = show(self.station, 'berthd.service', 'ActiveState', 'UnitFileState', 'MainPID')
        self.assertEqual((unit['ActiveState'], unit['UnitFileState']), ('active', 'enabled'))
        self.assertEqual(self.link.get('/v1/version')['mode'], 'system')
        processes = self.link.get('/v1/processes')
        main = [process for process in processes if str(process['pid']) == unit['MainPID']]
        self.assertTrue(main and 'berthd' in main[0]['command'], (unit, main))

    def test_the_plugin_service_is_a_unit_that_answers(self):
        unit = show(self.station, SERVICE, 'ActiveState', 'UnitFileState', 'User')
        self.assertEqual((unit['ActiveState'], unit['UnitFileState'], unit['User']), ('active', 'enabled', AGENT))
        answer = wait_for('the service answering', lambda: (done := self.station.berth(
            'call', HOST, 'e2e/echo', 'say', '{"text": "hi"}', check=False)).returncode == 0 and done, timeout=60)
        self.assertEqual(json.loads(answer.stdout), {'method': 'say', 'body': {'text': 'hi'}, 'level': 'host'})

    def test_ssh_takes_keys_only_and_no_root(self):
        settings = dict(line.split(' ', 1) for line in self.root('sshd -T').splitlines() if ' ' in line)
        self.assertEqual((settings['passwordauthentication'], settings['kbdinteractiveauthentication'],
                          settings['permitrootlogin']), ('no', 'no', 'no'))
        # The server offers nothing but keys.
        password = subprocess.run(
            ['ssh', '-o', 'BatchMode=yes', '-o', 'PubkeyAuthentication=no', '-o', 'ConnectTimeout=10',
             '-o', 'PreferredAuthentications=password,keyboard-interactive',
             '-o', f'UserKnownHostsFile={self.station.home}/.ssh/known_hosts', f'{OPERATOR}@{HOST}', 'true'],
            env=self.station.env, capture_output=True, text=True, timeout=60)
        self.assertNotEqual(password.returncode, 0)
        self.assertIn('Permission denied (publickey)', password.stderr)
        # Not even a key gets root in.
        self.root(f'install -d -m 0700 /root/.ssh && cat /home/{OPERATOR}/.ssh/authorized_keys >> /root/.ssh/authorized_keys')
        try:
            root = self.station.ssh('root', 'id -un', check=False)
            self.assertNotEqual(root.returncode, 0, root.stdout)
        finally:
            self.root(f'grep -vxFf /home/{OPERATOR}/.ssh/authorized_keys /root/.ssh/authorized_keys'
                      ' > /root/.ssh/keys.new || true; mv /root/.ssh/keys.new /root/.ssh/authorized_keys')

    def test_the_firewall_lets_in_ssh_and_nothing_else(self):
        status = self.root('ufw status verbose')
        self.assertIn('Status: active', status)
        self.assertIn('Default: deny (incoming)', status)
        served = self.link.post('/v1/sessions', {
            'kind': 'process', 'title': 'firewall probe',
            'command': ['python3', '-m', 'http.server', str(vm.PROBED_PORT), '--bind', '0.0.0.0']})
        try:
            wait_for('the probe listening in the guest', lambda: '200' in self.station.root_on_host(
                f'curl -s -o /dev/null -w %{{http_code}} http://127.0.0.1:{vm.PROBED_PORT}/', check=False).stdout)
            self.assertFalse(probe(vm.PROBE_PORT).startswith(b'HTTP/'))
            # The same request, let through, is answered: what kept it out was the firewall.
            self.root(f'ufw allow {vm.PROBED_PORT}/tcp')
            try:
                self.assertTrue(wait_for('the opened port answering',
                                         lambda: probe(vm.PROBE_PORT).startswith(b'HTTP/'), timeout=30))
            finally:
                self.root(f'ufw delete allow {vm.PROBED_PORT}/tcp')
        finally:
            self.link.post(f'/v1/sessions/{served["id"]}/signal', {'signal': 'TERM'})

    def test_the_status_task_ends_well(self):
        done = self.station.berth('run', HOST, 'status', check=False, timeout=180)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        for line in (f'Host        {HOST}', 'Provision   configured-needs-enrollment', 'ssh         active',
                     'berthd      active'):
            self.assertIn(line, done.stdout)
        self.assertNotRegex(done.stdout, r'(?m)^Failed', done.stdout)


@needs_boot
class Units(unittest.TestCase):
    """Units and their journal through berthd."""

    @classmethod
    def setUpClass(cls):
        cls.station = provisioned()
        cls.link = cls.station.link()

    def state(self, unit):
        return self.link.get(f'/v1/units/{unit}')

    def test_units_are_listed_with_their_state(self):
        units = {unit['unit']: unit for unit in self.link.get('/v1/units')}
        for name in ('berthd.service', 'ssh.service', SERVICE):
            self.assertEqual(units[name]['active'], 'active', units.get(name))

    def test_a_service_is_stopped_started_and_restarted(self):
        before = self.state(SERVICE)['MainPID']
        self.link.post(f'/v1/units/{SERVICE}/stop')
        self.assertEqual(self.state(SERVICE)['ActiveState'], 'inactive')
        refused = self.station.berth('call', HOST, 'e2e/echo', 'say', '{}', check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.link.post(f'/v1/units/{SERVICE}/start')
        self.assertEqual(self.state(SERVICE)['ActiveState'], 'active')
        started = self.state(SERVICE)['MainPID']
        self.link.post(f'/v1/units/{SERVICE}/restart')
        restarted = self.state(SERVICE)['MainPID']
        self.assertEqual(len({before, started, restarted}), 3, (before, started, restarted))
        wait_for('the service answering again', lambda: self.station.berth(
            'call', HOST, 'e2e/echo', 'say', '{}', check=False).returncode == 0, timeout=60)
        status, answer = self.link.request('POST', f'/v1/units/{SERVICE}/mask', {})
        self.assertEqual(status, 400, answer)

    def test_the_journal_of_a_unit(self):
        mark = random_text('journal')
        self.station.root_on_host(f'systemd-run --wait --quiet --unit=berth-boot-mark echo {mark}')
        # sudo logs the command with the mark too, from the SSH session.
        units = wait_for('the line in the journal', lambda: [
            entry['unit'] for entry in self.link.get('/v1/logs?limit=200') if mark in entry['message']])
        self.assertIn('berth-boot-mark.service', units)
        only = self.link.get('/v1/logs?unit=berth-boot-mark.service&limit=20')
        self.assertIn(mark, [entry['message'] for entry in only])
        self.link.post(f'/v1/units/{SERVICE}/restart')
        messages = [entry['message'] for entry in self.link.get(f'/v1/logs?unit={SERVICE}&limit=50')]
        self.assertTrue(any('Started' in message and 'e2e/echo' in message for message in messages), messages)


@needs_boot
class Reapply(unittest.TestCase):
    def test_applying_again_changes_nothing(self):
        station = provisioned()
        first = station.berth('apply', HOST, timeout=1800)
        self.assertEqual(ansible_recap(first.stdout)['failed'], 0)
        print(f'\n  first apply over SSH changed: {changed_tasks(first.stdout)}', end=' ')
        again = station.berth('apply', HOST, timeout=1800)
        self.assertEqual(ansible_recap(again.stdout)['failed'], 0)
        self.assertEqual(changed_tasks(again.stdout), [])


@needs_boot
class Reboot(unittest.TestCase):
    """berth apply --all --reboot on a host that needs it."""

    @classmethod
    def setUpClass(cls):
        cls.station = provisioned()
        cls.link = cls.station.link()

    def test_a_reboot_waits_for_running_sessions(self):
        station = self.station
        station.root_on_host('touch /var/run/reboot-required')
        sleeper = self.link.post('/v1/sessions', {'kind': 'process', 'title': 'busy', 'command': ['sleep', '900']})
        boot = station.root_on_host('cat /proc/sys/kernel/random/boot_id').stdout
        try:
            refused = station.berth('apply', '--all', '--reboot', check=False, timeout=1800)
            self.assertEqual(refused.returncode, 1, refused.stdout)
            self.assertRegex(refused.stdout, rf'(?m)^{HOST}: failed: needs a reboot, but 1 session is running there')
            self.assertEqual(station.root_on_host('cat /proc/sys/kernel/random/boot_id').stdout, boot)
        finally:
            self.link.post(f'/v1/sessions/{sleeper["id"]}/signal', {'signal': 'KILL'})

    def test_a_rollout_reboots_the_host_and_checks_it(self):
        station = self.station
        station.root_on_host('touch /var/run/reboot-required')
        boot = station.root_on_host('cat /proc/sys/kernel/random/boot_id').stdout
        done = station.berth('apply', '--all', '--reboot', check=False, timeout=2400)
        self.assertEqual(done.returncode, 0, done.stdout[-3000:] + done.stderr[-2000:])
        self.assertIn(f'▸ {HOST}: rebooting', done.stdout)
        self.assertIn(f'✓ {HOST} is up to date', done.stdout)
        self.assertNotEqual(station.root_on_host('cat /proc/sys/kernel/random/boot_id').stdout, boot)
        self.assertEqual(station.root_on_host('test -e /var/run/reboot-required', check=False).returncode, 1)
        for unit in ('berthd.service', SERVICE):
            self.assertEqual(show(station, unit, 'ActiveState')['ActiveState'], 'active', unit)


# Enrolment in a tailnet on first boot, against a control server of the tests'
# own. boot.py gives the profile the key and switches Tailscale on when
# BERTH_CI_BOOT_CONTROL_URL and BERTH_CI_BOOT_CONTROL_KEY are set.
@needs_boot
class Enrolment(unittest.TestCase):
    @unittest.skipUnless(os.environ.get('BERTH_BOOT_CONTROL_URL') and os.environ.get('BERTH_BOOT_CONTROL_KEY'),
                         'needs a tailnet control server: BERTH_CI_BOOT_CONTROL_URL and BERTH_CI_BOOT_CONTROL_KEY')
    def test_the_host_enrolled_on_first_boot(self):
        station = provisioned()
        status = json.loads(station.root_on_host('tailscale status --json').stdout)
        self.assertEqual(status['BackendState'], 'Running')
        self.assertEqual(status.get('CurrentTailnet', {}).get('Name') is not None, True)
        deployment = station.root_on_host('cat /opt/devbox-bootstrap/deployment.json').stdout
        self.assertNotIn(os.environ['BERTH_BOOT_CONTROL_KEY'], deployment)
