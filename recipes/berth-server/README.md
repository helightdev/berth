# A berth server

This recipe turns a small Ubuntu machine into a berth server. The server
runs in a Docker container, and your tailnet reaches it over HTTPS. A rented
virtual machine with 2 GB of memory is enough, as the server only looks after
hosts that run elsewhere.

It was run on Ubuntu 26.04 and needs 24.04 or later.

## What you get

| Part | Set-up |
| --- | --- |
| Docker | `docker.io` from Ubuntu's packages |
| Container | `berth`, from the image `ghcr.io/helightdev/berth`, restarted with the machine |
| Data | `/srv/berth` with the folders `config`, `data` and `ssh` |
| Port | 7777 on this machine only |
| Tailscale | Installed and joined when you pass an enrollment key |
| Tailnet | `tailscale serve` puts the server at `https://THIS-MACHINE.YOUR-TAILNET.ts.net` with TLS |
| Firewall | `ufw`: incoming traffic is refused except SSH and the tailnet |

## Run it

On the machine itself:

```bash
curl -fsSL https://github.com/helightdev/berth/archive/refs/heads/main.tar.gz \
  | tar xz --strip-components=1 --wildcards '*/recipes' \
    '*/provision/ansible/roles/devbox/files/berth-server-upgrade.py'
cd recipes/berth-server
sudo ./install.sh -e tailscale_auth_key=tskey-auth-EXAMPLE
```

`install.sh` installs `ansible-core` from Ubuntu's packages and applies
`site.yml` to the machine it runs on. Its arguments go to `ansible-playbook`.

From another machine that has Ansible:

```bash
ansible-playbook -i 192.0.2.10, -u root site.yml -e tailscale_auth_key=tskey-auth-EXAMPLE
```

A second run changes nothing that is already in place. A container named
`berth` keeps its image, its data and its options, and is started when it
was stopped. It is made again only when it lacks one of the recipe's
options, such as the port or a folder in `/srv/berth`.

## Use it

The first run makes a setup key and prints it at the end, with the server's
address. The key lets the first user collect the server's token. It lasts 24
hours and ends once `berth login` has collected the token. Whoever runs the recipe can hand on
the key and never sees the token.

On your own machine, give berth the key when it asks:

```bash
berth login https://THIS-MACHINE.YOUR-TAILNET.ts.net
```

Or open berth's screens with the key in `BERTH_SETUP_KEY`, on Linux, macOS
or Windows:

```bash
read -rs BERTH_SETUP_KEY && export BERTH_SETUP_KEY
berth --server https://THIS-MACHINE.YOUR-TAILNET.ts.net
```

The screens sign in and show the token. `tailscale serve status` on
the machine says the address. Make a new key on the machine at any time:

```bash
sudo docker exec berth berth server setup-key
```

The token itself gives full control of every host. Without a key, pass it to
`berth login` on standard input, on the machine itself:

```bash
sudo cat /srv/berth/data/berth/serve.token | berth login https://THIS-MACHINE.YOUR-TAILNET.ts.net --token-stdin
```

## The server on a node

A large server can run the berth server and host the machines too. Install
Ubuntu, run the recipe with `berth_server_node`, and the server can apply its
own machine as a [node](../../docs/content/docs/%28guide%29/use/nodes.mdx):

```bash
sudo ./install.sh -e berth_server_node=true -e tailscale_auth_key=tskey-auth-EXAMPLE
```

| Part | Set-up |
| --- | --- |
| Login | The server's own key logs in as `root` here |
| Name | The server finds this machine at the name `node` |
| Docker | The container gets the socket of this machine's engine, and joins its group |

Then, on a berth connected to the server, add the machine as a node, list
its features and apply it. This example calls it `big`:

```bash
berth host add big node --user root --type node \
  --feature docker/engine --feature incus/server
berth apply big
```

The apply installs Incus and Docker, and the server registers their
provisioners, which reach them on this machine. Docker's socket and the root
login give root on this machine, so whoever has the server's token controls
the whole machine. A container from an earlier run keeps its data and its
own options and gets these: the run makes it again with both.

Once the machine is a node, its action `upgrade-server` updates the server
in place. See [Update it](#update-it).

## Settings

The settings are in [`vars.yml`](vars.yml), each with a comment. Change them
there, or keep your own file and pass it with `-e @my-vars.yml`.

| Setting | Default | Meaning |
| --- | --- | --- |
| `berth_server_image` | `ghcr.io/helightdev/berth` | The image, with a tag such as `:edge` if you want one |
| `berth_server_dir` | `/srv/berth` | Where the server keeps its settings, data and SSH keys |
| `berth_server_port` | `7777` | The port on this machine only |
| `berth_server_tailscale_serve` | `true` | Put the server on the tailnet with TLS |
| `berth_server_update` | `false` | Pull the image and run the server from it when it is newer. See [Update it](#update-it) |
| `berth_server_node` | `false` | Let the server apply this machine as a node. See [The server on a node](#the-server-on-a-node) |
| `tailscale_auth_key` | empty | An enrollment key. Without one Tailscale is not installed |
| `tailscale_login_server` | empty | The address of a Headscale server |
| `ssh_from` | `any` | Who reaches SSH from outside the tailnet: `any`, or an address or network |

Pass the enrollment key on the command line. Do not save it in a file you
commit.

This machine takes a key you make, in Tailscale's admin console or with
`headscale preauthkeys create`, because the recipe runs before berth does.
Make it one-use and short-lived. The hosts berth sets up later get their keys
from berth, which makes them through the tailnet's API with the secret
`tailnet_api_key`.

## Update it

Run the recipe again with `berth_server_update`. It pulls the image and,
when it is newer, makes the container again from it:

```bash
sudo ./install.sh -e berth_server_update=true
```

The new container keeps the data in `/srv/berth` and every option of the old
one: its mounts, ports, extra host names, groups, environment, logging and
restart policy. An option the recipe sets, such as the port, takes the value
of this run. The old container is stopped and kept until the new one runs,
and starts again if the new one does not come up. A container with an option
the helper cannot carry over, such as `--privileged`, is left as it is, and
the run says which option it was.

On a machine that runs Incus, the container keeps the name `incus-host`, at
which the provisioners of berth 0.5 reach it. A run without
`berth_server_update` leaves the image as it is.

Pin a version with its tag:

```bash
sudo ./install.sh -e berth_server_update=true -e berth_server_image=ghcr.io/helightdev/berth:0.6.0
```

A server never goes back to an older version this way. Download the recipe
of the new version first, with the command in [Run it](#run-it), as each
version's recipe knows its own server.

Once the machine is a node, its action `upgrade-server` does the same from a
berth of the new version:

```bash
berth run big upgrade-server version=0.6.0
```

Do not remove the container by hand to update it. A new one from the recipe
has only the recipe's options, and loses those passed by hand.

## Good to know

- The tailnet address needs HTTPS certificates switched on for your tailnet.
  Set `berth_server_tailscale_serve` to `false` to leave the server on this
  machine only.
- A private image needs `sudo docker login ghcr.io` before the run.
- The server connects to its hosts over SSH with the keys in
  `/srv/berth/ssh`. That folder belongs to the user 1000, as the server in
  the container is that user.
- Back up `/srv/berth`. It holds the profiles, the secrets and the token.
- With `ssh_from` set to one address, SSH from anywhere else is refused.
  Your provider's console is the way back in if the address was wrong.
