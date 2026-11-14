# Incus

This plugin creates the machine for a host on an Incus server. It brings two
types of provisioner:

| Type | What it makes |
| --- | --- |
| `incus/vm` | A virtual machine. The server needs KVM. |
| `incus/container` | A system container. It starts faster and shares the server's kernel. |

Register a provisioner of a type for each Incus server, under a name of your
own and with the server as its setting:

    berth provisioner add ovh-incus incus/vm server=https://192.0.2.10:8443

Both start from an Ubuntu cloud image and set themselves up from berth's cloud
seed. Because the plugin copies the release into the machine itself, the profile
doesn't need a `release_url`. berth runs the plugin where berth runs and talks to the Incus API
over HTTPS. The `incus` command doesn't have to be installed there.

The machine is named `berth-HOST`, such as `berth-studio`. The plugin marks
it with `user.berth-host` and `user.berth-instance`. It lists and deletes only
machines with these marks.

While berth waits for a new machine to answer on SSH, it shows the latest lines
of the machine's console log. When the console has none, the plugin reads
cloud-init's output in the machine instead.

## Let the server trust berth

Incus accepts a client whose certificate is in its trust store. Make a key and
a certificate for berth:

    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
      -days 3650 -subj /CN=berth -keyout berth.key -out berth.crt

Copy `berth.crt` to the server and add it to the trust store there:

    incus config trust add-certificate berth.crt

Then store both files as berth secrets and delete the copies:

    berth secret set incus_client_cert < berth.crt
    berth secret set incus_client_key < berth.key

By default berth checks the server's certificate against the system's
certificate authorities. Incus makes its own certificate when it's installed,
which those don't know. Store that certificate as a third secret, and berth then
accepts exactly that certificate and no other:

    berth secret set incus_server_cert < server.crt

On the server the certificate is `/var/lib/incus/server.crt`.

Every Incus provisioner shares these three secrets. A provisioner for another
server may be given its own instead:

    berth secret set provisioner/lab-incus/incus_server_cert < lab-server.crt

The action `secrets` on the provisioner's page sets them too.

## Arguments

In berth each argument has a short label, and its row's Information panel
says what it means. The server is on the Install tab. CPUs, memory and disk
size are under Machine on the Settings tab.

`server` and `project` are the provisioner's own arguments. berth keeps them as
its settings, which every machine and action uses unless the call gives a value:

    berth provisioner ovh-incus run settings server=https://192.0.2.10:8443

| Argument | Default | Meaning |
| --- | --- | --- |
| `server` | | The address of the Incus API, such as `https://192.0.2.10:8443`. Required. |
| `cpus` | `4` | CPUs for the machine. |
| `memory` | `8GiB` | Memory for the machine. |
| `disk` | `60GiB` | Size of the root disk. |
| `image` | `ubuntu/26.04/cloud` | Image on `https://images.linuxcontainers.org`. Use a `cloud` image, which runs the seed. |
| `pool` | `default` | Storage pool for the root disk. |
| `network` | | Network for the machine. Left empty, the machine uses the network of the `default` profile. |
| `project` | `default` | Incus project the machine goes in. |

## Machines

Besides creating, starting, stopping and deleting, the plugin changes a
machine's CPU and memory limits and grows its root disk. Its log is the
console log Incus keeps, or cloud-init's output when the console has none.

## Instances

An instance is a plain container or virtual machine that doesn't become a berth
host. It starts from any image on `https://images.linuxcontainers.org`, such as
`debian/12`, and berth puts nothing of its own in it. Cloud-init user data that
you paste sets it up on its first boot, if the image has cloud-init.

Make one with **+ New instance** under the hosts, or the action New instance
on the provisioner's page. Both open a form laid out like a host's set-up, with every field on a row. Or:

    berth provisioner ovh-incus run new-instance name=web image=debian/12

berth keeps it as a resource with a page of its own. There it starts, stops,
changes and removes it, and shows its console log. The plugin marks it with
`user.berth-resource` and `user.berth-token`. Only `cpus`, `memory` and `disk`
change in place: a change to another argument is refused, so make a new
instance for it.

| Argument | Default | Meaning |
| --- | --- | --- |
| `image` | `ubuntu/24.04` | Image to start from. |
| `user_data` | | Cloud-init user data, several lines. Empty leaves the image as it is. |
| `cpus` | `2` | CPUs for the instance. |
| `memory` | `2GiB` | Memory for the instance. |
| `disk` | `20GiB` | Size of the root disk. |
| `pool` | `default` | Storage pool for the root disk. |
| `network` | | Network for the instance. Left empty, it uses the network of the `default` profile. |
| `nesting` | `yes` | Lets a container run containers of its own, such as Docker's. |

## Actions

The provisioner's page in berth, and `berth provisioner NAME`, list these
actions:

| Action | What it does |
| --- | --- |
| `server` | Says the server's version, its kernel and storage drivers, and whether it trusts berth. |
| `pools` | The storage pools, with how much of each is used. |
| `images` | The images the server keeps in the project, with their sizes. |
| `networks` | The networks a machine can be attached to. |
| `volumes` | The custom volumes of every pool, with their sizes and whether an instance uses each. |
| `create-volume` | Creates a custom volume in a pool, with an optional size. |
| `remove-volume` | Removes a custom volume. Incus refuses one that an instance uses. |
| `create-pool` | Creates a storage pool with the driver `dir`, `zfs`, `btrfs` or `lvm`. The last three take a size for their loop file. |
| `remove-pool` | Removes a storage pool. Incus refuses one that still holds something. |
| `remove-image` | Removes a cached image, by its alias or the start of its fingerprint. |

The page also offers the server's pools and networks for `pool` and `network`.

## Limits

- A container needs nesting for its systemd to come up. The plugin turns on
  `security.nesting` for each container it makes.
- In a container on an Ubuntu server, Chromium can't sandbox itself in a user
  namespace. berth gives the Chromium that comes with Playwright its setuid
  sandbox helper instead. A Chromium that the agent installs itself has none,
  so it can't start sandboxed. A virtual machine doesn't have this limit.
- A virtual machine needs KVM on the server. Without it Incus refuses to start
  the machine.
- The server needs to reach `https://images.linuxcontainers.org` to download the
  image the first time.
- berth reaches the machine through its tailnet once it's set up. The address
  Incus reports is only the first way in.
