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

The bundled image is built once, as `berth-docker-host:0.1.0`, the first time
a container is made. The account and keys are added when the container is
created. The same image serves every host.

## Arguments

In berth each argument has a short label, and its row's Information panel
says what it means. CPUs and Memory are under Machine on the Settings tab.

| Name | Default | Meaning |
| --- | --- | --- |
| `host` | empty | The Docker endpoint, such as `ssh://user@192.0.2.20`. Empty uses the local default. |
| `image` | empty | An image to use instead of the bundled one. It needs systemd, sshd, sudo and python3. |
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

## Actions

| Action | What it does |
| --- | --- |
| Start, stop | Start or stop the container. |
| Destroy | Removes the container, and the volume unless `keep_volume` is `yes`. A volume that berth did not create is always kept. |
| List | The containers on the engine with the `berth-host` label. |
| Log | The latest lines of the container's console, or of its journal when the console is empty. berth shows them while it waits for SSH. |

berth never touches a container without that label, even when its name
matches.

## Limits

A container is not a full host.

- The container runs privileged. systemd needs a cgroup tree of its own, and
  the firewall needs to change network rules. Docker allows both only to a
  privileged container. Root in the container has the rights of root on the
  machine that runs Docker, so use an engine you trust the host's users with.
- Chromium can't sandbox itself in a user namespace when the machine that
  runs Docker is Ubuntu. berth gives the Chromium that comes with Playwright
  its setuid sandbox helper instead. A Chromium that the agent installs itself
  has none, so it can't start sandboxed.
- Files outside `/srv/agent` go with the container when it is destroyed.
