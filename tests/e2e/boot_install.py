"""The unattended installer of berth iso: its disk guard refused a disk with
another serial, erased the one it was given and left the other alone, and the
installed system boots without the installer. boot.py ran the installer and
noted what it saw from outside the guest; these check that and the guest."""
import json
import os
import unittest

import vm
from harness import HOST, provisioned

TARGET, DECOY = 'berth-boot-target', 'berth-boot-decoy'


@unittest.skipUnless(os.environ.get('BERTH_BOOT') == 'iso', 'after the installer ran: ci/run.sh boot iso')
class Installer(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.station = provisioned()
        cls.facts = vm.CURRENT.facts

    def disks(self):
        listed = json.loads(self.station.root_on_host('lsblk --json -o NAME,SERIAL,TYPE,MOUNTPOINTS').stdout)
        return {disk.get('serial'): disk for disk in listed['blockdevices'] if disk['type'] == 'disk'}

    def test_the_guard_refuses_a_disk_with_another_serial(self):
        self.assertTrue(self.facts['refusal_seen'], self.facts)
        self.assertFalse(self.facts['refusal_powered_off'], 'the installer went on and powered off')
        self.assertEqual(self.facts['refusal_decoy_written'], 0)

    def test_the_named_disk_was_erased_and_installed(self):
        self.assertEqual(self.facts['install_exit'], 0)
        disks = self.disks()
        mounts = [point for part in disks[TARGET].get('children', []) for point in part['mountpoints'] if point]
        self.assertIn('/', mounts)
        self.assertIn('/boot/efi', mounts)
        # Nothing of the old contents is left at the start of the disk.
        start = self.station.root_on_host('dd if=/dev/disk/by-id/virtio-berth-boot-target bs=1M count=1 status=none'
                                          ' | od -An -tx1 -N4096 | grep -c "a5 a5 a5 a5 a5 a5 a5 a5"',
                                          check=False).stdout.strip()
        self.assertEqual(start, '0')

    def test_the_other_disk_was_left_alone(self):
        self.assertEqual(self.facts['install_decoy_written'], 0)
        self.assertFalse(self.disks()[DECOY].get('children'))
        self.assertEqual(vm.written(vm.CURRENT.machine.decoy), 0)

    def test_the_installed_system_boots_without_the_installer(self):
        self.assertIsNone(self.facts['installed_boot_cdrom'])
        types = self.station.root_on_host('lsblk -dn -o TYPE').stdout.split()
        self.assertNotIn('rom', types)
        cmdline = self.station.root_on_host('cat /proc/cmdline').stdout
        self.assertNotIn('autoinstall', cmdline)
        self.assertEqual(self.station.root_on_host('hostname').stdout.strip(), HOST)
