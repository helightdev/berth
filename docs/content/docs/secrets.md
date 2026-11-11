---
title: Secrets and registries
description: "Tokens and registry logins: where they are kept and how a host gets them."
---

Two things a host usually needs besides software: tokens, and logins for the
places packages and images come from. berth keeps both on your machine and
sends them to a host when you say so.

## Where things are kept

| What | Where | Notes |
| --- | --- | --- |
| Registries | `~/.config/berth/config.json` | addresses and user names, no passwords |
| Secret values and registry passwords | `~/.config/berth/secrets.json` | readable by you only (mode 0600), not encrypted |
| Which secrets and registries a host gets | the host's profile | names only |

On the host everything belongs to the agent account and is stored in plain
files there. The agent can read what you send it; that is the point. Give it
tokens that are limited to what it should do, and let them expire.

## Secrets

In berth: the **Secrets** tab of the home screen (key 4) holds the values. Enter
on "+ Add secret" asks for a name and then the value; Enter on a secret
replaces its value; `x` twice removes it. Values are never shown.

Which host gets what is chosen on that host's **Secrets** page (tab 3 of its
set-up pages): registries are switched on or off, and for a secret you type
how the host gets it, a variable name such as `GH_TOKEN` or a file path such as
`~/.config/app/token`. Then "Secrets and registry logins" on the
overview sends them.

The same from the command line:

    berth secret set github          # asks for the value without showing it
    echo "$TOKEN" | berth secret set github
    berth secret                     # names
    berth secret remove github

Give one to a host as an environment variable or as a file:

    berth host ovh secret github env GH_TOKEN
    berth host ovh secret deploy-cert file ~/.config/acme/cert.pem 0600
    berth host ovh secret github off
    berth host ovh                   # what the host is given

Then send them:

    berth manage ovh secrets

or **Secrets and registry logins** (under Sync) on the host's overview. Each send
replaces what was installed before, so removing a secret and sending again
removes it from the host's environment. Files that were written stay; delete
them on the host.

Environment variables land in `~/.config/devbox/secrets.env` of the agent,
which `agent-env` and the agent's shells read, and in a second file that the T3
Code service reads. Programs that are already running, T3 Code included, do
not see a new value until they are restarted. A value with several lines has
to be a file.

### GitHub

Create a fine-grained token limited to the repositories the agent should work
on (Contents and Pull requests: read and write), store it and hand it over as
`GH_TOKEN`:

    berth secret set github
    berth host ovh secret github env GH_TOKEN
    berth manage ovh secrets

`gh` uses that variable, and berth sets `gh` as git's credential helper for
github.com, so `git clone`, `git push` and `gh pr create` work for the agent
without a sign-in. Protect branches on GitHub's side; the token is the only
limit on what the agent can push.

## Registries

A registry is a server with one address and one login. They are edited on the
**Registries** tab of berth's home screen (key 3), or with `berth registry`.

| Kind | What it is | Used for |
| --- | --- | --- |
| `nexus` | A Sonatype Nexus server, with repositories under it | whatever its repositories are for |
| `npm` | Any npm-compatible registry at an address | the agent's npm registry and its login |
| `raw` | An address files are uploaded to with HTTP PUT | publishing the release archive |
| `docker` | Any container registry, as `HOST` or `HOST:PORT` | pushing the container image, the agent's Docker login |

A registry with a user name logs in with user name and password. Without a user
name, what is stored is used as a token: `_authToken` for npm, a bearer token
for uploads.

On the tab, Enter changes the selected row (or switches the kind or format),
`x` removes a repository, a user name or a stored password, and `x` twice on a
registry's name removes the registry. `c` checks the selected registry: that the
server answers, that each repository exists and is readable with the stored
login, and that a Docker address speaks the Docker API. It only reads, so it
does not tell you whether the account may upload. The same from the command line:

    berth registry add nexus nexus https://nexus.example.com ci-user
    berth registry login nexus                      # asks for the password or token
    berth registry repo nexus npm npm-group
    berth registry repo nexus raw berth-releases
    berth registry repo nexus docker docker-hosted nexus.example.com:8083

    berth registry add verdaccio npm https://npm.example.com     # token login
    berth registry add files raw https://files.example.com/berth ci-user
    berth registry add ghcr docker ghcr.io my-user

    berth registry                                  # the list
    berth registry check                            # do they answer and accept the login?

Repositories of a Nexus server:

| Format | Address | Used for |
| --- | --- | --- |
| `npm` | `URL/repository/NAME/` | the agent's npm registry and its login |
| `raw` | `URL/repository/NAME/` | publishing the release archive |
| `docker` | `HOST/NAME`, or the address you give | pushing the container image, the agent's Docker login |

A Docker repository with path based routing in Nexus needs no address: images
are at `HOST/REPOSITORY/IMAGE`. With a connector on a port or host name of its
own, give that address. Maven is not handled yet.

### Giving a host a registry

On the host's Secrets page, or:

    berth host ovh registry nexus on
    berth manage ovh secrets

The agent's `~/.npmrc` gets the first npm repository as its registry with the
login (in a block berth manages; the rest of the file is kept), and
`~/.docker/config.json` gets the login of every Docker address.

### Publishing the release archive

A machine set up from a cloud seed downloads the release archive. With
"Cloud-init seed" chosen on a host's Install page, **Publish release to**
switches between the file servers and raw repositories you have configured, and
**Download with login** decides whether the machine logs in for the download.
Making the seed then uploads the archive and points the seed at it.

The same from the command line:

    berth publish nexus/berth-releases my-server
    berth publish files my-server

Either way the seed pins the archive's SHA-256, and the archive contains no
profiles, keys or secrets. How the new machine may download it is your choice:

- **Without a login** (the default): the repository has to allow anonymous reads.
- **With a login:**

      berth publish nexus/berth-releases my-server --login              # the login of nexus
      berth publish nexus/berth-releases my-server --login nexus-read   # another registry's

  The cloud seed then contains that login, in a file only root can read that
  is deleted after the download. Whoever can read the seed (your cloud
  provider's console, the machine's metadata service until it is removed) can
  read the login, so use an account that may only read that repository. A
  second registry entry for the same server with such an account does that.

The login is taken from this machine when the seed is rendered, so it is not
written into the host's profile.

### Publishing the container image

    berth image push nexus/docker-hosted          # NEXUS-DOCKER-ADDRESS/berth-desktop:latest
    berth image push ghcr v1                      # ghcr.io/berth-desktop:v1

pushes the image built by the Docker install method.
