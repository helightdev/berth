#!/usr/bin/env python3
"""Boot a real install in QEMU, set it up on first boot, and test it with
berth over SSH.

    ci/run.sh boot cloud     the Ubuntu cloud image with the seed of berth seed
    ci/run.sh boot iso       the installer of berth iso, on a blank virtual disk
    ci/run.sh boot cloud --keep -- boot_host.FirstBoot

Only meant for the container of ci/boot.compose.yml. Logs, the timings and the
disk and memory used are in build/boot/KIND.
"""
import argparse
import json
import os
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import harness  # noqa: E402
import vm  # noqa: E402
from harness import AGENT, HOST, OPERATOR, SRC, Workstation, keygen  # noqa: E402

FIXTURE = SRC / 'tests/e2e/fixtures/plugin'
# The virtual disks' serials, which the installer sees as ID_SERIAL.
TARGET, DECOY = 'berth-boot-target', 'berth-boot-decoy'
# The disk guard's answer when no disk has the serial it was given.
REFUSED = 'Expected exactly one matching disk, got 0'
# Programs of the default package set that nothing here uses, left out to keep
# the first boot short. The desktop itself stays.
EXTRAS = {
    'libreoffice-writer', 'libreoffice-calc', 'libreoffice-impress', 'libreoffice-draw', 'gimp', 'inkscape',
    'evince', 'tesseract-ocr', 'tesseract-ocr-eng', 'tesseract-ocr-deu', 'ocrmypdf', 'pandoc',
    'fonts-noto-cjk',
}

# The tests that run after the boot, in order. Those of the container suite
# that apply the light profile of tests/e2e/site.yml, or that expect no
# systemd, are left out.
SUITE = {
    'cloud': ['boot_host.FirstBoot', 'e2e_host.Commands', 'e2e_host.Berthd', 'e2e_secrets', 'boot_host.Units',
              'boot_host.Reapply', 'boot_host.Reboot', 'boot_host.Enrolment'],
}
SUITE['iso'] = ['boot_install', *SUITE['cloud']]
LEFT_OUT = {'test_units_without_systemd_are_an_answer_not_a_crash'}


def control_server():
    """A tailnet control server to enrol the guest with on its first boot, as
    (URL, key), when one is given."""
    url, key = os.environ.get('BERTH_BOOT_CONTROL_URL'), os.environ.get('BERTH_BOOT_CONTROL_KEY')
    return (url, key) if url and key else None


def boot_vars():
    """The role as someone would set up a quick host: the desktop and berth's
    own parts, with the plugin of the tests, and without coding agents,
    toolchains, Playwright, Docker, Cockpit or office and graphics programs."""
    import yaml  # noqa: PLC0415 (from the venv with ansible)
    defaults = yaml.safe_load((SRC / 'provision/ansible/roles/devbox/defaults/main.yml').read_text())
    variables = {
        'devbox_plugin_features': ['e2e/hello'],
        'devbox_install_dev_tools': False,
        'devbox_install_playwright': False,
        'devbox_install_docker': False,
        'devbox_install_tailscale': False,
        'devbox_install_cockpit': False,
        'devbox_packages': [name for name in defaults['devbox_packages'] if name not in EXTRAS],
    }
    if control_server():
        # Enrolment on first boot. The role's option for a control server of
        # one's own is not there yet; its name goes here when it is.
        variables['devbox_install_tailscale'] = True
    return variables


def password_hash():
    password = os.urandom(12).hex()
    return vm.run('openssl', 'passwd', '-6', '-stdin', input=password, capture_output=True, text=True).stdout.strip()


def boot_profile(station, kind, **values):
    profile = json.loads((SRC / 'provision/config/example.json').read_text())
    profile.update({
        'hostname': HOST, 'profile': 'cloud', 'keyboard': 'us',
        # As berth writes a new host: the shipped plugins' features are chosen
        # in the list, not switched on for a profile from before them.
        'builder': {'shipped_plugins': True},
        # The user's key logs in first, as for a host they handed their key to.
        'access': {'address': HOST, 'user': OPERATOR, 'auth': 'ssh'},
        'operator_ssh_public_keys': [station.public],
        'agent_ssh_public_keys': [keygen(station.root / 'agent_key', 'agent@workstation')],
        'operator_password_hash': password_hash(),
        'ansible_vars': boot_vars(),
    })
    if kind != 'cloud':
        # The installed machine is reached only from this container, as QEMU's host.
        profile['ansible_vars']['devbox_bootstrap_ssh_cidr'] = f'{vm.GATEWAY}/32'
    if control_server():
        profile['tailscale_auth_key'] = control_server()[1]
    profile.update(values)
    return profile


def ssh_bytes(station, command, timeout=300):
    argv = ['ssh', '-o', 'BatchMode=yes', '-o', 'IdentitiesOnly=yes', '-o', 'ConnectTimeout=10',
            '-o', f'UserKnownHostsFile={station.home}/.ssh/known_hosts', '-i', str(station.key),
            f'{OPERATOR}@{HOST}', f'sudo -n sh -c {harness.shell_quote(command)}']
    return subprocess.run(argv, env=station.env, capture_output=True, timeout=timeout)


# ---------------------------------------------------------------------------
# Waiting for the guest


def wait_for_ssh(station, machine, timeout):
    def answers():
        if not machine.running():
            raise SystemExit(f'{machine.name} ended before it answered on SSH')
        return subprocess.run(['ssh-keyscan', '-T', '3', '-t', 'ed25519', HOST], capture_output=True,
                              text=True).stdout.strip()
    harness.wait_for('the guest answering on SSH', answers, timeout=timeout, every=5)
    station.trust_host()
    harness.wait_for('the operator logging in', lambda: station.ssh(OPERATOR, 'true', check=False).returncode == 0,
                     timeout=600, every=5)


def wait_for_provisioning(station, machine, timeout):
    """Until the first-boot service has applied the role: its ready marker and
    a successful run. A failed run ends the wait at once."""
    probe = ('cat /var/lib/devbox/provision-state 2>/dev/null || echo none; '
             'test -f /var/lib/devbox/ready && echo ready || echo not-ready; '
             'systemctl show -p ActiveState --value devbox-provision.service; '
             'journalctl -u devbox-provision.service -n 1 -o cat --no-pager | cut -c1-160')
    end = time.monotonic() + timeout
    shown = None
    while time.monotonic() < end:
        if not machine.running():
            raise AssertionError(f'{machine.name} ended while it was being set up')
        done = station.ssh(OPERATOR, f'sudo -n sh -c {harness.shell_quote(probe)}', check=False, timeout=60)
        lines = done.stdout.splitlines() if done.returncode == 0 else []
        if len(lines) >= 3:
            state, ready, active = lines[:3]
            if state == 'failed':
                tail = station.ssh(OPERATOR, 'sudo -n journalctl -u devbox-provision.service -n 40 -o cat --no-pager',
                                   check=False).stdout
                raise AssertionError(f'The first-boot provisioning failed. The end of its journal, all of which'
                                     f' is in provision.log:\n{tail}')
            if ready == 'ready' and state == 'configured-needs-enrollment' and active != 'activating':
                return
            line = lines[3] if len(lines) > 3 else ''
            if (state, line) != shown:
                vm.say(f'  {state}: {line}')
                shown = (state, line)
        time.sleep(15)
    raise AssertionError(f'The first boot did not finish within {timeout / 60:.0f} minutes')


# ---------------------------------------------------------------------------
# The two paths


def boot_cloud(run, side, station):
    with run.phase('fetch the cloud image'):
        image = vm.cloud_image()
    with run.phase('render the seed'):
        station.berth('plugins', 'add', str(FIXTURE))
        station.write_profile(boot_profile(station, 'cloud', release_url=f'https://{vm.GATEWAY}/devbox-release.tar.gz'))
        station.berth('seed', HOST, timeout=300)
        build = station.data / 'berth/build'
        side.publish(build / 'devbox-release.tar.gz')
        user_data = (build / HOST / 'cloud/user-data').read_text()
        shutil.copyfile(build / HOST / 'cloud/user-data', run.logs / 'user-data')
    machine = vm.Machine(HOST, run)
    # The provider's part: the instance's name, and trust in the test side's
    # certificate authority, which the release server's certificate is from.
    vendor = '#cloud-config\n' + json.dumps({'ca_certs': {'trusted': [side.ca]}}, indent=2) + '\n'
    seed = vm.seed_image(machine.dir / 'seed.iso', {
        'user-data': user_data, 'meta-data': vm.meta_data(HOST), 'vendor-data': vendor})
    machine.add_disk(vm.disk(machine.dir / 'disk.qcow2', '40G', backing=image), 'berth-boot-cloud')
    machine.cdrom = seed
    with run.phase('first boot until SSH'):
        machine.start('boot')
        wait_for_ssh(station, machine, timeout=900)
    with run.phase('first-boot provisioning'):
        wait_for_provisioning(station, machine, timeout=5400)
    return machine


def installer_kernel(iso, folder, console):
    """The installer's kernel, initrd and command line as its patched GRUB
    menu has them, with the console added, to boot it with the console on the
    serial port."""
    for name in ('casper/vmlinuz', 'casper/initrd', 'boot/grub/grub.cfg'):
        vm.run('xorriso', '-osirrox', 'on', '-indev', iso, '-extract', f'/{name}', folder / Path(name).name,
               capture_output=True)
    for line in (folder / 'grub.cfg').read_text().splitlines():
        words = shlex.split(line)
        if words[:2] == ['linux', '/casper/vmlinuz']:
            words = words[2:]
            cut = words.index('---') if '---' in words else len(words)
            return folder / 'vmlinuz', folder / 'initrd', ' '.join(words[:cut] + [console] + words[cut:])
    raise AssertionError('No installer entry in the patched GRUB menu')


def install_iso(run, side, station):
    with run.phase('fetch the installer ISO'):
        iso, sha = vm.server_iso()
    with run.phase('build the installer'):
        station.berth('plugins', 'add', str(FIXTURE))
        station.write_profile(boot_profile(station, 'iso', target_disk_mode='serial', target_disk_serial=TARGET,
                                           builder={'shipped_plugins': True, 'iso': str(iso),
                                                    'iso_sha256': sha}))
        station.berth('iso', HOST, timeout=1800)
        built = station.data / 'berth/build' / f'{HOST}-autoinstall.iso'
        run.facts['installer_gb'] = round(built.stat().st_size / 2**30, 2)

    # A machine whose only disk has another serial: the installer must stop
    # before it partitions anything.
    with run.phase('the installer refuses a disk with another serial'):
        refuse = vm.Machine('refuse', run)
        decoy = vm.filled_disk(refuse.dir / 'decoy.qcow2', '30G', '0x5a')
        refuse.add_disk(decoy, DECOY)
        refuse.cdrom = built
        refuse.no_reboot = True
        refuse.kernel = installer_kernel(built, refuse.dir, 'console=ttyS0,115200')
        refuse.start('refuse')
        def refused():
            if not refuse.running():
                raise SystemExit('the installer powered off instead of refusing')
            return REFUSED in refuse.serial()
        try:
            harness.wait_for('the disk guard refusing', refused, timeout=1800, every=5)
            # Long enough for an installer that went on regardless to start partitioning.
            time.sleep(60)
            refuse.screendump('refuse-screen')
            run.facts['refusal_powered_off'] = not refuse.running()
        finally:
            refuse.kill()
        run.facts['refusal_seen'] = REFUSED in refuse.serial()
        run.facts['refusal_decoy_written'] = vm.written(decoy)

    # The real install: the disk it may erase second, after one it must not touch.
    machine = vm.Machine(HOST, run)
    decoy = vm.filled_disk(machine.dir / 'decoy.qcow2', '30G', '0x5a')
    target = vm.filled_disk(machine.dir / 'target.qcow2', '40G', '0xa5')
    machine.add_disk(decoy, DECOY)
    machine.add_disk(target, TARGET)
    machine.cdrom = built
    machine.no_reboot = True
    with run.phase('unattended install'):
        machine.start('install')
        shots = iter(range(10**6))
        code = machine.wait(timeout=3600, every=lambda: machine.screendump(f'install-screen-{next(shots) % 3}'))
        run.facts['install_exit'] = code
        run.facts['install_decoy_written'] = vm.written(decoy)
        run.facts['install_target_written_gb'] = round(vm.written(target) / 2**30, 2)
        if code != 0:
            raise AssertionError(f'QEMU ended with {code} during the install')
    machine.cdrom = None
    machine.no_reboot = False
    with run.phase('installed system boots until SSH'):
        machine.start('installed')
        run.facts['installed_boot_cdrom'] = machine.cdrom
        wait_for_ssh(station, machine, timeout=900)
    with run.phase('first-boot provisioning'):
        wait_for_provisioning(station, machine, timeout=5400)
    machine.decoy = decoy
    return machine


# ---------------------------------------------------------------------------
# Logs, the tests, and keeping the guest up


GUEST_LOGS = {
    'journal.log': 'journalctl -b --no-pager -o short-iso',
    'journal-boots.log': 'journalctl --list-boots --no-pager',
    'provision.log': 'journalctl -u devbox-provision.service --no-pager -o short-iso',
    'berthd.log': 'journalctl -u berthd.service --no-pager -o short-iso',
    'cloud-init.log': 'cat /var/log/cloud-init.log',
    'cloud-init-output.log': 'cat /var/log/cloud-init-output.log',
    'cloud-init-status.json': 'cloud-init status --format json',
    'failed-units.txt': 'systemctl --failed --no-pager',
    'ufw.txt': 'ufw status verbose',
    'sshd.txt': 'sshd -T',
    'disks.txt': 'lsblk -o NAME,SERIAL,SIZE,TYPE,FSTYPE,MOUNTPOINTS',
    'apt-history.log': 'cat /var/log/apt/history.log',
    'apt-term.log': 'cat /var/log/apt/term.log',
    'apt-sources.txt': 'cat /etc/apt/sources.list.d/*.sources /etc/apt/sources.list 2>/dev/null',
    'installer.tar.gz': 'tar -C /var/log -czf - installer',
}


def collect(run, station):
    for name, command in GUEST_LOGS.items():
        try:
            done = ssh_bytes(station, command)
        except (subprocess.TimeoutExpired, OSError):
            continue
        if done.stdout:
            (run.logs / name).write_bytes(done.stdout)


def suite(kind, names):
    loader = unittest.TestLoader()
    tests = unittest.TestSuite()
    for name in names or SUITE[kind]:
        found = loader.loadTestsFromName(name)
        tests.addTests(test for test in iter_tests(found) if test._testMethodName not in LEFT_OUT)
    return tests


def iter_tests(found):
    for test in found:
        if isinstance(test, unittest.TestSuite):
            yield from iter_tests(test)
        else:
            yield test


def keep(run, station, machine):
    folder = run.dir / 'keep'
    folder.mkdir(exist_ok=True)
    key = folder / 'id_ed25519'
    shutil.copyfile(station.key, key)
    owner = os.environ.get('BERTH_BOOT_OWNER', '')
    if ':' in owner:
        uid, gid = (int(part) for part in owner.split(':'))
        for path in (folder, key):
            os.chown(path, uid, gid)
    key.chmod(0o600)
    port = os.environ.get('BERTH_BOOT_SSH_PORT', '22722')
    shown = key.relative_to(vm.WORK)
    print(f'''
The guest keeps running until Ctrl-C. From this machine:

  ssh -i build/boot/{shown} -p {port} -o UserKnownHostsFile=/dev/null {OPERATOR}@127.0.0.1
  ssh -i build/boot/{shown} -p {port} -o UserKnownHostsFile=/dev/null {AGENT}@127.0.0.1  (with the agent key: {shown.parent}/agent_key)

Its serial console:

  docker exec -it {socket.gethostname()} socat -,raw,echo=0 UNIX-CONNECT:{machine.dir}/serial.sock
''', flush=True)
    shutil.copyfile(station.root / 'agent_key', folder / 'agent_key')
    if ':' in owner:
        os.chown(folder / 'agent_key', uid, gid)
    (folder / 'agent_key').chmod(0o600)
    signals = {signal.SIGINT, signal.SIGTERM}
    signal.pthread_sigmask(signal.SIG_BLOCK, signals)
    signal.sigwait(signals)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('kind', choices=['cloud', 'iso'])
    parser.add_argument('--keep', action='store_true', help='keep the guest running after the tests')
    parser.add_argument('tests', nargs='*', help='test names as unittest takes them')
    args = parser.parse_args()
    if os.environ.get('BERTH_E2E') != '1' or not vm.WORK.is_dir():
        sys.exit('The boot tests run in their container: ci/run.sh boot')
    if not harness.BERTH.is_file():
        sys.exit(f'No {harness.BERTH}: build berth first')
    os.environ['BERTH_BOOT'] = args.kind
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    run = vm.Run(args.kind)
    vm.CURRENT = run
    side = station = machine = None
    outcome, extra = 'failed', {}
    try:
        side = vm.TestSide(run)
        station = Workstation('workstation', source_dir=SRC, trust=False)
        machine = (boot_cloud if args.kind == 'cloud' else install_iso)(run, side, station)
        harness.adopt(station)
        collect(run, station)
        with run.phase('tests'):
            result = unittest.TextTestRunner(verbosity=2).run(suite(args.kind, args.tests))
        outcome = 'passed' if result.wasSuccessful() else 'tests failed'
    except BaseException as error:  # noqa: BLE001 (any end of the run is reported and cleaned up)
        print(f'The {args.kind} boot did not complete: {error!r}', file=sys.stderr, flush=True)
        if isinstance(error, KeyboardInterrupt):
            outcome = 'interrupted'
    finally:
        if station and run.machine and run.machine.running() and station.root.joinpath('home/.ssh/known_hosts').exists():
            collect(run, station)
            if args.keep:
                keep(run, station, run.machine)
        if run.machine:
            run.machine.stop()
            run.machine.kill()
        if side:
            extra = side.cache_stats()
            side.stop()
        harness.cleanup()
        run.report(outcome, extra)
        # The guest's disks are only of use while it runs.
        shutil.rmtree(run.vm, ignore_errors=True)
        if outcome != 'passed':
            print(f'Logs of the {args.kind} boot are in build/boot/{args.kind}/logs', file=sys.stderr)
    sys.exit(0 if outcome == 'passed' else 1)


if __name__ == '__main__':
    main()
