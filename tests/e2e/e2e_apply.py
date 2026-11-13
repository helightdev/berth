"""Setting the test host up: a profile applied over SSH with the role, the
SSH keys each account takes, and applying again without changes. The second
apply is the one of the extended state (harness.extended()), which has the
role's plugin and tailnet tasks to repeat as well."""
import json
import unittest

from harness import (AGENT, HOST, OPERATOR, ansible_recap, changed_tasks, extended, fresh_runtime_dir, needs_e2e,
                     provisioned)


@needs_e2e
class Apply(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.station = provisioned()

    def test_the_first_apply_installs_berthd(self):
        recap = ansible_recap(self.station.first_apply)
        self.assertEqual((recap['failed'], recap['unreachable']), (0, 0))
        self.assertGreater(recap['changed'], 10)
        version = self.station.link().get('/v1/version')
        self.assertEqual(version['mode'], 'system')
        berthd = json.loads(self.station.root_on_host('cat /etc/berth/berthd.json').stdout)
        self.assertEqual((berthd['agent'], berthd['operator']), (AGENT, OPERATOR))
        tasks = json.loads(self.station.root_on_host('cat /etc/berth/tasks.json').stdout)
        self.assertIn('secrets', [task['name'] for task in tasks])
        print(f'\n  first apply: {self.station.apply_seconds:.0f}s, {recap}', end=' ')

    def test_the_operator_takes_berths_own_key(self):
        own = self.station.berth('key').stdout.strip()
        self.assertTrue(own.startswith('ssh-ed25519 '), own)
        self.assertEqual(own, self.station.own_key.with_suffix('.pub').read_text().strip())
        self.assertEqual(self.station.own_key.stat().st_mode & 0o777, 0o600)
        keys = self.station.ssh(OPERATOR, 'cat ~/.ssh/authorized_keys').stdout
        self.assertIn(own, keys)
        # With nothing of the user's: no key in ~/.ssh and no shared connection.
        moved = self.station.root / 'moved-key'
        self.station.key.rename(moved)
        try:
            fresh = {'XDG_RUNTIME_DIR': fresh_runtime_dir()}
            done = self.station.berth('exec', HOST, '--', 'id', '-un', env=fresh)
        finally:
            moved.rename(self.station.key)
        self.assertEqual(done.stdout.strip(), AGENT)

    def test_every_listed_key_logs_in_to_its_account(self):
        root = self.station.root
        for key in (self.station.key, root / 'second_key', self.station.own_key):
            self.assertEqual(self.station.ssh(OPERATOR, 'id -un', key=key).stdout.strip(), OPERATOR, key)
        for key in (root / 'agent_one', root / 'agent_two'):
            self.assertEqual(self.station.ssh(AGENT, 'id -un', key=key).stdout.strip(), AGENT, key)
        # The agent takes only its own keys.
        refused = self.station.ssh(AGENT, 'true', key=self.station.key, check=False)
        self.assertNotEqual(refused.returncode, 0)


@needs_e2e
class Again(unittest.TestCase):
    """The second apply of the extended state, with nothing changed in between."""

    @classmethod
    def setUpClass(cls):
        cls.applied = extended()

    def test_applying_again_changes_nothing(self):
        again = self.applied.second
        recap = ansible_recap(again.stdout)
        self.assertEqual(recap['failed'], 0)
        self.assertEqual(changed_tasks(again.stdout), [])
