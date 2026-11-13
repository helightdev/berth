"""The profile a host keeps of itself, and another berth taking the host over
from it: the file the role writes, berthd serving it, berth adopt reading it
over SSH while berthd is broken, and the next applies of the new berth."""
import json
import unittest

from harness import HOST, BerthdLink, OPERATOR, Workstation, changed_tasks, needs_e2e, provisioned, shell_quote

KEPT = '/etc/berth/profile.json'
# How the berth that wrote it reaches the host, which stays out of the file.
REACH = {'ssh_host', 'ssh_user', 'ssh_port', 'ssh_key', 'link', 'served', 'served_only'}


def block(station, key=None):
    """The keys the role gave the operator."""
    text = station.ssh(OPERATOR, "sed -n '/BEGIN DEVBOX/,/END DEVBOX/p' ~/.ssh/authorized_keys", key=key).stdout
    return [line for line in text.splitlines() if line and not line.startswith('#')]


@needs_e2e
class Adopt(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.station = provisioned()
        # The host keeps what this workstation applied last, whatever came before.
        cls.station.berth('apply', HOST, timeout=900)
        cls.kept = json.loads(cls.station.root_on_host(f'cat {KEPT}').stdout)

    @classmethod
    def tearDownClass(cls):
        # The workstation's own key is the operator's again, for the modules after this one.
        cls.station.berth('apply', HOST, timeout=900)

    def test_the_host_keeps_its_profile_for_root_only(self):
        self.assertEqual(self.station.root_on_host(f'stat -c %a:%U:%G {KEPT}').stdout.strip(), '600:root:root')
        self.assertNotEqual(self.station.ssh(OPERATOR, f'cat {KEPT}', check=False).returncode, 0)
        self.assertEqual((self.kept['format'], self.kept['host']), (1, HOST))
        self.assertRegex(self.kept['written'], r'^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$')
        saved = self.station.read_profile()
        kept = self.kept['profile']
        self.assertEqual(kept['builder'], {key: value for key, value in saved['builder'].items() if key not in REACH})
        self.assertNotIn('tailscale_auth_key', kept)
        for key, value in saved.items():
            if key not in ('builder', 'tailscale_auth_key'):
                self.assertEqual(kept[key], value, key)

    def test_berthd_serves_it(self):
        # With the user's key, which the profile lists, whichever berth applied last.
        link = BerthdLink(self.station, key=self.station.key)
        self.assertEqual(link.get('/v1/applied/profile'), self.kept)

    def test_another_berth_adopts_the_host_while_berthd_is_broken(self):
        adopter = Workstation('adopter')
        own = adopter.berth('key').stdout.strip()
        old_own = self.station.own_key.with_suffix('.pub').read_text().strip()
        self.assertIn(old_own, block(self.station))
        # Its keys are not on the host yet, and it says the two ways in.
        refused = adopter.berth('adopt', HOST, f'{OPERATOR}@{HOST}', check=False)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn('berth key', refused.stderr)
        self.assertIn('id_ed25519', refused.stderr)
        self.assertFalse(adopter.profile_path().exists())
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
        profile = adopter.read_profile()
        self.assertEqual((profile['builder']['ssh_host'], profile['builder']['ssh_user']), (HOST, OPERATOR))
        self.assertEqual(adopter.profile_path().stat().st_mode & 0o777, 0o600)
        # Once there, it is not replaced without being asked.
        again = adopter.berth('adopt', HOST, f'{OPERATOR}@{HOST}', check=False)
        self.assertNotEqual(again.returncode, 0)
        self.assertIn('--force', again.stderr)
        adopter.berth('adopt', HOST, f'{OPERATOR}@{HOST}', '--force')

        # Its apply repairs berthd and makes the host this berth's.
        adopter.berth('apply', HOST, timeout=900)
        self.station.root_on_host('rm -f /usr/local/bin/berthd.away')
        adopter.link().get('/v1/version')
        keys = block(self.station)
        self.assertIn(own, keys)
        self.assertNotIn(old_own, keys)
        # The profile it applied is the one the host kept, so the file stays as it was.
        kept = json.loads(self.station.root_on_host(f'cat {KEPT}').stdout)
        self.assertEqual(kept, self.kept)
        # And applying again changes nothing.
        second = adopter.berth('apply', HOST, timeout=900)
        self.assertEqual(changed_tasks(second.stdout), [])


if __name__ == '__main__':
    unittest.main()
