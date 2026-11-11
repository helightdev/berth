#!/usr/bin/env python3
"""Launch disposable UEFI QEMU tests without attaching host block devices."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import uuid

def regular(path):
    p=Path(path).resolve()
    if not p.is_file(): raise ValueError(f'Expected a regular image/firmware file: {p}')
    return p

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('mode',choices=['install','cloud','boot'])
    p.add_argument('--directory',required=True)
    p.add_argument('--code',required=True,help='OVMF_CODE.fd file')
    p.add_argument('--vars',required=True,help='pristine matching OVMF_VARS.fd file')
    p.add_argument('--image',help='autoinstall ISO (install) or Ubuntu qcow2 (cloud)')
    p.add_argument('--user-data',help='generated cloud seed (cloud mode)')
    p.add_argument('--serial',default='DEVBOX_TEST_DISK',help='virtio disk serial, must match installer udev ID_SERIAL')
    p.add_argument('--port',type=int,default=2222)
    p.add_argument('--memory',type=int,default=8192)
    p.add_argument('--cpus',type=int,default=4)
    p.add_argument('--tcg',action='store_true',help='software emulation instead of KVM')
    p.add_argument('--headless',action='store_true',help='no window; guest serial console on stdio')
    args=p.parse_args()
    try:
        folder=Path(args.directory).resolve()
        if ',' in str(folder) or any(',' in str(v) for v in [args.code,args.vars,args.image or '']):
            raise ValueError('QEMU image paths cannot contain commas')
        code=regular(args.code); pristine=regular(args.vars)
        disk=folder/'disk.qcow2'; nvram=folder/'OVMF_VARS.fd'
        if args.mode!='boot':
            if folder.exists(): raise ValueError('Use a new test directory; existing disks are never overwritten')
            image=regular(args.image or '')
            folder.mkdir(parents=True,mode=0o700)
            shutil.copyfile(pristine,nvram)
            if args.mode=='cloud':
                subprocess.run(['qemu-img','convert','-O','qcow2',str(image),str(disk)],check=True)
                subprocess.run(['qemu-img','resize',str(disk),'80G'],check=True)
                seed=regular(args.user_data or '')
                (folder/'meta-data').write_text(json.dumps({'instance-id':str(uuid.uuid4()),'local-hostname':'agent-qemu'}))
                subprocess.run(['cloud-localds',str(folder/'seed.iso'),str(seed),str(folder/'meta-data')],check=True)
            else:
                subprocess.run(['qemu-img','create','-f','qcow2',str(disk),'80G'],check=True)
        regular(disk); regular(nvram)
        cmd=['qemu-system-x86_64','-machine','q35','-smp',str(args.cpus),'-m',str(args.memory)]
        cmd += ['-accel','tcg','-cpu','max'] if args.tcg else ['-enable-kvm','-cpu','host']
        cmd += ['-drive',f'if=pflash,format=raw,readonly=on,file={code}',
                '-drive',f'if=pflash,format=raw,file={nvram}',
                '-drive',f'if=none,id=disk0,format=qcow2,file={disk}',
                '-device',f'virtio-blk-pci,drive=disk0,serial={args.serial}',
                '-netdev',f'user,id=net0,hostfwd=tcp:127.0.0.1:{args.port}-:22',
                '-device','virtio-net-pci,netdev=net0']
        if args.mode=='install': cmd += ['-cdrom',str(image),'-boot','once=d','-no-reboot']
        if args.mode=='cloud': cmd += ['-drive',f'file={folder / "seed.iso"},format=raw,media=cdrom,readonly=on']
        if args.headless: cmd += ['-display','none','-serial','stdio']
        subprocess.run(cmd,check=True)
    except (ValueError,OSError,subprocess.SubprocessError) as exc:
        p.exit(1,f'{exc}\n')

if __name__=='__main__': main()
