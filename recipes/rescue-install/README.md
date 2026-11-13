# Ubuntu from a rescue system

This recipe installs Ubuntu on a server from its provider's rescue system. It
is for a server whose provider has no working installer for the Ubuntu
release you want. The result is a plain server with SSH, which the
[`incus-host`](../incus-host/) recipe can then set up.

It was run for Ubuntu 26.04, from a rescue system that is Debian or Ubuntu.

## What you get

| Part | Set-up |
| --- | --- |
| Disks | A RAID 1 over both disks with one ext4 file system. One disk works too, without RAID |
| Boot | UEFI, with a boot partition on each disk. The server starts from either one, and with Secure Boot on or off |
| System | Ubuntu's own server image, with the generic kernel |
| Network | The card with the default route gets its address by DHCP. A fixed IPv6 address is optional |
| Login | SSH as root with your key. Passwords are refused |

## Run it

Everything on the disks is lost. Copy what you need from the server first.

1. Start the server in its rescue system from your provider's panel. Log in
   as root.
2. Find the disks with `lsblk`.
3. Fetch the script and run it:

```bash
curl -fsSL https://github.com/helightdev/berth/archive/refs/heads/main.tar.gz \
  | tar xz --strip-components=1 --wildcards '*/recipes'
cd recipes/rescue-install
./install.sh --disks /dev/nvme0n1,/dev/nvme1n1 --key ~/.ssh/authorized_keys --hostname big
```

The script lists the disks. Type the host name to confirm, and it wipes them.
The install takes a few minutes. Then set the server to start from its disk
in the panel, restart it and log in:

```bash
ssh root@192.0.2.10
```

## Options

| Option | Meaning |
| --- | --- |
| `--disks A,B` | The disks to install on. With one disk there is no RAID |
| `--key FILE` | Public keys that may log in as root. A rescue system often has yours in `~/.ssh/authorized_keys` |
| `--hostname NAME` | The server's name |
| `--release NAME` | The Ubuntu release by code name. The default is `resolute`, which is 26.04 |
| `--mac ADDRESS` | The network card to set up. The default is the one with the default route |
| `--ipv6 ADDRESS/N` | A fixed IPv6 address. It needs `--gateway6 ADDRESS` |
| `--yes` | Do not ask before the disks are wiped |

## Good to know

- The script refuses a system that did not start with UEFI, and a disk that
  the running system is on.
- The network card is told by its hardware address, so its name is `eth0`
  under every kernel.
- The RAID copies to the second disk in the background after the install.
  `cat /proc/mdstat` shows how far it is. The server can restart meanwhile.
- With one disk gone the server still starts from the other. That start
  takes about three minutes longer, while it waits for the missing disk.
- The serial console shows the start and a login prompt, which helps when the
  server does not answer.
- If the server does not come back, start the rescue system again. The
  install is on the disks and can be looked at from there.
