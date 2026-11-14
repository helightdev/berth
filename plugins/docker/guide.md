# Docker

This plugin makes a machine out of a Docker container. The container runs
Ubuntu 26.04 with systemd as its first process and an SSH server. berth
applies its role to it over SSH, as it does to any other machine.

Use it to try berth without a spare machine, or to run a host on a Docker
engine you already have. It also runs Compose projects on the same engine,
as [deployments](#deployments), and plain containers from any image, as
[instances](#instances).

## What it makes

- A container named `berth-HOST`, with the labels `berth-host` and
  `berth-instance`. It restarts unless you stop it.
- An operator account with passwordless sudo. berth's own key and the profile's
  operator keys can log in as that account.
- A volume mounted at `/srv/agent`. The agent's data stays when the container is replaced.

The bundled image is built once, as `berth-docker-host:0.2.0`, when the first
container is made or the action `build` asks for it. The account and keys are
added when the container is created. The same image serves every host.

## Arguments

In berth each argument has a short label, and its row's Information panel
says what it means. CPUs and Memory are under Machine on the Settings tab.

The plugin brings the type `docker/container`. Register a provisioner of it for
each engine, under a name of your own:

    berth provisioner add docker docker/container
    berth provisioner add build-box docker/container host=ssh://ops@192.0.2.20

`host` is the provisioner's own argument. berth keeps it as a setting, which
every machine and action uses unless it is given another:

    berth provisioner build-box run settings host=ssh://ops@192.0.2.20

| Name | Default | Meaning |
| --- | --- | --- |
| `host` | empty | The Docker endpoint, such as `ssh://user@192.0.2.20`. Empty uses the local default. |
| `image` | empty | An image to use instead of the bundled one. It needs systemd, sshd, sudo and python3. |
| `runtime` | `privileged` | `privileged` or `sysbox`. See [Sysbox](#sysbox). |
| `ssh_port` | empty | The port to publish SSH on. Empty publishes none, and berth uses the container's own address on the Docker network. |
| `publish_address` | `127.0.0.1` | The address berth uses with `ssh_port`. Loopback binds the port on loopback only. Any other address binds it on all interfaces. |
| `volume` | `berth-HOST-data` | The volume for `/srv/agent`. |
| `cpus` | empty | The CPUs the container may use, such as `2` or `1.5`. Empty means no limit. |
| `memory` | empty | The memory the container may use, such as `4GiB`, `512MiB` or `4g`. Empty means no limit. |
| `keep_volume` | `no` | `yes` keeps the volume when the container is destroyed. It is a choice of `no` and `yes`. |

`cpus` and `memory` cap the container with Docker's own limits. There is no
disk limit, because Docker can cap a container's disk only on some storage drivers.

Without `ssh_port`, the container's address is only reachable from the machine
that runs Docker. Publish a port when berth runs elsewhere.

### An engine over SSH

With `host=ssh://user@address`, the plugin only sets `DOCKER_HOST`. Docker then
starts the system's `ssh` for each call, with your own SSH configuration. It
does not use berth's recorded host key of that server, and it does not use
berth's key. So:

- The address has to be in your own known hosts. Log in once with plain `ssh`,
  such as `ssh ops@192.0.2.20`, and accept the key.
- The account's key has to be one your `ssh` offers, from your SSH
  configuration or agent.
- ssh cannot ask for a password, because the calls have no terminal.

### berth in a container

berth's own image has Docker's client, Compose and Buildx. To use the engine of
the machine the container runs on, give the container the engine's socket and
the socket's group:

    docker run … --volume /var/run/docker.sock:/var/run/docker.sock \
      --group-add "$(stat --format %g /var/run/docker.sock)" ghcr.io/helightdev/berth

Leave `host` empty then. The socket gives root on that machine. The
`incus-host` recipe sets its berth server up this way.

## Machines

| Verb | What it does |
| --- | --- |
| Start, stop | Start or stop the container. |
| Change | New CPU and memory limits for a container that runs on. |
| Logs | The latest lines of the container's console, or of its journal when the console is empty. berth shows them while it waits for SSH. |
| Destroy | Removes the container, and the volume unless `keep_volume` is `yes`. A volume that berth did not create is always kept. |
| List | The containers on the engine with the `berth-host` label. |

berth never touches a container without that label, even when its name
matches.

## Instances

An instance is one plain container from any image, such as `nginx:1.27` or
`debian:12`. It becomes no berth host, and berth puts nothing of its own in it.
Make one with **+ New instance** under the hosts, or the action New instance
on the provisioner's page. Both open a form laid out like a host's set-up, with every field on a row. Or:

    berth provisioner docker run new-instance name=web image=nginx:1.27 ports=8080:80

| Argument | Default | Meaning |
| --- | --- | --- |
| `image` | empty | The image to start from. It's required. |
| `command` | empty | The command to run instead of the image's own. |
| `environment` | empty | Variables, one `NAME=VALUE` a line. |
| `ports` | empty | Ports to publish, comma-separated, such as `8080:80`. |
| `volumes` | empty | Volumes to mount, comma-separated, as `NAME:/path` or `/host/path:/path`. |
| `network` | empty | The Docker network to attach it to. |
| `restart` | `no` | `no`, `unless-stopped` or `always`. |
| `cpus`, `memory` | empty | Limits, as for machines. |

The container gets the labels `berth-resource` and `berth-token`. berth starts,
stops, changes the limits of and removes only a container with the first one,
and not one of a deployment. Only `cpus` and `memory` change in place: a change
to another argument is refused, so make a new instance for it. The values of
`environment` reach Docker in its environment, not on its command line.
Removing an instance removes its anonymous volumes and keeps named ones.

## Deployments

A deployment is a Compose project. Its spec is a Compose file, and its name is
the project's name: lowercase letters, digits, `-` and `_`, starting with a
letter or digit. Compose runs where berth runs, against the engine named by
`host`, so the machine that runs berth needs the `docker compose` plugin.

The plugin gives the Compose file to `docker compose` on standard input, from
an empty directory. A path in the file is taken from that empty directory, so
give bind mounts as absolute paths on the Docker host.

Write `${NAME}` in the file for a secret value, such as a
password. berth gives the secrets of the deployment to `docker compose` as
variables of its environment, and Compose puts them in place. They are never
on a command line or in a file, and the plugin hides them in what it shows.

berth's labels `berth-resource` and `berth-token` go on every container of the
project. The plugin lists, changes and removes only projects whose containers
have them.

| Verb | What it does |
| --- | --- |
| Create, change | `docker compose up -d --remove-orphans` with the Compose file. Running it again with the same file does not change anything. |
| Start, stop | Start or stop the project's containers. |
| Logs | The logs of every service. |
| Destroy | `docker compose down`. It removes the project's named volumes too, unless `keep_volumes` is `yes`. |
| List | The projects whose containers have berth's labels. |

Make one with `berth deploy NAME --provisioner NAME --spec FILE`, or with the
action `new-deployment` on the provisioner's page, which takes the Compose file
pasted whole.

| Argument | Default | Meaning |
| --- | --- | --- |
| `keep_volumes` | `no` | `yes` keeps the project's named volumes when the deployment is removed. |

A deployment's page lists these actions:

| Action | What it does |
| --- | --- |
| `services` | Each container of the project, with its service and state. |
| `restart` | Restarts the containers as they are. |
| `pull` | Pulls the images the Compose file names and brings the project up again, so a container whose image changed is recreated. |

## Actions

The provisioner's page in berth, and `berth provisioner NAME`, list these
actions:

| Action | What it does |
| --- | --- |
| `engine` | Says the engine's version and runtimes, and how many machines berth made there. |
| `images` | The engine's images, with their sizes. |
| `volumes` | Every volume of the engine, marked when berth made it, and whether a container uses each. |
| `create-volume` | Creates a named volume, which an instance can mount. |
| `remove-volume` | Removes a named volume. Docker refuses one that a container uses. |
| `pull-image` | Pulls an image onto the engine. |
| `remove-image` | Removes an image. Docker refuses one that a container uses. |
| `build` | Builds the bundled image ahead of the first machine. |
| `prune` | Removes the images of older versions of this plugin that no container uses. With `volumes=yes` it also removes the volumes berth made that no container uses, such as those kept with `keep_volume`, and the work in them. |

`prune` does not remove a container, nor anything berth did not make.

## Sysbox

[Sysbox](https://github.com/nestybox/sysbox) is a container runtime. Under it
a container runs systemd and Docker without `--privileged`, and root in the
container is not root on the Docker host. With `runtime=sysbox` the plugin
starts the container with `--runtime sysbox-runc` in place of `--privileged`
and its cgroup and `/run` options:

    berth provision studio-2 docker runtime=sysbox

The Docker host needs Sysbox installed. The plugin checks for the runtime
`sysbox-runc` before it makes anything, and says what to install when it is
missing. The action `engine` lists the runtimes the engine has, and the
provisioner's page offers `sysbox` only where the engine has it. The recipe
`incus-host` in the berth source installs Sysbox beside Docker when you ask it
to. Sysbox's own documentation lists the distributions and kernels it supports.

Whether berth's role applies cleanly inside a Sysbox container has not been
checked yet. These are the parts to watch: the firewall, the tailnet's tunnel
device and the desktop's renderer.

## Limits

A container is not a full host.

- By default the container runs privileged. systemd needs a cgroup tree of its
  own, and the firewall needs to change network rules. Docker allows both only
  to a privileged container. Root in the container has the rights of root on
  the machine that runs Docker, so use an engine you trust the host's users
  with, or Sysbox.
- Chromium can't sandbox itself in a user namespace when the machine that
  runs Docker is Ubuntu. berth gives the Chromium that comes with Playwright
  its setuid sandbox helper instead. A Chromium that the agent installs itself
  has none, so it can't start sandboxed.
- Files outside `/srv/agent` go with the container when it is destroyed.
