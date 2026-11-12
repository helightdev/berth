# Security

## Where berth is meant to run

berth manages machines with root access, so it matters where its parts can be
reached from.

**berth isn't built to face the open internet.** Keep it on a private network.
A tailnet, a VPN or a LAN you trust all work. If it has to be reachable from
further away, don't publish it directly. Use a reverse proxy with TLS and its
own authentication, or an API of yours that wraps berth.

For each part:

- **`berth serve`** gives whoever holds its token control of every host it
  knows: commands as root, files, secrets, power. With `--expose` it listens
  on plain HTTP, and the token is sent in the clear unless the network or a
  proxy encrypts it. The roles and OAuth 2.0 tokens narrow what a caller may
  do. They don't make the server safe to publish.
- **`berthd`** on a host listens on unix sockets only, and berth reaches it
  over SSH. Nothing of it is on the network.
- **The desktop, T3 Code and Cockpit** on a host are served on the host's
  private addresses. The desktop link contains its password, so treat the link
  as the password.
- **The agent account** is for coding agents, which run code they wrote
  themselves. Its sudo rights are set by the profile (`agent_privileges`).
  Don't put anything on a host that an agent on it shouldn't be able to read.
- **Secrets** are kept unencrypted on the machine that runs berth, in a file
  only its user can read, and are written to the agent's files on the hosts
  that take them.

A host is an agent's workspace and should be treated as one. Give it
credentials with the narrowest scope that does the job, and assume that what
is on it can leak.

## Supported versions

Fixes go into the latest release. berth is at 0.x and older releases aren't
maintained.

## Reporting a vulnerability

Please don't open a public issue. Use **Report a vulnerability** under the
repository's Security tab, which opens a private report to the maintainer.

Say what you found, how to reproduce it and what an attacker gains. You'll get
an answer within a week. Once a fix is released, the report is published with
credit to you, unless you'd rather not be named.

Things that follow from the section above aren't vulnerabilities. Examples are
the token giving full control, or plain HTTP being readable on a network you
exposed it to. Ways around those limits are, such as a caller with the `view`
role doing something only `admin` may, or a request getting in without the
token.
