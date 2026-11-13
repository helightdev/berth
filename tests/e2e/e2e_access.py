"""How berth logs in to a host, against the bare test host with nothing but
sshd and the operator account: first contact with a one-time password,
berth's own key, a key added to the key ring, a password through berth's
askpass helper, a changed host key refused until it is accepted, and a
rotation of berth's key. No profile is applied.

It runs on `dock` before e2e_own_key, which wants that host as a provider
hands it over: at the end the operator's password, its authorized_keys, the
provided key and the host's SSH host keys are as they were."""
import subprocess
import tempfile
import unittest
from pathlib import Path

from harness import (BERTH, HOST, OPERATOR, Workstation, hand_over_operator_key, keygen, needs_e2e, profile,
                     random_text, shell_quote, wait_for)

# What sshd reads before the test host's own setting, which turns passwords off.
PASSWORD_CONF = '/etc/ssh/sshd_config.d/05-e2e-password.conf'
AUTHORIZED = f'/home/{OPERATOR}/.ssh/authorized_keys'
BACKUP = '/root/e2e-access'
# The listening sshd, which reads its configuration and host keys again on HUP.
RELOAD_SSHD = 'kill -HUP "$(pgrep -o -x sshd)"'


def scanned():
    """The host key the test host shows now, as ssh-keyscan reads it."""
    return subprocess.run(['ssh-keyscan', '-T', '2', '-t', 'ed25519', HOST], capture_output=True, text=True,
                          check=False).stdout.strip()


def fingerprint(line):
    """The SHA256 fingerprint of a known_hosts line."""
    with tempfile.NamedTemporaryFile('w', suffix='.keys') as file:
        file.write(line + '\n')
        file.flush()
        said = subprocess.run(['ssh-keygen', '-l', '-f', file.name], capture_output=True, text=True, check=True)
    return said.stdout.split()[1]


@needs_e2e
class Access(unittest.TestCase):
    """The tests build on each other, in the order of their names."""

    @classmethod
    def setUpClass(cls):
        # The provider's console: a key handed over at the start, root through sudo.
        cls.admin = Workstation('access-admin')
        hand_over_operator_key(cls.admin.public)
        wait_for('the operator taking the provided key',
                 lambda: cls.admin.ssh(OPERATOR, 'true', check=False).returncode == 0, timeout=60)
        cls.original_host_key = scanned()
        cls.password = random_text('pw')
        cls.root(f'mkdir -p {BACKUP} && cp -a /etc/ssh/ssh_host_* {BACKUP}/ '
                 f'&& if [ -e {AUTHORIZED} ]; then cp -a {AUTHORIZED} {BACKUP}/authorized_keys; fi')
        # The password goes to chpasswd on its standard input.
        cls.admin.ssh(OPERATOR, 'sudo -n chpasswd', input=f'{OPERATOR}:{cls.password}\n')
        # These tests fail logins on purpose, and sshd would shut their address out for it.
        cls.root(f"printf 'PasswordAuthentication yes\\nKbdInteractiveAuthentication yes\\n"
                 f"PerSourcePenalties no\\n' > {PASSWORD_CONF} "
                 f'&& {RELOAD_SSHD}')
        wait_for('sshd answering again', lambda: cls.admin.ssh(OPERATOR, 'true', check=False).returncode == 0)
        cls.station = Workstation('access', ssh_key=False, trust=False)
        cls.station.write_profile(profile(access={'address': HOST, 'user': OPERATOR, 'auth': 'berth'}))

    @classmethod
    def tearDownClass(cls):
        restore = (f'rm -f {PASSWORD_CONF} {AUTHORIZED} && usermod --password "*" {OPERATOR} '
                   f'&& if [ -e {BACKUP}/authorized_keys ]; then cp -a {BACKUP}/authorized_keys {AUTHORIZED}; fi '
                   f'&& rm -f /etc/ssh/ssh_host_* && cp -a {BACKUP}/ssh_host_* /etc/ssh/ && rm -rf {BACKUP} '
                   f'&& {RELOAD_SSHD}')
        # The admin's ssh checks the host key the host has now.
        cls.admin.trust_host()
        cls.root(restore)
        wait_for('the host showing its own host key again', lambda: scanned() == cls.original_host_key)
        cls.admin.trust_host()
        # The provider's key goes too; e2e_own_key hands over its own.
        cls.root(f'rm -f /etc/ssh/provided-keys/{OPERATOR}', check=False)
        hand_over_operator_key()

    @classmethod
    def root(cls, command, check=True):
        return cls.admin.ssh(OPERATOR, f'sudo -n sh -c {shell_quote(command)}', check=check)

    def berth(self, *args, **kwargs):
        kwargs.setdefault('input', '')
        kwargs.setdefault('timeout', 120)
        return self.station.berth(*args, **kwargs)

    def logs_in(self):
        """What `berth ssh` does with the profile as it is: the account it
        reached, or its failure. It runs without a terminal, so ssh can ask
        nobody for a password."""
        return subprocess.run([str(BERTH), 'ssh', HOST, 'id -un'], env=self.station.env, cwd=self.station.home,
                              stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=60,
                              start_new_session=True)

    def set_auth(self, auth):
        kept = self.station.read_profile()
        kept['access']['auth'] = auth
        self.station.write_profile(kept)

    def authorized(self):
        return self.root(f'cat {AUTHORIZED}').stdout

    def test_1_first_contact_installs_berths_key_with_a_password_used_once(self):
        refused = self.logs_in()
        self.assertNotEqual(refused.returncode, 0, refused.stdout)
        own = self.station.own_public()
        done = self.berth('key', 'trust', HOST, input=self.password + '\n')
        self.assertIn(f'{HOST} takes the key berth now', done.stdout)
        self.assertIn(own, self.authorized())
        self.assertEqual(self.logs_in().stdout.strip(), OPERATOR)
        # The host key it met is recorded, and the password is not kept.
        self.assertIn(fingerprint(self.original_host_key), self.berth('key', 'host', HOST).stdout)
        self.assertNotIn(f'ssh/{HOST}', self.berth('secret').stdout)
        self.assertEqual(self.station.read_profile()['access']['auth'], 'berth')
        # Once it takes the key, nothing more is asked or added.
        again = self.berth('key', 'trust', HOST)
        self.assertIn('already', again.stdout)
        self.assertEqual(self.authorized().count(own.split()[1]), 1)

    def test_2_a_key_of_the_ring_logs_in_alone(self):
        file = self.station.root / 'deploy'
        public = keygen(file, 'deploy@access')
        self.admin.ssh(OPERATOR, 'cat >> ~/.ssh/authorized_keys', input=public + '\n')
        added = self.berth('key', 'add', 'deploy', str(file))
        self.assertIn('Added the key deploy', added.stdout)
        self.assertNotIn('PRIVATE KEY', added.stdout + self.berth('key').stdout)
        self.set_auth('key:deploy')
        try:
            self.assertEqual(self.logs_in().stdout.strip(), OPERATOR)
            options = self.berth('inventory', HOST).stdout
            self.assertIn('keys/deploy', options)
            self.assertNotIn('keys/berth', options)
            refused = self.berth('key', 'remove', 'deploy', check=False)
            self.assertNotEqual(refused.returncode, 0)
            self.assertIn(HOST, refused.stderr)
        finally:
            self.set_auth('berth')
        self.berth('key', 'remove', 'deploy')

    def test_3_a_password_goes_through_berths_askpass_helper(self):
        self.set_auth('password')
        try:
            # Without a stored password nobody can type one: it fails, and asks no program.
            without = self.logs_in()
            self.assertNotEqual(without.returncode, 0)
            self.berth('secret', 'set', f'ssh/{HOST}', input=self.password + '\n')
            self.assertEqual(self.logs_in().stdout.strip(), OPERATOR)
            self.assertNotIn(self.password, self.berth('inventory', HOST).stdout)
        finally:
            self.berth('secret', 'remove', f'ssh/{HOST}', check=False)
            self.set_auth('berth')

    def test_4_a_changed_host_key_is_refused_until_it_is_accepted(self):
        recorded = self.berth('key', 'host', HOST).stdout
        self.root(f'rm -f /etc/ssh/ssh_host_* && ssh-keygen -A && {RELOAD_SSHD}')
        new = wait_for('the host showing a new host key',
                       lambda: (now := scanned()) and now != self.original_host_key and now)
        self.admin.trust_host()
        refused = self.logs_in()
        self.assertNotEqual(refused.returncode, 0)
        self.assertRegex(refused.stderr, r'HOST IDENTIFICATION HAS CHANGED|Host key verification failed')
        # berth's own logins say what happened and what to do.
        said = self.berth('tasks', HOST, check=False)
        self.assertNotEqual(said.returncode, 0)
        self.assertIn(f'berth key host {HOST} --accept', said.stderr)
        self.assertEqual(self.berth('key', 'host', HOST).stdout, recorded)
        accepted = self.berth('key', 'host', HOST, '--accept')
        self.assertIn(fingerprint(new), accepted.stdout)
        self.assertEqual(self.logs_in().stdout.strip(), OPERATOR)

    def test_5_a_rotation_replaces_berths_key_on_the_host(self):
        before = self.station.own_public()
        rotated = self.berth('key', 'rotate', timeout=300)
        self.assertIn(f'{HOST} takes the new key', rotated.stdout)
        after = self.station.own_public()
        self.assertNotEqual(after, before)
        keys = self.authorized()
        self.assertIn(after.split()[1], keys)
        self.assertNotIn(before.split()[1], keys)
        # The other lines stay: the deploy key the provider added.
        self.assertIn('deploy@access', keys)
        self.assertNotIn('being replaced', self.berth('key').stdout)
        self.assertEqual(self.logs_in().stdout.strip(), OPERATOR)
        self.assertEqual(Path(f'{self.station.own_key}.pub').read_text().strip(), after)
