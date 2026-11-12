# Incus

This plugin creates the machine for a host on an Incus server. It offers two
provisioners:

| Provisioner | What it makes |
| --- | --- |
| `incus/vm` | A virtual machine. The server needs KVM. |
| `incus/container` | A system container. It starts faster and shares the server's kernel. |

Both start from an Ubuntu cloud image and set themselves up from berth's cloud
seed. Because the plugin copies the release into the machine itself, the profile
doesn't need a `release_url`. berth runs the plugin where berth runs and talks to the Incus API
over HTTPS. The `incus` command doesn't have to be installed there.

The machine is named `berth-HOST`, such as `berth-studio`. The plugin marks
it with `user.berth-host` and `user.berth-instance`. It lists and deletes only
machines with these marks.

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

## Arguments

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

## Limits

- A container needs nesting for its systemd to come up. The plugin turns on
  `security.nesting` for each container it makes.
- A virtual machine needs KVM on the server. Without it Incus refuses to start
  the machine.
- The server needs to reach `https://images.linuxcontainers.org` to download the
  image the first time.
- berth reaches the machine through its tailnet once it's set up. The address
  Incus reports is only the first way in.
