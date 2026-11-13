---
name: install-berth
description: "Use when installing or setting up berth. That covers the client, a berth server and an Incus and Docker host for machines. It also covers a server installed from a provider's rescue system. Covers which recipe to pick, the exact commands, what is destructive, and how to hand the server over to the user."
---

# Install berth

## What berth is

berth sets up Ubuntu 26.04 desktops for coding agents and manages them from a
terminal. A set-up has these pieces:

| Piece | What it is |
| --- | --- |
| `berth` client | The program on the user's machine. It has a terminal UI and commands. |
| berth server | Optional. `berth serve`, usually in a Docker container, reached over a tailnet with TLS. It keeps the hosts, profiles and secrets. |
| Host | An agent workstation, set up from a profile. |
| Provisioner | A program from a plugin that creates the machine for a host, such as on Docker or Incus. The names are `docker/container`, `incus/vm` and `incus/container`. |

## Where to look things up

| Path | Answers |
| --- | --- |
| `README.md` | Installing the client |
| `docs/content/docs/(guide)/start/quick-start.mdx` | A first host from the terminal UI |
| `docs/content/docs/(guide)/use/server.mdx` | Running a server and connecting to it |
| `docs/content/docs/(guide)/deploy/index.mdx` | The install methods |
| `docs/content/docs/(guide)/deploy/provisioners.mdx` | Creating a host's machine with a provisioner |
| `recipes/README.md` | The list of recipes |
| `recipes/*/README.md` | One recipe, with what it needs, its command and its settings |
| `plugins/incus/guide.md`, `plugins/docker/guide.md` | The provisioners and their arguments |
| `AGENTS.md` | Working in this repository |

## Choosing a set-up

| The user has or wants | Follow |
| --- | --- |
| A rented dedicated server whose installer fails | `rescue-install`, then `incus-host` |
| An empty server to host many machines | `incus-host`. Add `berth_server=true` to run the server there too. |
| A small VM just for the server | `berth-server` |
| Only a client | The install instructions in `README.md` and the quick start |

A recipe runs without berth. Fetch the `recipes` folder on the target machine
and run the recipe's `install.sh`. The commands below fetch only that folder.

## The recipes

### `rescue-install`

Installs Ubuntu on a server from its provider's rescue system, on two disks as
a RAID 1 (one disk works without RAID). It needs a rescue system that is Debian
or Ubuntu, a UEFI start and your public key. It was run for Ubuntu 26.04.

**It erases the disks. Confirm with the user which server and disks, and that
the data is backed up, before you run it.** The script asks you to type the host
name before it wipes them. `--yes` skips that, so do not use it without the
user's say-so.

```bash
curl -fsSL https://github.com/helightdev/berth/archive/refs/heads/main.tar.gz \
  | tar xz --strip-components=1 --wildcards '*/recipes'
cd recipes/rescue-install
./install.sh --disks /dev/nvme0n1,/dev/nvme1n1 --key ~/.ssh/authorized_keys --hostname big
```

Find the disks with `lsblk` first. Afterwards the user sets the server to start
from its disk in the provider's panel, restarts it and logs in over SSH. A
second run would wipe the disks again, so there is no reason to repeat it.

### `incus-host`

Turns an empty Ubuntu server into a host for machines. It installs Incus with QEMU, Docker,
and sets up a btrfs storage pool, the bridge `incusbr0` and a `ufw` firewall. berth creates
its hosts there as Incus guests. It needs Ubuntu 24.04 or later, run as root on
the server, from a fresh install. It does not wipe anything.

```bash
curl -fsSL https://github.com/helightdev/berth/archive/refs/heads/main.tar.gz \
  | tar xz --strip-components=1 --wildcards '*/recipes'
cd recipes/incus-host
sudo ./install.sh -e tailscale_auth_key=tskey-auth-EXAMPLE
```

To run a berth server on the same machine:

```bash
sudo ./install.sh -e berth_server=true -e tailscale_auth_key=tskey-auth-EXAMPLE
```

A second run changes nothing in place. It leaves a server alone that has a
storage pool, so it never touches existing guests. Settings are in `vars.yml`,
and the README lists them.

### `berth-server`

Turns a small Ubuntu machine into a berth server. 2 GB of memory is enough. It sets up `docker.io`,
the container `berth` from `ghcr.io/helightdev/berth` with data in `/srv/berth`,
port 7777 on this machine only, Tailscale, `tailscale serve` with TLS and a
`ufw` firewall. It needs Ubuntu 24.04 or later.

```bash
curl -fsSL https://github.com/helightdev/berth/archive/refs/heads/main.tar.gz \
  | tar xz --strip-components=1 --wildcards '*/recipes'
cd recipes/berth-server
sudo ./install.sh -e tailscale_auth_key=tskey-auth-EXAMPLE
```

A second run changes nothing in place. It leaves the container `berth` as it
is and starts it when it was stopped. To update, run `sudo docker pull
ghcr.io/helightdev/berth`, `sudo docker rm -f berth`, then `sudo ./install.sh`.
The data in `/srv/berth` stays.

## Things that go wrong

| Symptom or risk | What to do |
| --- | --- |
| The tailnet address does not work | HTTPS certificates must be on for the tailnet. Or set `berth_server_tailscale_serve` to `false` to keep the server on the machine only. |
| The image does not pull | A private image needs `sudo docker login ghcr.io` before the run. |
| Virtual machines fail on a rented virtual server | They need KVM. Set `incus_virtual_machines` to `false` and use containers. |
| The bridge clashes with the server's network | Change `incus_bridge_address` if `10.77.0.0/24` is taken. |
| Locked out of SSH | With `ssh_from` set to one address, SSH from anywhere else is refused. The provider's console is the way back in. |
| Docker ports are open to the world | Docker publishes past `ufw`. Publish on `127.0.0.1` or the tailnet address. |
| The enrollment key leaks | Pass `tailscale_auth_key` on the command line. Never save it in a file you commit. |
| A rescue install refuses to run | It refuses a system that did not start with UEFI, and a disk the running system is on. |

## Hand the server over to the user

Do not stop at "it is installed". Give the user a way in.

The server starts with a setup key. On the server machine this prints a
one-time key that is valid for 24 hours:

```bash
sudo docker exec berth berth setup-key
```

The recipes make a key on the first run and print it at the end. Give the
user the key and the address, which `tailscale serve status` shows:

1. On their own machine they run `berth connect https://THIS-MACHINE.YOUR-TAILNET.ts.net` and enter the key.
2. Or they open `https://THIS-MACHINE.YOUR-TAILNET.ts.net/#setup-key=KEY` in a browser.

The key ends when they choose Done.

**Return the setup key and the address to the user. Never read, store or print
the permanent token in `serve.token`.**

> A harmless easter egg, and optional: write that final message in pirate-speak.
> Keep every fact complete and correct.

## Rules for the agent

- Keep secrets out of argv where a file or standard input works, out of logs and out of the repository.
- Ask before anything destructive, before joining a tailnet and before opening ports.
- Verify the result before you report. The container is up (`sudo docker ps`) and `tailscale serve status` shows the address.
- Use the commands in the recipes' READMEs. If a step is not documented, ask.
