# Contributing to berth

Thanks for wanting to help. This page says what berth is trying to be, and how
a change gets from an idea to `main`. Coding agents working in the repository
should also read [AGENTS.md](../AGENTS.md).

## What berth is

berth sets up a machine for coding agents and manages it afterwards. It
installs a desktop, an agent account, and whatever a host's profile switches
on. Once the host runs, berth shows its status and handles its sign-ins,
updates, secrets and shells.

It's a tool at the level of a framework, and it's kept narrow on purpose:

- **Not an agent runner.** berth doesn't run agents, schedule their work or judge
  their output. It prepares the machine they run on. Orchestrators such as T3
  Code are things berth installs, and it doesn't try to become one.
- **Not a bundle.** The core is small, and we want it no larger. An agent, a
  toolchain or a service is a plugin, and a host only gets the ones its
  profile names. Whatever works as a plugin should be one, often in a
  repository of its own.
- **One host, described once.** A profile is a JSON file that says what a host
  is. Installing and applying again read the same file. A feature that only
  works by logging in to the host and changing things by hand doesn't fit.
- **Built from plain parts.** The installer is an Ansible role and the API is
  ordinary HTTP. We prefer the plain way over the clever one, so that someone
  can read what happens to their machine.

A change that makes berth do more is weighed against these points. Doing the
same with less is always welcome.

## Before you write code

**Open an issue first.** This goes for features, for changes in behaviour and
for anything larger than an obvious fix. Say what you want to do and why. The
scope should be small enough to review in one sitting. Several small issues are
better than one large one.

You don't have to wait for an answer before you start. Once the issue is
there, you can begin on the pull request straight away if you like. The order
is what matters, since the issue is where we find out whether the change
belongs in berth at all, and it's cheaper to find that out before the work is
done.

A few rules follow from that:

- A feature pull request with no issue behind it will be closed.
- Opening the pull request first and then filing an issue that summarises it
  doesn't count. The issue comes first.
- A first contribution always starts with an issue. Pull requests from
  accounts with no earlier issues or history in the project will be closed,
  however good the code is.

Typo fixes, broken links and small documentation corrections can go straight
to a pull request.

## Using language models

Using a model to help is fine, and welcome. Most of berth was written with
one. What we ask:

- **You review it.** Read every line of a change before you submit it, and be
  able to explain it. The maintainer reviews it as well, but that's the second
  review, and yours comes first.
- **Use a capable model.** Weak models produce code that looks right and
  isn't, and reviewing that costs more than writing it would have.
- **No unattended submissions.** Issues and pull requests opened by an agent
  with nobody reading them will be closed.
- **Prose passes Vale.** The repository has style rules that catch the usual
  machine phrasing in documentation. They have to pass, see below.

Issues and commit messages may be written with a model too. We'd ask you to
write the core of an issue yourself first, even if a model tidies it
afterwards. A short account in your own words tells us more than a formatted page.

## Making the change

Everything runs in containers, so Docker is all you need on your machine:

```bash
ci/run.sh check     # formatting, lints, unit tests, prose
ci/run.sh e2e       # end-to-end tests against a test host
ci/run.sh all       # both
```

`ci/run.sh shell` opens a shell in the development container. Building
without containers is described in the
[building reference](../docs/content/docs/%28guide%29/reference/building.mdx).

The container tests don't settle whether a change works. The test host is a
bare container that lacks systemd, a desktop and the real agents. A clean run
there covers only part of what berth does. Try your change on a real
deployment as well: a laptop, a server or a VM that berth installed. Most
contributors have one, and we test on ours before a release. Say in the pull
request what you tried it on.

A pull request is ready when:

- `ci/run.sh all` passes.
- You ran the change on a real host, when it touches anything a host does.
- Behaviour that changed has a test. For anything that touches a host, that
  means a case in `tests/e2e/`.
- Documentation under `docs/content/docs/` is updated in the same pull
  request, and Vale doesn't report warnings:

  ```bash
  vale --minAlertLevel=warning "docs/content/docs/(guide)" README.md AGENTS.md
  ```

- The terminal client and the page in a browser do the same things. A feature
  added to one is added to the other, with the shared part in
  `crates/berth-tui`.
- Commits follow [Conventional Commits](https://www.conventionalcommits.org/),
  with a short subject that says what changed.
- The pull request links its issue and stays within it. Leave unrelated
  cleanups for a pull request of their own.

## Reporting bugs

Open an issue with what you ran, what you expected and what happened. The
output of `berth --version` and of `berth run HOST status` helps. Take
addresses, tokens and passwords out before you paste anything.

Don't report security problems in an issue. [SECURITY.md](SECURITY.md) says
where they go.

## Licence

berth is licensed under either of the [Apache License, Version 2.0](../LICENSE-APACHE)
or the [MIT license](../LICENSE-MIT), at your option. By contributing you agree
that your contribution is licensed the same way, with no further terms.
