# Docker

This plugin makes a machine out of a Docker container. The container runs
Ubuntu 26.04 with systemd as its first process and an SSH server. berth
applies its role to it over SSH, as it does to any other machine.

Use it to try berth without a spare machine, or to run a host on a Docker
engine you already have.

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

`host` is the provisioner's own argument. berth keeps it as a setting, which
every machine and action uses unless it is given another:

    berth provisioner docker/container run settings host=ssh://ops@192.0.2.20

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

## Machines

| Verb | What it does |
| --- | --- |
| Start, stop | Start or stop the container. |
| Change | New CPU and memory limits for a container that runs on. |
| Logs | The container's log. |
| Destroy | Removes the container, and the volume unless `keep_volume` is `yes`. A volume that berth did not create is always kept. |
| List | The containers on the engine with the `berth-host` label. |

berth never touches a container without that label, even when its name
matches.

## Actions

The provisioner's page in berth, and `berth provisioner docker/container`,
list these actions:

| Action | What it does |
| --- | --- |
| `engine` | Says the engine's version and runtimes, and how many machines berth made there. |
| `images` | The engine's images, with their sizes. |
| `volumes` | The volumes berth made, and whether a container uses each. |
| `build` | Builds the bundled image ahead of the first machine. |
| `prune` | Removes the images of older versions of this plugin that no container uses. With `volumes=yes` it also removes the volumes berth made that no container uses, such as those kept with `keep_volume`, and the work in them. |

`prune` does not remove a container, nor anything berth did not make.

## Sysbox

[Sysbox](https://github.com/nestybox/sysbox) is a container runtime. Under it
a container runs systemd and Docker without `--privileged`, and root in the
container is not root on the Docker host. With `runtime=sysbox` the plugin
starts the container with `--runtime sysbox-runc` in place of `--privileged`
and its cgroup and `/run` options:

    berth provision studio-2 docker/container runtime=sysbox

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
- Files outside `/srv/agent` go with the container when it is destroyed.
