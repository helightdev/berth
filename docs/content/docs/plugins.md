---
title: Plugins
description: "Add optional software, guides and overview actions to a machine."
---

A plugin adds optional things to a machine. Each thing is a **feature**: packages,
set-up steps, a guide for the agents working there, and entries on the host's
overview in berth. A plugin can have several features, and each one is switched
on by itself. The Unity plugin in `plugins/unity/` has two, the CLI and an
editor, and is a good one to copy.

## The package

A plugin is a package in npm's format: a folder with a `package.json`, which
has a `berth` section next to the usual fields. That makes plugins something
`npm pack` and `npm publish` understand, so they can be shared through npmjs.com
or a private registry such as Nexus or Verdaccio.

    berth-plugin-unity/
      package.json
      guide.md
      tasks/cli.yml
      tasks/editor.yml

```json
{
  "name": "berth-plugin-unity",
  "version": "0.1.0",
  "displayName": "Unity",
  "description": "Unity CLI and editor for berth machines.",
  "keywords": ["berth-plugin"],
  "files": ["tasks", "guide.md"],
  "berth": {
    "features": [
      {
        "id": "cli",
        "group": "Development Tools",
        "label": "Unity CLI",
        "description": "Shown as help in berth.",
        "summary": "Unity CLI (`unity`) with its agent skills.",
        "packages": ["libgtk-3-0t64"],
        "commands": ["unity"],
        "tasks": "tasks/cli.yml",
        "guide": "guide.md",
        "actions": [
          {"name": "status", "label": "Sign-in and license", "run": "unity auth status"}
        ]
      }
    ]
  }
}
```

The plugin's id is the package name without its scope and without a leading
`berth-plugin-`, so `@acme/berth-plugin-unity` is `unity`. `berth.id` overrides
that. Ids and feature ids are lower-case letters, digits, `-` and `_`, starting
with a letter. `displayName` is the name berth shows.

### Features

| Field | Meaning |
| --- | --- |
| `id` | Required. The feature is referred to as `PLUGIN/ID`. |
| `group` | The group of the overview the action is listed in: `System`, `Access`, `Sync`, `Sign-In`, or a name of your own. Without it, a group named after the plugin. A sign-in belongs in `Sign-In`. |
| `label`, `description` | Name and help text in berth. |
| `summary` | One line for the machine's `/etc/devbox/AGENT.md`. |
| `packages` | Ubuntu packages to install. |
| `commands` | Commands the machine must have afterwards. They are added to `/etc/devbox/expected-commands`, which the self-test checks. |
| `tasks` | Path of an Ansible task list in the package. |
| `guide` | Path of a guide for agents in the package. |
| `actions` | Entries for the host's overview. |

Only `id` is required.

### Actions

| Field | Meaning |
| --- | --- |
| `name` | Identifier, unique within the plugin. |
| `label`, `help` | What berth shows. |
| `run` | Shell command to run on the host. |
| `user` | `agent` (default: as the agent, in its home, with the desktop environment of `agent-env`), `root` (through sudo) or `operator` (the SSH user). |
| `terminal` | `true` for a command that asks something or shows a link to follow, such as a sign-in. In the UI it gets a terminal of its own in a panel: keys and pastes go to the host, a click on a web address opens it on your machine, a right-click copies a word such as a one-time code, Ctrl+Q closes it. From the command line it runs in your terminal over `ssh -t`. Sign-ins should not open a browser on the host: start them with `env -u DISPLAY BROWSER=echo` so the tool prints its link. |
| `group` | The group of the overview the action is listed in: `System`, `Access`, `Sign-In`, or a name of your own. Without it, a group named after the plugin. A sign-in belongs in `Sign-In`. |
| `extends` | One task of berth, or a list: `status`, `smoke` (the Self-Test), `update`, `secrets` (the secrets sync) or `sign-in:status` (Show who is signed in). The action then has no row of its own. It runs after the task, every time, and its output follows the task's. Start its output with a line that names the plugin. |

The actions of the features that are on appear on the overview, in their group,
and show their output there. From the command line:
`berth manage HOST PLUGIN:ACTION`; `berth manage HOST status` includes what
plugins add to it. Actions without `terminal` have no terminal, so make them
non-interactive.

### Tasks

A plain Ansible task list, included while the role runs: after the base tools
and coding agents are installed, before the desktop is configured. Features are
applied in the order the plugin lists them, so a later one can rely on an
earlier one. Everything the role knows is available, most usefully:

- `devbox_user`, `devbox_home`: the agent's account and home
- `devbox_plugin_dir`: the package's directory, for files it ships
- `devbox_plugin_features`: every feature chosen for the machine
- `devbox_software_rendering`, `devbox_profile` and the other `devbox_` settings

A setting of your own, say a version, can be given per host in the profile's
`ansible_vars` as long as its name starts with `devbox_`; read it with a default:
`{{ devbox_unity_cli_version | default('1.0.0-beta.17') }}`.

Tasks must be safe to run again, because applying a configuration runs them
every time.

### Guide

Installed as `/etc/devbox/guides/PLUGIN-FEATURE.md` and linked from the
machine's `AGENT.md`. It is a template, so it can adapt to the machine:

    {% if devbox_software_rendering %}
    This machine renders on the CPU; pass `--software`.
    {% endif %}

Write it for an agent that has never seen the tool: what the command is, what
has to happen before first use, what needs a person, and what it must not ask
about again.

## Installing a plugin

| Location | For |
| --- | --- |
| `plugins/ID/` in the source | plugins shipped with berth: `claude`, `codex`, `pi`, `t3`, `github`, `unity` |
| `~/.config/berth/plugins/` | your own |

The Plugins tab of the home screen (`5`) lists both with their features, installs
one from a package name, a `.tgz` or a folder with Enter on "+ Add plugin", and
removes one of yours with `x` twice.

From the command line, your own can be put there in two ways:

    berth plugins add @acme/berth-plugin-foo     # from a registry
    berth plugins add ./foo-1.0.0.tgz            # a tarball from npm pack
    berth plugins add ~/src/berth-plugin-foo     # a folder

which runs `npm install --ignore-scripts --prefix ~/.config/berth/plugins`, so
registries and credentials come from your npm configuration. Or copy the folder
to `~/.config/berth/plugins/NAME/` by hand; npm is not needed for that.

`berth plugins` lists what is found and from where. A plugin of yours with the
same id as a shipped one replaces it. berth reads a plugin's `package.json` and
hands its tasks to Ansible; it never runs the package's npm scripts.

## Using one

Features are off until chosen. On the Features page of a host they are listed
in their groups next to the built-in entries. Selecting a group's heading and
pressing Space switches everything in it on, or off when all of it is on. The
choice is stored in the profile as

    "ansible_vars": { "devbox_plugin_features": ["unity/cli", "unity/editor"] }

and reaches a running host with **Apply this configuration**.

## How a machine describes itself

`/etc/devbox/AGENT.md` is generated on each machine from what was installed
there: the tool categories, the coding agents, whether rendering is on the CPU,
and one line per plugin feature. It stays short and points to
`/etc/devbox/guides/`, which holds `desktop.md`, `graphics.md` and the guides of
the features. The agents' own instruction files reference `AGENT.md`.

## Other ways of installing

- **ISO, USB seed, cloud seed:** the release archive contains the shipped
  plugins and, when built through berth, yours as well.
- **Container:** set `PLUGIN_FEATURES='["unity/cli"]'` for the build. Only
  plugins in the source's `plugins/` directory are in the build context.
- **Plain Ansible:** pass `devbox_plugin_features`, and `devbox_plugin_dirs`
  (`{"ID": "/path"}`) for plugins outside the source's `plugins/`.

## A minimal example

`~/.config/berth/plugins/hello/package.json`:

```json
{
  "name": "berth-plugin-hello",
  "version": "1.0.0",
  "displayName": "Hello",
  "berth": {
    "features": [
      {
        "id": "cowsay",
        "group": "Development Tools",
        "label": "cowsay",
        "description": "Installs cowsay and adds a greeting to the overview.",
        "summary": "`cowsay` is installed.",
        "packages": ["cowsay"],
        "commands": ["cowsay"],
        "actions": [
          {"name": "greet", "label": "Say hello", "run": "cowsay hello from $(hostname)"}
        ]
      }
    ]
  }
}
```

Switch it on under Development Tools, apply the configuration, and "Say hello"
appears on the host's overview.
