---
title: Tools and graphics
description: "What is installed, how graphics work, and what an agent finds on the machine."
---

The default provision includes the development languages and utility categories in
[Boat's tools list](https://docs.boat.dev/machines/tools), checked 2026-11-11.
Only the explicitly requested coding clients are installed: Claude Code, T3 Code,
Codex and Pi. Boat's other agents, service-specific `boat`/`host` commands, and Lux
are excluded. Desktop automation uses this repository's existing adapter.

## Tool inventory

| Area | This devbox |
| --- | --- |
| JavaScript | Node 24 by default, Node 22 available; npm, Bun/bunx, Deno, pnpm |
| Python | Ubuntu Python, pip/venv/pipx, uv, artifact libraries and Playwright |
| Native | GCC/G++, Clang, CMake, Ninja, pkg-config, Go, Rust/Cargo/rustup |
| JVM | OpenJDK 21, Maven, Gradle, Kotlin/kotlinc, Scala/scalac, sbt |
| Other languages | Ruby/gem/Bundler, PHP/Composer, Erlang/Elixir, .NET 10 SDK, R |
| Containers | Docker and Compose on full hosts; agent belongs to the Docker group |
| Browser/media/desktop | Chrome, Firefox, Playwright Chromium, FFmpeg, xdotool, wmctrl, XFCE and the office/artifact tools |
| General CLI | Git, Git LFS, gh, ripgrep, jq, curl, SSH, tmux, archive/search/debugging tools |

Versions intentionally differ from Boat's Ubuntu 24.04 snapshot. We use Ubuntu
26.04 packages and **mise instead of nvm** to select multiple runtimes. No separate
nvm installation is needed. The test container does not run a nested Docker daemon
or mount the host Docker socket; use QEMU for Docker-on-devbox tests.

[Defaults](https://github.com/helightdev/berth/blob/main/ansible/roles/devbox/defaults/main.yml) hold package selections and
tool versions. mise is checksum-pinned; its configuration belongs to `agent` at
`~/.config/mise/config.toml`. Shims are on the shell, desktop and T3 service PATH.
Project-local mise configs can override global selections after the usual mise
trust step. Examples as `agent`:

```bash
mise ls
mise exec node@22 -- node --version
agent-env node --version
agent-env codex --version
agent-env pi --version
```

The resolved runtime inventory is `/var/lib/devbox/toolchains.json`. Node 22 and
.NET 10 currently use release selectors; clean installations may resolve newer
patches. Pin their full versions in `devbox_node_versions` and `devbox_mise_tools`
for closer reproduction. Ubuntu repositories, npm transitive dependencies and
Python ranges also remain mutable. This is a reproducible setup recipe, not a
complete artifact lock or offline image.

Codex **0.167.1** uses `@openai/codex`; Pi **1.5.1** uses
`@earendil-works/pi-coding-agent` with `--ignore-scripts`, following their official
installation instructions. They install under `~/.local` as `agent`. Version
changes are applied by rerunning Ansible. Authentication is separate: launch
`codex` or `pi` and complete the client's sign-in. Shared machine instructions are
referenced from `~/.claude/CLAUDE.md`, `~/.codex/AGENTS.md`, and
`~/.pi/agent/AGENTS.md`, preserving existing content.

Select development categories independently in the Features step:

| Category | Ansible setting | Docker build environment |
| --- | --- | --- |
| JavaScript runtimes | `devbox_tools_web` | `INSTALL_WEB_TOOLS` |
| Native development | `devbox_tools_native` | `INSTALL_NATIVE_TOOLS` |
| JVM development | `devbox_tools_jvm` | `INSTALL_JVM_TOOLS` |
| .NET development | `devbox_tools_dotnet` | `INSTALL_DOTNET_TOOLS` |
| Scripting languages | `devbox_tools_scripting` | `INSTALL_SCRIPTING_TOOLS` |
| Scientific computing | `devbox_tools_scientific` | `INSTALL_SCIENTIFIC_TOOLS` |
| Google Chrome | `devbox_tools_chrome` | `INSTALL_CHROME_TOOLS` |

All categories default to enabled. Anything beyond them, such as the Unity CLI,
is a [plugin](plugins.md). Node/npm, Python, GCC, CMake and Ninja remain
base dependencies. The category package lists, runtime tools and expected smoke
commands are defined together in `devbox_tool_categories`. Switching a category
off skips its optional packages and runtimes on a fresh installation. Existing
installed packages are retained when reapplying a profile.

Older profiles using `devbox_install_boat_tools` provide the default for categories
without an explicit selection. Older Docker invocations using `INSTALL_BOAT_TOOLS`
also provide the fallback for the individual build switches.

The coding agents, T3 Code and GitHub's CLI are plugins in `plugins/`:
`claude/cli`, `codex/cli`, `pi/cli`, `t3/server` and `github/cli`. A machine gets
the ones listed in `devbox_plugin_features` (`PLUGIN_FEATURES` for the
container), which lists all five unless set. berth turns the older switches
(`devbox_install_agents`, `devbox_agent_*`, `devbox_install_t3`) of a profile
into that list when it opens it. T3 Code's plugin installs it; the role still
runs it as a service and publishes it. Update on a host's overview also brings
Claude Code, Codex, Pi and T3 Code to their latest versions; the versions in
`devbox_claude_version`, `devbox_codex_version`, `devbox_pi_version` and
`devbox_t3_version` are the least ones, so applying the configuration again does
not go back. Playwright and software graphics have
their own toggles.

## Prompts an agent would not see

Some programs stop at a dialog on the desktop the first time they run, and an
agent working over SSH never notices. The role removes the common ones:

- **Keyring**: the agent gets a default keyring without a password, so Chrome,
  the Unity CLI and anything else using the Secret Service store credentials
  without asking. The secrets are unencrypted in `~/.local/share/keyrings`. An
  existing keyring is left as it is.
- **Chrome**: the terms-of-service dialog, the default-browser question and the
  welcome page are skipped, and usage statistics are off.

Signing in to an agent, to Unity or to a browser profile still needs a person.

## GPU-less machines

`devbox_software_rendering: true` is the default for the virtual agent session in
every profile. Mesa provides OpenGL through **llvmpipe** and Vulkan through
**lavapipe**. The latter's installed ICD filename is discovered rather than
hardcoded. No physical GPU, `/dev/dri` passthrough or privileged container is
required for these software paths.

TigerVNC already supplies the persistent virtual X server on `:1`. **Xvfb** is
also installed for isolated GUI jobs that should not share the agent desktop:

```bash
agent-env glxinfo -B
agent-env vulkaninfo --summary
agent-env xvfb-run -a -s '-screen 0 1280x720x24 +extension GLX' glxinfo -B
# With a separately installed and licensed Unity editor/project:
agent-env xvfb-run -a /path/to/Unity -batchmode -force-glcore \
  -projectPath /srv/agent/workspaces/example -quit -logFile -
```

The Unity command is an invocation example, not a Unity installation or validation.
It selects OpenGL explicitly; software Vulkan compatibility varies by application.
No HELIX environment variables are introduced. Compose uses `init: true` so Xvfb
jobs and application subprocesses run beneath an init process. With `docker run`,
also pass `--init`; do not make `xvfb-run` PID 1.

The render settings reach the desktop, D-Bus-activated GUI apps, T3 and commands
launched with `agent-env`. They do not change the laptop's physical login session.
To render on the host's GPU instead, set `devbox_software_rendering: false`
("Software graphics" off in the UI), reapply and reboot. The role then adds the
agent to the `render` and `video` groups, and the virtual display is started
with TigerVNC's `-rendernode` on the first usable `/dev/dri/renderD*`, so OpenGL
and Vulkan programs in the agent desktop run on that GPU through Mesa. Check
with `agent-env glxinfo -B`. This covers GPUs with Mesa drivers (Intel, AMD); it
does not set up NVIDIA's proprietary driver or GPU passthrough into a VM. CPU rendering is suitable for headless tools and tests;
performance and application support must be measured for your workload.

`agent-env devbox-smoke` fails on missing expected commands, verifies llvmpipe on
both the shared desktop and Xvfb, checks lavapipe enumeration, launches the coding
clients with `--version`, and runs the existing desktop/browser/artifact checks.
Graphics reports are saved alongside screenshots in the smoke artifact directory.
These are acceptance checks to run on the installed target, not claims that the
graphics stack has already been exercised in the authoring environment.

## Installation references

- [Codex CLI](https://learn.chatgpt.com/docs/codex/cli) and [release versions](https://learn.chatgpt.com/docs/changelog)
- [Pi installation](https://pi.dev/)
- [mise installation](https://mise.jdx.dev/installing-mise.html), [tool registry](https://mise.jdx.dev/registry.html), and [shims](https://mise.jdx.dev/dev-tools/shims.html)
- [Google repository signing](https://www.google.com/linuxrepositories/)
- [Mesa software-rendering settings](https://docs.mesa3d.org/envvars.html) and [llvmpipe](https://docs.mesa3d.org/drivers/llvmpipe.html)
