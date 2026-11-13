"""The profile a host keeps of itself, and another berth taking the host over
from it: the file the role writes, berthd serving it, berth adopt reading it
over SSH while berthd is broken, and the next applies of the new berth.

It looks at the host as provisioned() applied it, and leaves it so: the new
berth applies the profile the host kept, and its key goes again at the end."""
import hashlib
import json
import unittest

from harness import HOST, BerthdLink, OPERATOR, Workstation, changed_tasks, needs_e2e, provisioned, shell_quote

KEPT = '/etc/berth/profile.json'
# What belongs to one berth's way to the host, which stays out of the file.
LOCAL = {'served', 'served_only', 'usb_device', 'manage_task'}


def marker(public):
    """What names a berth's block in authorized_keys: the SHA-1 of its key."""
    return f'BERTH {hashlib.sha1(public.split()[1].encode()).hexdigest()}'


def block(station, name):
    """The keys in one block the role wrote for the operator."""
    text = station.ssh(OPERATOR, f"sed -n '/^# BEGIN {name}$/,/^# END {name}$/p' ~/.ssh/authorized_keys").stdout
    return [line for line in text.splitlines() if line and not line.startswith('#')]


def removal(public):
    """The command the docs give for taking a berth's access away by hand."""
    name = marker(public)
    return f"sed -i '/^# BEGIN {name}$/,/^# END {name}$/d' ~/.ssh/authorized_keys"


@needs_e2e
class Adopt(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.station = provisioned()
        cls.kept = json.loads(cls.station.root_on_host(f'cat {KEPT}').stdout)

    def test_the_host_keeps_its_profile_for_root_only(self):
        self.assertEqual(self.station.root_on_host(f'stat -c %a:%U:%G {KEPT}').stdout.strip(), '600:root:root')
        self.assertNotEqual(self.station.ssh(OPERATOR, f'cat {KEPT}', check=False).returncode, 0)
        self.assertEqual((self.kept['format'], self.kept['host']), (2, HOST))
        self.assertRegex(self.kept['written'], r'^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$')
        saved = self.station.read_profile()
        kept = self.kept['profile']
        # How this berth logs in means nothing to another one.
        self.assertNotIn('access', kept)
        self.assertEqual(kept['builder'], {key: value for key, value in saved['builder'].items() if key not in LOCAL})
        self.assertNotIn('tailscale_auth_key', kept)
        for key, value in saved.items():
            if key not in ('access', 'builder', 'tailscale_auth_key'):
                self.assertEqual(kept[key], value, key)

    def test_berthd_serves_it(self):
        # With the user's key, which the profile lists, whichever berth applied last.
        link = BerthdLink(self.station, key=self.station.key)
        self.assertEqual(link.get('/v1/applied/profile'), self.kept)
        # The controller says the host is a workstation, as the file shows.
        self.assertEqual(link.get('/v1/version')['level'], 'workstation')

    def test_another_berth_adopts_the_host_while_berthd_is_broken(self):
        adopter = Workstation('adopter')
        own = adopter.own_public()
        old_own = self.station.own_public()
        self.assertEqual(block(self.station, marker(old_own)), [old_own])
        # Its keys are not on the host yet, and it says the ways in.
        refused = adopter.berth('adopt', HOST, f'{OPERATOR}@{HOST}', check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn('berth key show', refused.stderr)
        self.assertIn('--key', refused.stderr)
        self.assertNotEqual(adopter.berth('profile', HOST, check=False).returncode, 0)
        # Its public key put into authorized_keys by hand, as from the console.
        self.station.ssh(OPERATOR, f'echo {shell_quote(own)} >> ~/.ssh/authorized_keys')
        self.station.root_on_host('mv /usr/local/bin/berthd /usr/local/bin/berthd.away')
        try:
            adopted = adopter.berth('adopt', HOST, f'{OPERATOR}@{HOST}')
        except AssertionError:
            self.station.root_on_host('mv /usr/local/bin/berthd.away /usr/local/bin/berthd')
            raise
        self.assertIn(f'Adopted {HOST}', adopted.stdout)
        self.assertIn(f'berth apply {HOST}', adopted.stdout)
        self.assertIn('still has access', adopted.stdout)
        # It logs in as it got in: with its own key, and the operator's sudo took no password.
        profile = adopter.read_profile()
        self.assertEqual(profile['access'], {'address': HOST, 'user': OPERATOR, 'auth': 'berth'})
        # Once there, it is not replaced without being asked.
        again = adopter.berth('adopt', HOST, f'{OPERATOR}@{HOST}', check=False)
        self.assertNotEqual(again.returncode, 0)
        self.assertIn('--force', again.stderr)
        adopter.berth('adopt', HOST, f'{OPERATOR}@{HOST}', '--force')

        # Its apply repairs berthd and gives its key a block of its own.
        adopter.berth('apply', HOST, timeout=900)
        self.station.root_on_host('rm -f /usr/local/bin/berthd.away')
        adopter.link().get('/v1/version')
        self.assertEqual(block(self.station, marker(own)), [own])
        # The berth that set the host up keeps its access.
        self.assertEqual(block(self.station, marker(old_own)), [old_own])
        self.assertEqual(self.station.ssh(OPERATOR, 'id -un', key=self.station.own_key).stdout.strip(), OPERATOR)
        # The apply as the operator settled how it logs in.
        self.assertEqual(adopter.read_profile()['access']['sudo'], 'none')
        # The profile it applied is the one the host kept, so the file stays as it was.
        kept = json.loads(self.station.root_on_host(f'cat {KEPT}').stdout)
        self.assertEqual(kept, self.kept)
        # And applying again changes nothing.
        second = adopter.berth('apply', HOST, timeout=900)
        self.assertEqual(changed_tasks(second.stdout), [])

        # Taking a berth's access away is deleting its block by hand, here the
        # new one's with the line added for it, so the host is as it was.
        self.station.ssh(OPERATOR, removal(own))
        self.station.ssh(OPERATOR, f'grep -vxF {shell_quote(own)} ~/.ssh/authorized_keys > ~/.ssh/kept;'
                                   ' cat ~/.ssh/kept > ~/.ssh/authorized_keys; rm ~/.ssh/kept')
        self.assertEqual(block(self.station, marker(own)), [])
        self.assertNotEqual(self.station.ssh(OPERATOR, 'true', key=adopter.own_key, check=False).returncode, 0)
        self.assertEqual(block(self.station, marker(old_own)), [old_own])


if __name__ == '__main__':
    unittest.main()
