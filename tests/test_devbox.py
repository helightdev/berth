import base64
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[1]

def load(name,path):
    spec=importlib.util.spec_from_file_location(name,ROOT/path)
    module=importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
    return module

builder=load('builder','bin/devbox.py')
guard=load('guard','bin/check-target-disk.py')

def config():
    return {'hostname':'test-devbox','profile':'qemu',
        'operator_ssh_public_key':'ssh-ed25519 '+base64.b64encode(b'\0'*64).decode(),
        'operator_password_hash':'$6$salt$'+'a'*86,'target_disk_serial':'DEVBOX_TEST_DISK',
        'release_url':'https://example.com/devbox.tar.gz',
        'ansible_vars':{'devbox_bootstrap_ssh_cidr':'10.0.2.2/32'}}

class DiskGuardTests(unittest.TestCase):
    def test_exact_single_nonremovable_disk(self):
        disk={'id_serial':'ABC','type':'disk','path':'/dev/vda','rm':False,'tran':'virtio'}
        self.assertEqual(guard.select('ABC',[disk]),'/dev/vda')
        for override in [{'rm':True},{'tran':'usb'},{'mounted':True},{'read_only':True}]:
            with self.assertRaises(ValueError): guard.select('ABC',[{**disk,**override}])
        for candidates in [[],[disk,disk]]:
            with self.assertRaises(ValueError): guard.select('ABC',candidates)
    def test_rejects_globs_and_placeholders(self):
        for serial in ['', 'REPLACE_ME','*','disk?','a[b]','/dev/sda','disk;reboot']:
            with self.assertRaises(ValueError): guard.validate_serial(serial)

    def test_automatic_disk_selection_excludes_unsafe_targets(self):
        disk = {'type':'disk', 'path':'/dev/nvme0n1', 'size':512, 'rota':False}
        for unsafe in [{'rm':True}, {'tran':'usb'}, {'read_only':True}, {'mounted':True}]:
            self.assertEqual(guard.select_policy(guard.policy({'target_disk_mode':'auto'}),
                [disk, {**disk, **unsafe, 'path':'/dev/sdb'}]), disk['path'])
        with self.assertRaises(ValueError):
            guard.select_policy(guard.policy({'target_disk_mode':'auto'}), [disk, {**disk, 'path':'/dev/sdb'}])
        with self.assertRaises(ValueError):
            guard.select_policy(guard.policy({'target_disk_mode':'auto'}), [])

    def test_disk_model_path_type_and_size_policies(self):
        ssd = {'type':'disk', 'path':'/dev/nvme0n1', 'size':512, 'rota':False, 'model':'Samsung_SSD'}
        hdd = {'type':'disk', 'path':'/dev/sda', 'size':1024, 'rota':True, 'model':'WDC_HDD'}
        for options, expected in [
            ({'target_disk_mode':'model', 'target_disk_model':'Samsung*'}, ssd),
            ({'target_disk_mode':'path', 'target_disk_path':'/dev/nvme*'}, ssd),
            ({'target_disk_mode':'largest'}, hdd),
            ({'target_disk_mode':'smallest'}, ssd),
            ({'target_disk_mode':'ssd'}, ssd),
            ({'target_disk_mode':'hdd'}, hdd),
            ({'target_disk_mode':'largest', 'target_disk_ssd':'ssd'}, ssd),
        ]:
            self.assertEqual(guard.select_policy(guard.policy(options), [ssd,hdd]), expected['path'])
        with self.assertRaises(ValueError):
            guard.select_policy(guard.policy({'target_disk_mode':'largest'}), [ssd,{**hdd,'size':512}])
        with self.assertRaises(ValueError):
            guard.select_policy(guard.policy({'target_disk_mode':'largest'}), [ssd,{**hdd,'size':None}])

    def test_invalid_disk_policies_are_rejected(self):
        for options in [
            {'target_disk_mode':'model'}, {'target_disk_mode':'path'},
            {'target_disk_mode':'unknown'}, {'target_disk_mode':[]},
            {'target_disk_mode':'auto','target_disk_ssd':False},
        ]:
            with self.assertRaises(ValueError): guard.policy(options)

    def test_detected_path_is_pinned_in_autoinstall(self):
        with tempfile.TemporaryDirectory() as temp:
            source=Path(temp)/'source.json'; destination=Path(temp)/'autoinstall.yaml'
            source.write_text(json.dumps(builder.autoinstall(config())))
            guard.configure_install(source,destination,'/dev/nvme0n1')
            result=json.loads(destination.read_text())['autoinstall']
            self.assertEqual(result['storage']['layout']['match'],{'path':'/dev/nvme0n1'})
            self.assertEqual(result['identity']['hostname'],'test-devbox')
            self.assertEqual(destination.stat().st_mode & 0o777,0o600)

class RenderTests(unittest.TestCase):
    def test_placeholder_example_cannot_produce_installer(self):
        with self.assertRaises(ValueError): builder.checked_config(ROOT/'config/example.json','usb')
    def test_cloud_seed_carries_a_release_login_only_when_given(self):
        paths=lambda seed:[entry['path'] for entry in seed['write_files']]
        self.assertNotIn('/opt/devbox-bootstrap/release-login',paths(builder.cloud_config(config(),'a'*64)))
        with patch.dict(os.environ,{'BERTH_RELEASE_LOGIN':'user = "reader:secret"'}):
            seed=builder.cloud_config(config(),'a'*64)
        entry=[entry for entry in seed['write_files'] if entry['path'].endswith('release-login')][0]
        self.assertEqual((entry['content'],entry['permissions']),('user = "reader:secret"\n','0600'))
        with patch.dict(os.environ,{'BERTH_RELEASE_LOGIN':'url = "https://evil.example"'}):
            self.assertRaises(ValueError,builder.cloud_config,config(),'a'*64)
    def test_cloud_seed_has_no_disk_actions(self):
        c=config(); result=builder.cloud_config(c,'a'*64)
        self.assertNotIn('autoinstall',result)
        self.assertNotIn('storage',result)
        self.assertIn('--no-block',result['runcmd'][2])
        self.assertLess(len(json.dumps(result)),32768)
        d=json.loads(result['write_files'][1]['content'])
        self.assertEqual(d['release_sha256'],'a'*64)
    def test_usb_guard_runs_before_serial_matched_storage(self):
        result=builder.autoinstall(config())['autoinstall']
        self.assertEqual(result['storage']['layout']['match'],{'serial':'DEVBOX_TEST_DISK'})
        self.assertIn('check-target-disk.py',result['early-commands'][0][1])
        self.assertEqual(result['shutdown'],'poweroff')
    def test_usb_auto_policy_renders_without_serial(self):
        c=config(); c.pop('target_disk_serial'); c['target_disk_mode']='auto'
        with tempfile.TemporaryDirectory() as temp, patch.object(builder,'BUILD',Path(temp)/'build'):
            path=Path(temp)/'host.json'; path.write_text(json.dumps(c))
            result=builder.render(path,'usb')
            payload=result/'agent-bootstrap'
            selection=json.loads((payload/'disk-policy.json').read_text())
            self.assertEqual(selection,{'target_disk_mode':'auto'})
            source=json.loads((payload/'autoinstall.json').read_text())['autoinstall']
            self.assertEqual(source['storage']['layout']['match'],{'path':'/dev/DEVBOX_SELECTION_PENDING'})
            self.assertIn('--autoinstall',source['early-commands'][0])

    def test_default_auto_and_disabled_wifi(self):
        self.assertEqual(guard.policy({})['mode'], 'auto')
        c=config(); c.pop('target_disk_serial')
        c['wifi_enabled']=False
        c['wifi']={'interface':'wlan0','ssid':'saved','password':'saved'}
        result=builder.autoinstall(c)['autoinstall']
        self.assertNotIn('network', result)
        self.assertEqual(result['storage']['layout']['match'], {'path':'/dev/DEVBOX_SELECTION_PENDING'})

    def test_custom_accounts_reach_installer_cloud_and_provisioning(self):
        c=config()
        c.update(operator_username='admin',agent_username='robot',
                 agent_password_hash='$6$salt$hash',agent_passwordless_sudo=False)
        self.assertEqual(builder.autoinstall(c)['autoinstall']['identity']['username'],'admin')
        cloud=builder.cloud_config(c,'a'*64)
        self.assertEqual(cloud['users'][1]['name'],'admin')
        deployment=builder.deployment(c,'a'*64)
        self.assertEqual(deployment['ansible_vars']['devbox_user'],'robot')
        self.assertFalse(deployment['ansible_vars']['devbox_agent_passwordless_sudo'])
        self.assertEqual(deployment['ansible_vars']['devbox_operator_user'],'admin')

    def test_wifi_special_characters_are_encoded_not_executed(self):
        c=config(); c['wifi']={'interface':'wlan0','ssid':'a "quote"','password':'$HOME;`example`'}
        text=json.dumps(builder.autoinstall(c))
        network=json.loads(text)['autoinstall']['network']
        value=network['wifis']['wlan0']['access-points']
        self.assertEqual(value[c['wifi']['ssid']]['password'],c['wifi']['password'])
        self.assertTrue(network['ethernets']['wired']['optional'])
    def test_wifi_interface_is_detected_on_the_target(self):
        c=config(); c['wifi']={'ssid':'home','password':'secret'}
        with tempfile.TemporaryDirectory() as temp:
            path=Path(temp)/'host.json'; path.write_text(json.dumps(c))
            builder.checked_config(path,'usb')
            source=Path(temp)/'source.json'; source.write_text(json.dumps(builder.autoinstall(c)))
            for interfaces,expected in [(['wlp2s0','wlx0'],'wlp2s0'),([],None)]:
                destination=Path(temp)/'autoinstall.yaml'
                guard.configure_install(source,destination,'/dev/vda',interfaces)
                network=json.loads(destination.read_text())['autoinstall']['network']
                self.assertIn('wired',network['ethernets'])
                self.assertEqual(list(network.get('wifis',{})),[expected] if expected else [])
        with tempfile.TemporaryDirectory() as temp:
            (Path(temp)/'wlan0/wireless').mkdir(parents=True); (Path(temp)/'eth0').mkdir()
            self.assertEqual(guard.wireless_interfaces(temp),['wlan0'])
    def test_iso_checksum_is_checked_when_given(self):
        with tempfile.TemporaryDirectory() as temp, patch.object(builder,'BUILD',Path(temp)/'build'), \
                patch.object(builder.shutil,'which',return_value='/usr/bin/xorriso'):
            path=Path(temp)/'host.json'; path.write_text(json.dumps(config()))
            iso=Path(temp)/'source.iso'; iso.write_bytes(b'not an iso')
            with self.assertRaisesRegex(ValueError,'does not match'): builder.build_iso(path,iso,'0'*64)
            with self.assertRaisesRegex(ValueError,'64 lowercase'): builder.build_iso(path,iso,'abc')
    def test_cloud_recovery_network_is_optional_but_checked(self):
        with tempfile.TemporaryDirectory() as temp:
            path=Path(temp)/'host.json'
            c=config(); c['ansible_vars']={}; path.write_text(json.dumps(c))
            builder.checked_config(path,'cloud')
            c['ansible_vars']={'devbox_bootstrap_ssh_cidr':'not a network'}; path.write_text(json.dumps(c))
            with self.assertRaises(ValueError): builder.checked_config(path,'cloud')
    def test_malformed_profiles_are_rejected_cleanly(self):
        for change in [{'hostname':None},{'hostname':'trailing-'},{'profile':'container'},
                       {'agent_ssh_public_key':'not a key'},{'operator_username':'root'},
                       {'agent_username':'ops'},{'operator_username':'operator'},{'agent_username':'admin'},{'agent_passwordless_sudo':'no'},
                       {'ansible_vars':{'other':1}},{'target_disk_mode':'serial','target_disk_serial':''}]:
            with self.subTest(change=change), tempfile.TemporaryDirectory() as temp:
                path=Path(temp)/'host.json'; path.write_text(json.dumps({**config(),**change}))
                with self.assertRaises(ValueError): builder.checked_config(path,'usb')
    def test_agent_privileges_are_checked_and_passed_on(self):
        for value,passed in [(None,''),('full','full'),('limited','limited'),('none','none')]:
            with self.subTest(value=value), tempfile.TemporaryDirectory() as temp:
                profile={**config(),'agent_passwordless_sudo':False}
                if value: profile['agent_privileges']=value
                else: profile['agent_password_hash']='$6$salt$'+'a'*86
                path=Path(temp)/'host.json'; path.write_text(json.dumps(profile))
                checked=builder.checked_config(path,'usb')
                self.assertEqual(builder.deployment(checked,'0'*64)['ansible_vars']['devbox_agent_privileges'],passed)
        with tempfile.TemporaryDirectory() as temp:
            path=Path(temp)/'host.json'; path.write_text(json.dumps({**config(),'agent_privileges':'some'}))
            with self.assertRaises(ValueError): builder.checked_config(path,'usb')
    def test_seed_embeds_the_current_release(self):
        with tempfile.TemporaryDirectory() as temp, patch.object(builder,'BUILD',Path(temp)/'build'):
            path=Path(temp)/'host.json'; path.write_text(json.dumps(config()))
            stale=Path(temp)/'build/devbox-release.tar.gz'; stale.parent.mkdir(); stale.write_bytes(b'stale')
            result=builder.render(path,'usb')
            deployment=json.loads((result/'agent-bootstrap/deployment.json').read_text())
            self.assertEqual(deployment['release_sha256'],builder.digest(result/'agent-bootstrap/release.tar.gz'))
            self.assertEqual(deployment['release_sha256'],builder.digest(stale))
    def test_installed_bootstrap_copy_is_root_only(self):
        commands=builder.autoinstall(config())['autoinstall']['late-commands']
        self.assertIn('chown -R root:root /target/opt/devbox-bootstrap',commands)
        self.assertLess(commands.index('chmod -R go-rwx /target/opt/devbox-bootstrap'),len(commands)-1)
    def test_grub_requires_installer_and_quotes_semicolon(self):
        text='set timeout=30\nmenuentry "Install" {\n linux /casper/vmlinuz quiet ---\n}\n'
        patched=builder.patch_grub(text)
        self.assertIn('"ds=nocloud;s=/cdrom/nocloud/"',patched)
        self.assertEqual(patched.count('set timeout='),1)
        self.assertTrue(patched.startswith('set default=0\nset timeout=5\n'))
        with self.assertRaises(ValueError): builder.patch_grub('not an Ubuntu ISO')
    def test_release_is_deterministic_and_excludes_local_secrets(self):
        with tempfile.TemporaryDirectory() as temp, patch.object(builder,'BUILD',Path(temp)):
            first=builder.digest(builder.release()); second=builder.digest(builder.release())
            self.assertEqual(first,second)
            import tarfile
            with tarfile.open(Path(temp)/'devbox-release.tar.gz') as archive:
                names=archive.getnames()
            self.assertIn('bootstrap/bootstrap.sh',names)
            self.assertFalse(any('secrets/' in x or 'config/local.json' in x for x in names))
    def test_rendered_seeds_roundtrip_and_are_private(self):
        with tempfile.TemporaryDirectory() as temp, patch.object(builder,'BUILD',Path(temp)/'build'):
            path=Path(temp)/'host.json'; path.write_text(json.dumps(config()))
            result=builder.render(path,'usb')
            seed=result/'nocloud/user-data'
            self.assertEqual(seed.stat().st_mode & 0o777,0o600)
            self.assertIn('autoinstall',json.loads(seed.read_text().split('\n',1)[1]))
            self.assertTrue((result/'agent-bootstrap/release.tar.gz').exists())

class HostTaskTests(unittest.TestCase):
    """The scripts berthd runs for berth's own tasks."""
    TASKS=ROOT/'ansible/roles/devbox/files/tasks'
    def test_task_scripts_parse_and_share_the_helpers(self):
        scripts=sorted(self.TASKS.glob('*.sh'))
        self.assertEqual({p.stem for p in scripts},{'common','status','access','publish','pair','update','reboot'})
        for script in scripts+[ROOT/'plugins/backup/files/backup.sh']:
            with self.subTest(script=script.name):
                subprocess.run(['bash','-n',str(script)],check=True)
        for name in ['status','access','publish','pair']:
            self.assertIn('common.sh',(self.TASKS/f'{name}.sh').read_text())
    def test_the_old_ways_are_gone(self):
        self.assertFalse((ROOT/'bin/manage-host.sh').exists())
        self.assertFalse((ROOT/'ansible/roles/devbox/files/desktop.py').exists())
    def test_host_programs_travel_in_the_release_when_built(self):
        built=ROOT/'ansible/roles/devbox/files/bin/berthd'
        if not built.exists(): self.skipTest('run `cargo build -p berth` first')
        with tempfile.TemporaryDirectory() as temp, patch.object(builder,'BUILD',Path(temp)):
            import tarfile
            with tarfile.open(builder.release()) as archive: names=archive.getnames()
        for name in ['berthd','box']:
            self.assertIn(f'ansible/roles/devbox/files/bin/{name}',names)
        self.assertIn('ansible/roles/devbox/files/tasks/status.sh',names)
        self.assertIn('plugins/backup/files/backup.sh',names)

if __name__=='__main__': unittest.main()
