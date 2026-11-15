# Recipes

Set-ups that go with berth but are not installed by it. Each folder is one
recipe with its own guide and a script that runs it.

| Recipe | What it sets up |
| --- | --- |
| [`rescue-install`](rescue-install/) | Ubuntu on a server from its provider's rescue system, on two disks as a RAID 1. For when the provider's installer does not work. |
| [`berth-server`](berth-server/) | Turns a machine into a berth server, in a Docker container and on your tailnet with TLS. With `berth_server_node`, the server can then apply its own machine as a node. |

A recipe runs without berth. The playbooks use only the modules that come
with `ansible-core`, and share the tasks in `shared/`.

A server that hosts Incus and Docker machines is not a recipe: berth applies
it as a [node](../docs/content/docs/%28guide%29/use/nodes.mdx), whose features
install and update the engines and register their provisioners. A server the
earlier `incus-host` recipe set up is taken in as a node in the same way.

The recipes that set up a berth server end with a setup key. With it the
first user collects the server's token, with `berth login URL`, so the token never has to be read out or passed on.

## Get the recipes

A recipe needs only this folder. This fetches it into `recipes/` in the
current directory:

```bash
curl -fsSL https://github.com/helightdev/berth/archive/refs/heads/main.tar.gz \
  | tar xz --strip-components=1 --wildcards '*/recipes'
```

The recipes share the tasks in `shared/`, so fetch the folder as a whole.
