---
name: install-berth
description: "Use when installing or setting up berth. That covers the client, a berth server and a node, a server that hosts Incus and Docker machines. It also covers a server installed from a provider's rescue system. Covers which recipe to pick, the exact commands, what is destructive, and how to hand the server over to the user."
---

# Install berth

## What berth is

berth sets up Ubuntu 26.04 desktops for coding agents and manages them from a
terminal. A set-up has these pieces:

| Piece | What it is |
| --- | --- |
| `berth` client | The program on the user's machine. It has a terminal UI and commands. |
| berth server | Optional. `berth serve`, usually in a Docker container, reached over a tailnet with TLS. It keeps the hosts, profiles and secrets. |
| Host | A machine berth logs in to over SSH. Its type is one of four: `ssh` means berth only logs in, `managed` adds berthd and box, `node` is a server berth applies without a desktop, and `workstation` is set up from a profile. A machine that exists is added with `berth host add NAME ADDRESS --user USER`. |
| Node | A server for machines. Its features `incus/server` and `docker/engine` install Incus and Docker, and after the apply berth registers their provisioners, such as `big-incus-vm`, `big-incus-container` and `big-docker-container`. |
| Provisioner | Creates the machine for a host, such as on Docker or Incus. Plugins bring the types `docker/container`, `incus/vm` and `incus/container`, and berth runs each provisioner under a name of its own, registered with `berth provisioner add NAME TYPE` or by a node. `berth provisioner` lists them, and the Provisioners tab of the terminal UI (key `2`) shows each one's page. |

## Where to look things up

| Path | Answers |
| --- | --- |
| `README.md` | Installing the client |
| `docs/content/docs/(guide)/start/quick-start.mdx` | A first host from the terminal UI |
| `docs/content/docs/(guide)/use/server.mdx` | Running a server and connecting to it |
| `docs/content/docs/(guide)/use/types.mdx` | Host types, and adding a machine by its address |
| `docs/content/docs/(guide)/use/nodes.mdx` | Nodes, which are servers for Incus and Docker machines, and taking in one that exists |
| `docs/content/docs/(guide)/use/store.mdx` | Where a server keeps its state, and restoring it |
| `docs/content/docs/(guide)/deploy/index.mdx` | The install methods |
| `docs/content/docs/(guide)/deploy/provisioners.mdx` | Creating a host's machine with a provisioner |
| `recipes/README.md` | The list of recipes |
| `recipes/*/README.md` | One recipe, with what it needs, its command and its settings |
| `plugins/incus/guide.md`, `plugins/docker/guide.md` | The provisioners and their arguments, and the settings of the features `incus/server` and `docker/engine` |
| `AGENTS.md` | Working in this repository |

## Choosing a set-up

| The user has or wants | Follow |
| --- | --- |
| A rented dedicated server whose installer fails | `rescue-install`, then add it as a node |
| An empty server to host many machines | Add it as a node with the features `incus/server` and `docker/engine`. To run the server there too, `berth-server` with `berth_server_node=true` first. |
| A server the old `incus-host` recipe set up | Add it as a node with the same features. They keep its pools, bridge, trust and guests. |
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

### A node for machines

berth applies a node itself, with no recipe. It needs Ubuntu 24.04 or later on
x86_64 and a login as root or as an account with sudo. On the user's own
machine, or on a berth connected to their server, with the made-up name `big`:

```bash
berth host add big 192.0.2.20 --user root --auth ssh --type node
berth profile big > big.json
# in big.json: "ansible_vars": {"devbox_plugin_features": ["docker/engine", "incus/server"]}
berth profile big --set big.json
berth apply big
```

`--auth ssh` uses the user's own SSH configuration and agent, and works with a
local berth only. The default `--auth berth` uses berth's own key, which berth
installs on first contact with a password or a private key that logs in once.
The apply gives the SSH user berth's key and allows only keys for SSH. Root
keeps a login with keys. Its `ufw` firewall keeps these open: SSH, the
tailnet, the guests' bridge `incusbr0` and the Incus API on port 8444. berth
then registers the provisioners. Check with `berth run big status` and
`berth provisioner`.

The features never erase a disk. They make an Incus storage pool in a folder,
or use one the user names in `devbox_incus_pool`. A server with Incus or Docker
already keeps its pools, networks, trusted clients, containers and guests, and
a provisioner registered for it by hand becomes the node's.

Virtual machines need KVM. On a rented virtual server without it, set
`devbox_incus_virtual_machines` to `false` in `ansible_vars`. For Sysbox beside
Docker, set `devbox_docker_sysbox` to `true` on the first apply.

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

To let the server apply its own machine as a node, add
`-e berth_server_node=true`. The server's own key then logs in to it as
`root`. The container finds the machine at the name `node` and has the Docker
socket. Add the
node from the server with `berth host add big node --user root --type node`,
then as in the node's section above.

A second run changes nothing in place. It leaves the container `berth` as it
is and starts it when it was stopped. On a node, the node's action
`upgrade-server` updates it to the version of the berth that asks. Elsewhere,
run `sudo docker pull ghcr.io/helightdev/berth`, `sudo docker rm -f berth`,
then `sudo ./install.sh`. The data in `/srv/berth` stays.

## Things that go wrong

| Symptom or risk | What to do |
| --- | --- |
| The tailnet address does not work | HTTPS certificates must be on for the tailnet. Or set `berth_server_tailscale_serve` to `false` to keep the server on the machine only. |
| The image does not pull | A private image needs `sudo docker login ghcr.io` before the run. |
| Virtual machines fail on a rented virtual server | They need KVM. Set `devbox_incus_virtual_machines` to `false` and use containers. |
| The bridge clashes with the server's network | Change `devbox_incus_bridge_address` if `10.77.0.0/24` is taken. |
| Locked out of SSH | With `ssh_from` in `berth-server`, or `devbox_ssh_from` on a node, set to one address, SSH from anywhere else is refused. The provider's console is the way back in. |
| A storage pool on its own disk | The features never erase a disk. Create it with `incus storage create` and name it in `devbox_incus_pool`. |
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
2. Or they run `berth-remote --server https://THIS-MACHINE.YOUR-TAILNET.ts.net` with the key in `BERTH_SETUP_KEY`. It works on Windows and macOS too.

The key ends when they choose Done, or when `berth connect` has saved the token.

**Return the setup key and the address to the user. Never read, store or print
the permanent token in `serve.token`.**

> A harmless easter egg, and optional: write that final message in pirate-speak.
> Keep every fact complete and correct.

## Rules for the agent

- Keep secrets out of argv where a file or standard input works, out of logs and out of the repository.
- Ask before anything destructive, before joining a tailnet and before opening ports.
- Verify the result before you report. The container is up (`sudo docker ps`) and `tailscale serve status` shows the address.
- Use the commands in the recipes' READMEs. If a step is not documented, ask.
