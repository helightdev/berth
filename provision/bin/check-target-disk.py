#!/usr/bin/env python3
"""Runs in the installer before partitioning: select one eligible target disk, pin
autoinstall storage to its detected path, and resolve an automatic Wi-Fi interface."""
import argparse
import fnmatch
import json
import os
from pathlib import Path
import re
import subprocess
import sys

def validate_serial(serial):
    if not serial or serial.startswith('REPLACE') or not re.fullmatch(r'[A-Za-z0-9_.:+-]+', serial):
        raise ValueError('An exact non-placeholder disk ID_SERIAL is required; no globs or paths')

def select(serial, disks):
    validate_serial(serial)
    matches = [d for d in disks if d.get('id_serial') == serial and d.get('type') == 'disk']
    if len(matches) != 1:
        raise ValueError(f'Expected exactly one matching disk, got {len(matches)}')
    disk = matches[0]
    if disk.get('rm') or disk.get('tran') == 'usb' or disk.get('read_only'):
        raise ValueError('Refusing removable, USB, or read-only target')
    if disk.get('mounted'):
        raise ValueError('Refusing a target with mounted filesystems')
    return disk['path']

MODES = {'serial', 'auto', 'model', 'path', 'largest', 'smallest', 'ssd', 'hdd'}

def policy(config):
    """Validated selection policy. Profiles that only name a serial keep serial mode."""
    serial = config.get('target_disk_serial') or ''
    legacy = isinstance(serial, str) and serial and not serial.startswith('REPLACE')
    result = {
        'mode': config.get('target_disk_mode', 'serial' if legacy else 'auto'),
        'serial': serial,
        'model': config.get('target_disk_model', ''),
        'path': config.get('target_disk_path', ''),
        'ssd': config.get('target_disk_ssd', 'any'),
    }
    if any(not isinstance(value, str) for value in result.values()):
        raise ValueError('Disk selection values must be strings')
    if result['mode'] not in MODES:
        raise ValueError('Unknown disk selection mode')
    if result['ssd'] not in {'any', 'ssd', 'hdd'}:
        raise ValueError('Disk type filter must be any, ssd or hdd')
    if result['mode'] == 'serial':
        validate_serial(result['serial'])
    if result['mode'] == 'model' and not result['model'].strip():
        raise ValueError('Model selection needs a model pattern')
    if result['mode'] == 'path' and not result['path'].startswith('/dev/'):
        raise ValueError('Path selection needs a /dev/ path or pattern')
    return result

def select_policy(selection, disks):
    mode = selection['mode']
    if mode == 'serial':
        return select(selection['serial'], disks)
    candidates = [
        disk for disk in disks
        if disk.get('type') == 'disk'
        and not (disk.get('rm') or disk.get('tran') == 'usb'
                 or disk.get('read_only') or disk.get('mounted'))
    ]
    if mode == 'model':
        candidates = [d for d in candidates if fnmatch.fnmatchcase(d.get('model', ''), selection['model'])]
    if mode == 'path':
        pattern = selection['path']
        if not any(ch in pattern for ch in '*?['):
            pattern = os.path.realpath(pattern)
        candidates = [d for d in candidates if fnmatch.fnmatchcase(d['path'], pattern)]
    kind = mode if mode in {'ssd', 'hdd'} else selection['ssd']
    if kind != 'any':
        candidates = [d for d in candidates if isinstance(d.get('rota'), (bool, int))
                      and bool(d['rota']) == (kind == 'hdd')]
    if mode in {'largest', 'smallest'} and candidates:
        if any(not isinstance(d.get('size'), int) or d['size'] <= 0 for d in candidates):
            raise ValueError('Cannot select by size with missing disk capacity')
        size = (max if mode == 'largest' else min)(d['size'] for d in candidates)
        candidates = [d for d in candidates if d['size'] == size]
    if len(candidates) != 1:
        raise ValueError(f'Expected exactly one eligible disk, got {len(candidates)}')
    return candidates[0]['path']

WIFI_AUTO = 'DEVBOX_WIFI_AUTO'

def wireless_interfaces(sysfs='/sys/class/net'):
    root = Path(sysfs)
    if not root.is_dir():
        return []
    return sorted(p.name for p in root.iterdir()
                  if (p / 'wireless').exists() or (p / 'phy80211').exists())

def resolve_wifi(network, interfaces):
    """Replace the automatic Wi-Fi placeholder with the machine's wireless interface.

    Netplan cannot match Wi-Fi devices by pattern, so the name is filled in here.
    Without a wireless interface the section is dropped and wired DHCP remains."""
    wifis = network.get('wifis') or {}
    if WIFI_AUTO not in wifis:
        return None
    settings = wifis.pop(WIFI_AUTO)
    if interfaces:
        wifis[interfaces[0]] = settings
        return interfaces[0]
    del network['wifis']
    return ''

def configure_install(source, destination, selected, interfaces=None):
    config = json.loads(source.read_text())
    config['autoinstall']['storage']['layout']['match'] = {'path': selected}
    network = config['autoinstall'].get('network')
    if network:
        found = resolve_wifi(network, wireless_interfaces() if interfaces is None else interfaces)
        if found is not None:
            print(f'Wi-Fi interface: {found or "none found, continuing with wired networking"}', file=sys.stderr)
    # JSON is valid YAML. Subiquity re-reads this file after early commands.
    destination.write_text(json.dumps(config, indent=2) + '\n')
    destination.chmod(0o600)

def inventory():
    result = subprocess.run(['lsblk', '--json', '--bytes', '--paths', '-o', 'PATH,TYPE,RM,RO,TRAN,MOUNTPOINTS,SIZE,ROTA,MODEL'], check=True, capture_output=True, text=True)
    rows = json.loads(result.stdout)['blockdevices']
    def mounted(row):
        return any(row.get('mountpoints') or []) or any(mounted(c) for c in row.get('children', []))
    disks = []
    for row in rows:
        if row['type'] != 'disk':
            continue
        info = subprocess.run(['udevadm', 'info', '--query=property', '--name', row['path']], check=True, capture_output=True, text=True)
        props = dict(line.split('=',1) for line in info.stdout.splitlines() if '=' in line)
        disks.append({**row, 'id_serial': props.get('ID_SERIAL'), 'read_only': row['ro'], 'mounted': mounted(row), 'model': props.get('ID_MODEL', row.get('model') or '')})
    return disks

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument('--serial')
    group.add_argument('--policy')
    parser.add_argument('--source')
    parser.add_argument('--autoinstall')
    args = parser.parse_args()
    try:
        subprocess.run(['udevadm', 'settle'], check=True)
        selection = policy(json.loads(Path(args.policy).read_text())) if args.policy else policy({'target_disk_serial': args.serial})
        selected = select_policy(selection, inventory())
        if bool(args.source) != bool(args.autoinstall):
            raise ValueError('Both source and autoinstall paths are required for rewriting')
        if args.source:
            configure_install(Path(args.source), Path(args.autoinstall), selected)
        print(selected)
    except (ValueError, OSError, subprocess.SubprocessError) as exc:
        print(str(exc), file=sys.stderr)
        sys.exit(1)
