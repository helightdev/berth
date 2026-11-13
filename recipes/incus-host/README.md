# An Incus and Docker host

This recipe turns an empty Ubuntu server into a host for machines. It installs
Incus and Docker and sets up a firewall. The guests get a network and a
storage pool. berth can then create its hosts on the server as Incus guests,
so several agent machines run on one large server.

It was run on Ubuntu 26.04 and needs 24.04 or later.

## What you get

| Part | Set-up |
| --- | --- |
| Incus | From Ubuntu's packages, with QEMU for virtual machines |
| Storage | A btrfs pool named `default`: a 120 GiB file, or a disk you name |
| Network | The bridge `incusbr0` at `10.77.0.1/24`. Guests get an address from it and go out through the server. No IPv6 |
| Default profile | Nesting is on. A container's systemd needs it, and so does Docker in a guest |
| Incus API | Port 8444, open on the tailnet interface only |
| Docker | `docker.io` with Compose and Buildx, from Ubuntu's packages |
| Firewall | `ufw`: incoming traffic is refused except SSH, the tailnet and the guests' bridge |
| Tailscale | Installed and joined when you pass an enrollment key |
| berth server | On request, berth's server in a Docker container, set up to create machines on this Incus |

## Before you start

The recipe does not wipe anything, so start from a fresh install. Reinstall
the server with plain Ubuntu and your SSH key from your provider's panel, or
with the [`rescue-install`](../rescue-install/) recipe when that installer
does not work.

Virtual machines need KVM. A dedicated server has it. A rented virtual server
often does not, and then only containers work. Set `incus_virtual_machines` to
`false` there.

## Run it

On the server itself:

```bash
curl -fsSL https://github.com/helightdev/berth/archive/refs/heads/main.tar.gz \
  | tar xz --strip-components=1 --wildcards '*/recipes'
cd recipes/incus-host
sudo ./install.sh -e tailscale_auth_key=tskey-auth-EXAMPLE
```

`install.sh` installs `ansible-core` from Ubuntu's packages and applies
`site.yml` to the machine it runs on. Its arguments go to `ansible-playbook`.

From another machine that has Ansible:

```bash
ansible-playbook -i 192.0.2.10, -u root site.yml -e tailscale_auth_key=tskey-auth-EXAMPLE
```

A second run changes nothing that is already in place. It leaves a server
alone that has a storage pool, so it never touches existing guests.

## Settings

The settings are in [`vars.yml`](vars.yml), each with a comment. Change them
there, or keep your own file and pass it with `-e @my-vars.yml`.

| Setting | Default | Meaning |
| --- | --- | --- |
| `incus_virtual_machines` | `true` | Install QEMU for virtual machines |
| `incus_pool_size` | `120GiB` | Size of the storage pool's file |
| `incus_pool_source` | empty | An empty disk or partition to use instead of a file. Its contents are lost |
| `incus_bridge_address` | `10.77.0.1/24` | The server's address on the guests' bridge |
| `incus_api_address` | `[::]:8444` | Where the Incus API listens |
| `incus_api_interface` | `tailscale0` | The one interface the firewall opens the API on. Empty opens it nowhere |
| `incus_trusted_clients` | none | Clients that may use the API, each with a name and a certificate file |
| `berth_server` | `false` | Also run a berth server here. See below |
| `tailscale_auth_key` | empty | An enrollment key. Without one Tailscale is not installed |
| `tailscale_login_server` | empty | The address of a Headscale server |
| `ssh_from` | `any` | Who reaches SSH from outside the tailnet: `any`, or an address or network |

Pass the enrollment key on the command line. Do not save it in a file you
commit.

## A berth server on the same machine

With `berth_server` set, the recipe also runs berth's server in a Docker
container:

```bash
sudo ./install.sh -e berth_server=true -e tailscale_auth_key=tskey-auth-EXAMPLE
```

| Part | Set-up |
| --- | --- |
| Container | `berth`, from the image `ghcr.io/helightdev/berth`, restarted with the machine |
| Data | `/srv/berth` with the folders `config`, `data` and `ssh` |
| Port | 7777 on this machine only |
| Tailnet | `tailscale serve` puts it at `https://THIS-MACHINE.YOUR-TAILNET.ts.net` with TLS |
| Incus | The server gets a key and a certificate that this Incus trusts, stored as its secrets |

The first run makes a setup key and prints it at the end, with the server's
address. The key lets the first user collect the server's token, for 24 hours
or until they choose Done. On your own machine:

```bash
berth connect https://THIS-MACHINE.YOUR-TAILNET.ts.net
```

berth asks for the key. In a browser, open
`https://THIS-MACHINE.YOUR-TAILNET.ts.net/#setup-key=KEY` instead. Make a new
key with `sudo docker exec berth berth setup-key`.

The token itself gives full control of every host. Without a key, read it with
`sudo cat /srv/berth/data/berth/serve.token`, then set it with
`berth config server` and `berth config server_token`. To create a machine on this Incus, give the
provisioner the argument `server=https://incus-host:8444`. The container
connects to the guests at their addresses on the bridge. berth can set up a
new machine there before it has joined a tailnet.

The tailnet address needs HTTPS certificates switched on for your tailnet.
Set `berth_server_tailscale_serve` to `false` to leave the server on this
machine only. A private image needs `docker login ghcr.io` before the run.

## Let another berth create machines here

berth talks to the Incus API with a client certificate. Make a key and a
certificate on the machine that runs berth:

```bash
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:secp384r1 -sha384 \
  -nodes -days 3650 -subj "/CN=berth" -keyout berth.key -out berth.crt
```

Then let the server trust it. Either name it in `incus_trusted_clients` and
run the recipe again, or copy `berth.crt` to the server and add it there:

```bash
incus config trust add-certificate berth.crt --name berth
```

Store the key and the certificate as berth's secrets `incus_client_key` and
`incus_client_cert`. The guide of berth's Incus plugin has the rest.

## Good to know

- The bridge's network must not be one the server is on already. Change
  `incus_bridge_address` if `10.77.0.0/24` is taken.
- The guests' addresses are only reachable from the server. A guest that
  berth sets up joins your tailnet, and berth reaches it there.
- Docker opens the ports its containers publish by itself, past `ufw`.
  Publish a port on `127.0.0.1` or on the tailnet address to keep it private.
- With `ssh_from` set to one address, SSH from anywhere else is refused.
  Your provider's console is the way back in if the address was wrong.
- To look at the result: `incus info`, `incus list`, `sudo ufw status`.
