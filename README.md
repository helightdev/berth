<img src="assets/logo.svg" alt="" width="72" align="right">

# berth

Set up and look after agent workstations: a spare laptop, a dedicated server or
VM, or a container, turned into an Ubuntu 26.04 **amd64** desktop that coding
agents work in and that you reach from your browser.

`berth` is the tool on your own machine. It keeps one profile per host, builds
the unattended installer or cloud seed for it (and writes the USB drive), installs
over SSH, and manages the host afterwards. The software it puts on a host is
called *devbox*, which is why that name appears in paths and settings there.

A host gets: XFCE on a persistent virtual display reached through noVNC, Firefox,
Chromium, LibreOffice, image/PDF/OCR tools, development toolchains, Claude Code,
T3 Code, Codex, Pi, passwordless sudo for the `agent` account, Tailscale, Cockpit,
unattended security updates and a small CLI/stdio-MCP desktop adapter. Every
install path runs the same Ansible role. A laptop additionally gets a physical
XFCE login. Graphics run on the host's GPU or on the CPU; see
[tools and graphics](docs/content/docs/tools.md) for that and for the tool inventory.

Optional extras such as the Unity CLI are [plugins](docs/content/docs/plugins.md): an npm-format package with
one or more features: packages, Ansible tasks and a guide for the agents. You can add your own.
Tokens and logins for Nexus, npm and Docker registries are handed to a host with
[secrets and registries](docs/content/docs/secrets.md).

**Status:** early. [Validation](docs/content/docs/validation.md) lists what has been run and
what still needs a target. Try a profile in the container or a VM before booting
the installer on real hardware: **the installer erases the disk it selects.**

## Install berth

Needs Linux, Rust 1.88+ and Python 3. Building an installer also needs `xorriso`
and `curl`; installing over SSH needs Ansible (`make tools` creates a local
virtualenv for it), and passwords are hashed with OpenSSL.

```bash
git clone URL berth && cd berth
make install     # ~/.local/bin/berth
berth
```

The command carries the provisioning source (the Ansible role, the bootstrap
scripts, the shipped plugins) and unpacks it into its data directory when it is
first needed, and again after an update. The binary is all there is to install,
so it can be copied to another machine as it is.

`make link` installs the command so that it uses the checkout instead, which is
what you want while changing things. `berth config source ""` goes back to the
built-in source.

| What | Where |
| --- | --- |
| Settings | `~/.config/berth/config.json` |
| Hosts, one private profile each, with generated passwords beside them | `~/.config/berth/hosts/NAME.json` |
| Provisioning source, unpacked from the command | `~/.local/share/berth/source/` |
| Build output: release archive, seeds, installer images | `~/.local/share/berth/build/` |
| Downloaded Ubuntu installer ISOs | `~/.local/share/berth/iso/` (setting `iso_dir`) |

`XDG_CONFIG_HOME` and `XDG_DATA_HOME` are honoured. `berth config` shows the
paths and settings; `berth config KEY VALUE` changes one, and so does the
Settings tab of the home screen. `d` on the host list makes the selected host the
default. Number keys switch tabs everywhere, and the mouse works throughout. The provisioning source
is looked for in `BERTH_SOURCE`, the `source` setting, the current directory and
then the built-in copy. Only the first two are for development; without them a
checkout in the current directory is used when berth is started inside one.

## Use it

```bash
berth                  # home screen: pick a host or create one
berth laptop           # open a host directly
berth list             # saved hosts and their addresses
```

Opening a host shows its overview: the SSH address, the things you can do with
the running host, and the way into its set-up. Set-up is a flow of its own with
three tabs; Esc returns to the overview.

| Set-up tab | For |
| --- | --- |
| Features | Which coding agents (Claude Code, Codex, Pi), T3 Code as their orchestrator, development tool categories, Playwright, CPU or GPU graphics |
| Accounts | Operator and agent usernames, SSH keys, passwords and sudo policy; fills in from the keys on this machine |
| Install | How to install, the settings that method needs, what is still missing, and Run |

A new host opens on Features; one with an SSH address opens on its overview.

### Install

| Method | What happens |
| --- | --- |
| USB installer | Builds an unattended installer from the Ubuntu Server ISO and writes it to a USB drive. F4 on the ISO field downloads the ISO and checks it against Ubuntu's `SHA256SUMS`. Choose the drive under *Then write to USB drive*, or press `w` when the build has finished |
| Cloud-init seed | Renders user data for a provider that takes cloud-init, plus the release archive to upload |
| Existing host over SSH | Runs the role on an Ubuntu 26.04 host you can already log in to |
| This machine or an inventory | Runs the role locally or against your own Ansible inventory |
| Container image | Builds the desktop as a Docker image for trying things out |

Run refuses to start while problems are listed above it. Builds run inside berth
with their output and a progress bar; Esc cancels and Enter returns. The two
Ansible methods use the normal terminal, where Ansible and SSH can ask for
passwords, and return to berth afterwards.

With a blank inventory, *This machine* provisions **the machine you are sitting
at**, and the tab says so. Only do that on a dedicated box.

### The overview

Give the SSH address (machines on your tailnet are suggested), then pick what to
do. Tasks ask for your sudo password on the host, show their output and stay on
the page.

| Group | Task | Does |
| --- | --- | --- |
| System | Status | Provisioning state, services, graphics renderer, disk space, failed units, and the access links |
| | Terminal | A shell on the host in a panel; Space switches between the agent and the SSH user |
| | Self-Test | `devbox-smoke` as the agent |
| | Update | `apt` update and upgrade; says whether a reboot is needed |
| | Reboot | Restarts the host |
| Access | Access links and desktop password | Links for the desktop, T3, Cockpit and SSH, and the virtual desktop's password |
| | Publish desktop and T3 on the tailnet | `tailscale serve` for both, and a trusted certificate for Cockpit |
| | Use T3 from another device | A one-time pairing link for a phone or another computer |
| Sync | Secrets and registry logins | Sends this host's secrets and logins to the agent account |
| Sign-In | Show who is signed in | The accounts of the agent that the host's plugins know about: Claude Code, Codex, GitHub, Unity |
| | Sign in to Claude Code, Codex, GitHub | From their plugins: runs the tool's sign-in on the host in a panel and shows its link; you sign in from your own browser |
| Set-Up | Apply this configuration | Runs the role again with this profile, for example after changing what is installed |

Sign-ins and the terminal run in a panel inside berth: a click on a web address
opens it on your machine, a right-click copies a word such as a one-time code,
Ctrl+V pastes, the wheel scrolls back and Ctrl+Q closes it.

The coding agents (Claude Code, Codex, Pi), T3 Code and GitHub's CLI are
plugins that come with berth, switched on per host on the Features page like
any other. [Plugins](docs/content/docs/plugins.md) put their own actions into these groups or
bring groups of their own, and add to Status, Self-Test, Update, the secrets
sync and the sign-in overview. The Unity plugin adds its sign-in to Sign-In, for example, and its
editors to Status.

### Without the UI

```bash
berth iso laptop                     # build the installer (downloads the ISO if none is set)
berth iso laptop --write /dev/sdX    # ...and write it to a USB drive
berth seed cloudbox                  # render the cloud-init seed
berth manage laptop status           # or access, publish, pair, smoke, update, reboot
berth manage laptop sign-in:claude   # or sign-in:codex, sign-in:github, sign-in:status
berth ssh laptop                     # a shell on the host
berth ssh laptop uptime              # or one command
berth ssh laptop --agent             # a shell as the agent account
```

### Keys

| Key | Action |
| --- | --- |
| Up/Down, Enter | Select a row; Enter edits text, flips a switch, cycles a choice or runs a task |
| Tab / Shift+Tab, 1-3 | Next / previous tab, or a tab by number (also accepts the current edit) |
| Right or F2, Left | Focus *Suggestions* (detected keys, hosts, choices) and back |
| F3 | Browse for a file on path fields. Ctrl+Enter picks the directory itself |
| F4 | Generate: a hostname, a password and its hash, a Tailscale key, the ISO download or its checksum |
| F5 | Type a password and store its hash; on Wi-Fi fields, unlock the saved passwords |
| Ctrl+S | Save the profile |
| Ctrl+U, Esc | Clear the field being edited, cancel the edit |
| x or Delete | Unset the selected text, path or secret field |
| q or Esc | From set-up back to the overview, from there to the home screen. Asks once more when there are unsaved changes |

The mouse works too: click tabs, rows, buttons and suggestions, scroll lists.

### What berth reads from your machine

- **SSH:** every `.pub` file in `~/.ssh`, keys held by ssh-agent, private key
  paths and `Host` aliases from the SSH configuration including `Include` files.
- **Tailscale:** the machines in `tailscale status`, as address suggestions.
  With `TS_API_KEY` set, F4 on the Tailscale key field creates a one-use
  enrollment key through the Tailscale API.
- **Wi-Fi:** saved network names. Their passwords only after F5 on a Wi-Fi field,
  which asks for your password, passes it to `sudo` once and keeps that
  authorisation alive while berth runs. berth never asks on its own.
- **Environment:** blank fields can be filled from `DEVBOX_HOSTNAME`,
  `DEVBOX_TARGET_DISK_SERIAL`, `DEVBOX_RELEASE_URL`, `DEVBOX_ISO`,
  `DEVBOX_ISO_SHA256` and `DEVBOX_INVENTORY`.

Generated passwords are written next to the profile as `NAME.credentials.json`
(mode 0600); the profile itself only stores hashes. An empty agent password locks
password login, and turning off the agent's passwordless sudo requires one.
Profiles hold Wi-Fi passwords and enrollment keys in clear text, so keep the
hosts directory private and out of version control.

### Install an existing host over SSH

On the Install tab, choose **Existing host over SSH**. Enter the hostname, IP address or SSH
config alias. Accepting the host fills blank user, port and private key fields
from OpenSSH's effective configuration. The firewall keeps SSH open for the
address you are installing from and for the recovery CIDR if you set one; other
access goes through the tailnet.

Choose whether to prompt for SSH and sudo passwords. Existing ssh-agent and SSH
config settings apply. berth generates
a temporary private Ansible inventory and provisions the selected host directly.
Install the local Ansible dependencies with `make tools` first. The target needs
Ubuntu 26.04 amd64, Python 3, SSH access and a login that can use sudo. OpenSSH's
host key verification applies. Complete first-time host verification through
`ssh YOUR_HOST` if required by your Ansible/OpenSSH setup.

Browser access uses [noVNC](https://github.com/novnc/noVNC#server-requirements).
TigerVNC supplies the virtual X display and its loopback RFB endpoint.
Websockify connects the browser to that endpoint. The TigerVNC password utility
generates the authentication file used by the server.

## Quick start: container desktop

On a machine with Docker Engine and Compose:

```bash
cd devbox
make docker-build
make docker-up
docker compose -f container/compose.yml logs -f
```

Open **http://127.0.0.1:16080/vnc.html** for the desktop and
**http://127.0.0.1:13773/** for T3. Obtain the per-container VNC password:

```bash
docker compose -f container/compose.yml exec desktop cat /home/agent/.config/tigervnc/password
make docker-test
```

The password is generated at runtime, never baked into the image. Ports are
published on host loopback only. If running Docker on another machine, tunnel
those ports with SSH/Tailscale. Named volumes retain workspaces/artifacts;
credentials and the remaining container home are disposable. `make docker-down`
does not delete those volumes. Do not use this container as a full machine test.

For a smaller first build without agent binaries or Playwright browsers:

```bash
PLUGIN_FEATURES='[]' INSTALL_PLAYWRIGHT=false \
INSTALL_WEB_TOOLS=false INSTALL_NATIVE_TOOLS=false INSTALL_JVM_TOOLS=false \
INSTALL_DOTNET_TOOLS=false INSTALL_SCRIPTING_TOOLS=false \
INSTALL_SCIENTIFIC_TOOLS=false INSTALL_CHROME_TOOLS=false make docker-build
```

Firefox and the desktop are still included. A full build downloads substantial
packages; allow several GiB of disk space. No privileged container or host Docker
socket is needed. Docker's default seccomp profile does not let Chromium or Chrome
create their sandbox, so browsers do not start in the container and the smoke
test lists that check as skipped. Use a VM or full host for browser work rather
than switching the sandbox off.

## Repository commands

```bash
make install        # berth for this user, provisioning source built in
make link           # berth for this user, using this checkout
make ui             # run berth from the checkout without installing
make tools          # local .venv, pinned ansible-core (internet needed)
make docs           # build the documentation site into docs/out (Node needed)
make prose          # Vale over the README and guides
make validate       # Python, shell and Rust checks; template tests need the .venv
make syntax         # Ansible syntax check
make release        # build/devbox-release.tar.gz and build/release.sha256
```

`make release` is deterministic for the same source files and uses an explicit
allowlist. It excludes local host config, secrets, virtualenvs, and build output.
Host-specific installer ISOs/seeds are private artifacts and must not be published.

## Provision an existing dedicated Ubuntu machine

No partitioning is performed by Ansible. Use only a dedicated supported host:
the role installs services, changes firewall policy, and grants host-wide sudo.

Local application **on that target**, not your everyday workstation:

```bash
make tools
make apply PROFILE=qemu ARGS=--ask-become-pass
```

For remote application, put inventory and variables in a private file, for example:

```yaml
all:
  hosts:
    agent-cloud:
      ansible_host: YOUR_SERVER_IP_OR_TAILNET_NAME
      ansible_user: ops
      devbox_profile: cloud
      devbox_bootstrap_ssh_cidr: YOUR_ADMIN_IP/32
```

```bash
.venv/bin/ansible-playbook -i secrets/hosts.yml ansible/site.yml --become
```

Profiles: `laptop`, `cloud`, `qemu`, `container`. The direct-apply default is
`qemu`; choose deliberately. Only the laptop role installs LightDM/power defaults.
Cloud networking and disks are preserved. The agent username and home are
configurable in Accounts; the UID is discovered rather than assumed.

Defaults are in [defaults/main.yml](ansible/roles/devbox/defaults/main.yml).
External binaries are installed as `agent`. T3 **0.1.8** uses a checked-in SHA256
from the upstream GitHub release; Claude **2.1.322** uses its official installer
with an explicit version. Update both version and checksum when changing T3.
Playwright **1.65.0** installs its matching Chromium. Ansible **2.21.6** supports
Ubuntu's Python 3.14; the build workstation needs Python 3.12 or newer for it.
Python artifact dependencies
use compatible ranges and OS packages track Ubuntu; this is not a bit-identical
fully frozen image. Claude's installer itself is obtained over HTTPS at provision
time. See [sources and qualification](docs/content/docs/validation.md).

## Enroll and access a full host

Log in as the operator (`ops` by default), then switch to `agent` for application setup:

```bash
sudo -iu agent
claude
t3 service status
cat ~/.config/tigervnc/password
agent-env devbox-smoke
```

Complete Claude/Codex/Pi sign-in, T3's onboarding/device pairing, and any GitHub/browser
sign-ins. Installing software does not authenticate it. Test `sudo -n true` through
a command actually launched by T3, too.

With a Tailscale enrollment key in the profile the host joins the tailnet during
provisioning and publishes the desktop and T3 there (`devbox_tailscale_serve`,
on by default; it needs HTTPS certificates enabled for the tailnet). Without a
key, enroll by hand and publish from the host's overview or:

```bash
sudo tailscale up
sudo tailscale serve --bg --https=443 http://127.0.0.1:6080
sudo tailscale serve --bg --https=8443 http://127.0.0.1:3773
```

Enable tailnet HTTPS if requested. Open `https://HOST.TAILNET.ts.net/vnc.html`
and `https://HOST.TAILNET.ts.net:8443/`. Keep T3 pairing enabled and use the
tailnet HTTPS address as the route rather than a loopback pairing address.
No Funnel/public proxy is configured.

### SSH logins

`devbox_harden_ssh` (SSH hardening on the Install tab, on by default) makes the SSH
server accept keys only and refuse root: `PasswordAuthentication no`,
`KbdInteractiveAuthentication no` and `PermitRootLogin no` in
`/etc/ssh/sshd_config.d/00-devbox.conf`. It is applied only when a key-based
login for a non-root account will exist afterwards, meaning the operator has an
SSH public key in the profile or the account used for provisioning has its own
authorized keys; otherwise the run says so and leaves the server as it was.
Passwords remain in use for the console, the laptop's graphical login, Cockpit
and `sudo`. Root has no password and is not managed by the role. Generated
account passwords are in `PROFILE.credentials.json` next to the profile.

### Cockpit

Full hosts get the [Cockpit](https://cockpit-project.org/) web console by default
(`devbox_install_cockpit`, or the Cockpit switch on the Install tab): logs, systemd
services, storage, software updates, a file browser and a terminal, plus network
settings on laptops. It listens on port 9090 with a self-signed certificate and
the firewall only admits it from the tailnet:

```
https://HOST.TAILNET.ts.net:9090
```

Without a tailnet, tunnel it: `ssh -L 9090:127.0.0.1:9090 ops@HOST` and open
`https://127.0.0.1:9090`. Log in as the operator with its password, so give the
operator one in Accounts; Cockpit does not accept SSH keys, and the agent has no
password unless you set one. Switching the option off on an existing host stops
the console and closes the port but leaves the packages installed.

UFW allows management on `tailscale0`. Cloud provisioning requires an explicit
bootstrap SSH CIDR; remote Ansible also preserves the current SSH source when
available. Remove the temporary rule only after verifying tailnet access. Docker
published ports can bypass normal UFW input rules: publish project services on
loopback or configure Docker's forwarding policy explicitly.

Without tailnet enrollment, QEMU's host can tunnel through its forwarded SSH port:

```bash
ssh -p 2222 -L 16080:127.0.0.1:6080 -L 13773:127.0.0.1:3773 ops@127.0.0.1
```

## Desktop tools and future controller integration

As `agent`:

```bash
agent-env devbox-desktop screenshot --output /srv/agent/artifacts/screen.png
agent-env devbox-desktop click --args '{"x":300,"y":250}'
agent-env devbox-desktop type --args '{"text":"Hello"}'
agent-env devbox-desktop key --args '{"key":"ctrl+l"}'
agent-env devbox-desktop pause
agent-env devbox-desktop resume
```

Desktop input is serialized per call and pause blocks cooperating tools. It is
not a multi-task scheduler: only one GUI task should use the desktop at a time.
For MCP, merge `/etc/devbox/mcp.json` into the controller's supported config, or
use Claude's command on versions that support it:

```bash
claude mcp add --transport stdio --scope user devbox-desktop -- \
  /usr/local/bin/agent-env /usr/local/bin/devbox-desktop --mcp
```

The adapter supports screenshot, click, typing, key chords, scroll, and clipboard
read. GUI apps and T3-launched commands share `DISPLAY`, Xauthority and the user
session bus. Ordinary shell commands can use `agent-env` to enter that environment.
Instructions are linked from Claude, Codex and Pi's user instruction files without
overwriting existing text; see [tools and graphics](docs/content/docs/tools.md).
T3 remains usable for coding if the desktop fails.

## Build an unattended USB image

The builder needs Python 3 and `xorriso`. Validation on the build machine also
needs QEMU, OVMF, `cloud-image-utils`, and optionally Canonical's autoinstall validator.

1. Create the host in berth (`berth NAME`). Without berth, copy
   `config/example.json` to `config/local.json` (gitignored) and use the `make`
   targets below.
2. Fill in the operator SSH public key, a password hash (`mkpasswd -m sha-512`),
   hostname, and disk-selection mode. In the UI, choose the ISO action
   and select Disk selection on the Install tab. Serial mode uses the exact udev
   `ID_SERIAL` from `udevadm info --query=property --name=/dev/nvme0n1`.
   Auto mode detects the only eligible disk when the USB boots.
3. Keep `profile: laptop`. Wi-Fi defaults to off. Enable Wi-Fi on the Install tab to
   reveal the SSID, password and interface fields. Networks saved in
   NetworkManager on the build machine are offered for the SSID and bring their
   password along; the tab shows whether a typed password matches the saved one.
   Leave the interface blank:
   the installer finds the wireless interface on the target itself, and falls
   back to wired networking if there is none. JSON profiles use
   `wifi_enabled: true` and a `wifi` object with `ssid`, `password` and
   optionally `interface`.
   Wired DHCP stays configured either way. Without Tailscale enrollment or a
   recovery SSH CIDR, the installed machine only accepts SSH from the tailnet.
4. Optionally supply a short-lived one-use Tailscale key. The `tailscale`
   command cannot create keys, so the UI uses the Tailscale API: export an API
   access token from the admin console as `TS_API_KEY`, then F4 on the Tailscale
   enrollment key field creates a one-use, pre-approved key valid for 24 hours
   (`DEVBOX_TAILSCALE_TAGS=tag:devbox,...` for tailnets that require tags). Reusable keys and
   existing machine identities do not belong in installer images.
5. Download an official Ubuntu 26.04 Server amd64 ISO. The UI computes its
   SHA256 when you choose the file, and the builder prints it when you pass
   none. Compare that value with Ubuntu's signed `SHA256SUMS`; a checksum taken
   from the file itself only proves the file has not changed since.

```bash
make iso CONFIG=config/local.json \
  ISO=/path/to/ubuntu-26.04-live-server-amd64.iso \
  ISO_SHA256=THE_VERIFIED_64_CHARACTER_SHA256   # optional: fail if the ISO differs
```

Result: `build/HOSTNAME-autoinstall.iso`. The builder refuses to replace an
existing image unless it is given `--overwrite`; the UI always rebuilds it.

To put it on a USB drive, press `w` when the build has finished in the UI. It
asks which drive when several are connected, asks you to confirm erasing it,
asks for your `sudo` password and writes the image with a progress bar. Choosing
a drive under *Then write to USB drive* above Run beforehand does the
same straight after the build. Outside the UI:

```bash
sudo bin/write-usb.sh build/HOSTNAME-autoinstall.iso /dev/sdX
```

The script only accepts a whole USB disk that is large enough, refuses one with
a system mount on it and unmounts anything else on it first.

**Booting the installer erases the selected disk.** The installer excludes
mounted, removable, USB and read-only disks. It stops if selection is ambiguous.
The disk is selected on the target during early commands, and the storage
configuration is then pinned to its detected path.

Disk-selection modes are configured with `target_disk_mode`:

| Mode | Selection |
| --- | --- |
| `serial` | Exact `target_disk_serial`; retained for older serial-based profiles |
| `auto` | The only eligible disk; default for new profiles |
| `model` | Match `target_disk_model` against udev ID_MODEL, with glob support |
| `path` | Match `target_disk_path`, with glob support and exact by-id symlink resolution |
| `largest` / `smallest` | Select by capacity among eligible disks |
| `ssd` / `hdd` | The only eligible non-rotating / rotating disk |

For `auto`, `model`, `path`, `largest` and `smallest`,
`target_disk_ssd` optionally filters candidates with `any`, `ssd` or `hdd`.
The UI shows relevant match fields and offers the mode values in Suggestions.
All modes require one final candidate, including when capacities tie. For example:

```json
{
  "target_disk_mode": "auto",
  "target_disk_ssd": "ssd"
}
```

Merge these values into your full host profile. They replace the need to specify
a serial. Disk selection applies to USB autoinstall. Cloud seeds and direct
Ansible deployment preserve the existing disk layout. Test the selected policy
in QEMU before booting a physical machine.

The ISO preserves the source image's boot configuration with `xorriso` replay,
patches GRUB for automatic install, and embeds the seed plus verified configuration
archive. It powers off after OS installation. Remove the USB and power on: the
first-boot service installs the remaining desktop/apps and retries transient
failures. Network access is required. Ubuntu firmware Secure Boot behaviour and
the customized ISO still need the VM/hardware qualification described below.

Progress and recovery:

```bash
sudo journalctl -u devbox-provision -f
cat /var/lib/devbox/provision-state
sudo systemctl start devbox-provision.service
```

`configured-needs-enrollment` is intentionally not a claim that agents are signed
in. A `ready` file marks successful Ansible convergence, not all acceptance tests.
The bootstrap checksum-checks the archive against the seed's expected digest.
The original private USB/seed may retain credentials even after installed copies
have been consumed; one-use enrollment keys limit that lifetime.

## Cloud deployment (Hetzner or another cloud-init provider)

Use a fresh Ubuntu 26.04 amd64 instance. Set `profile: cloud` and say where the
machine gets the **generic release archive** from. In berth, "Publish release
to" on the Install page offers the file servers and raw repositories of the
Registries tab and uploads the archive when the seed is made; see
[secrets and registries](docs/content/docs/secrets.md). Or set an HTTPS `release_url` and
upload it yourself.

`devbox_bootstrap_ssh_cidr` under `ansible_vars` limits SSH from outside the
tailnet to your network, for example `YOUR_ADMIN_IP/32`. Left out, a cloud host
accepts SSH from any address; keep SSH hardening on in that case, so only keys
work.

```bash
make cloud-seed CONFIG=config/local.json
```

Every seed build repackages the release, so the seed always names the digest of
the current source. Upload `build/devbox-release.tar.gz` unchanged to that URL. Supply the resulting
`build/HOST/cloud/user-data` during server creation. It is JSON-form YAML with the
required `#cloud-config` header. It embeds the expected archive digest, creates
operator access, and schedules the same provisioning service. Use a provider
firewall scoped to your admin IP for SSH; do not open VNC, T3 or Cockpit publicly.
The seed is limited to 32 KiB for Hetzner's console field.

This repository does not create/bill cloud resources or select a provider SKU.
Keep provider credentials on your deployment workstation. The cloud bootstrap
does not partition or format disks and retains the provider's default account.

## QEMU tests

Create a separate test host config with `profile: qemu` and virtual serial
`DEVBOX_TEST_DISK`. The udev identifier for the virtio disk should be checked in
the VM installer if it differs. Never put the Huawei disk serial in the VM seed.

Build a test ISO, then launch it with matching OVMF firmware files:

```bash
python3 bin/qemu.py install --directory build/vm-install \
  --image build/agent-qemu-autoinstall.iso \
  --code /path/to/OVMF_CODE.fd --vars /path/to/OVMF_VARS.fd
```

After installer poweroff, boot the installed disk without the ISO:

```bash
python3 bin/qemu.py boot --directory build/vm-install \
  --code /path/to/OVMF_CODE.fd --vars /path/to/OVMF_VARS.fd
```

For the cloud path, upload the release and generate a cloud seed with the qemu
profile and `devbox_bootstrap_ssh_cidr: 10.0.2.2/32`, then:

```bash
python3 bin/qemu.py cloud --directory build/vm-cloud \
  --image /path/to/ubuntu-26.04-cloudimg-amd64.img \
  --user-data build/agent-qemu/cloud/user-data \
  --code /path/to/OVMF_CODE.fd --vars /path/to/OVMF_VARS.fd
```

To test the role itself without publishing a release, boot the cloud image with
a minimal user-data that only creates your login, then use *Existing host over SSH*
against `127.0.0.1` port 2222 with the `qemu` profile. Add `--headless` to run
without a window and get the guest's serial console on stdio.

The runner creates its own qcow2 disk and writable firmware variables and refuses
to overwrite an existing test directory. Defaults: 4 vCPU, 8 GiB RAM, port 2222
forwarded to guest SSH on host loopback. Add `--tcg` without KVM. Image filenames
and OVMF package paths vary: pass real downloaded files, not the example paths.

Acceptance: provision twice; reboot without login; reconnect to the same desktop;
run `agent-env devbox-smoke`; verify browser sandbox, T3-launched sudo and GUI
input; test wrong disk serial refusal; then test lid/AC/battery on the Huawei.

## Updates, persistence, and current boundaries

- Ubuntu security and Firefox vendor updates are automatic, with automatic reboot
  disabled. Tailscale and other package updates can be applied in maintenance.
- Claude self-update is disabled; change its version in defaults deliberately.
  T3 uses a pinned runtime. Reapplying does not restart already-running desktops
  or controller turns; restart user services during a maintenance window after
  changing versions, service environment, or unit files.
- The bootstrap retries incomplete provisioning; it does **not** pull mutable Git
  branches or automatically replace an already completed release. Use a deliberate
  Ansible application to update a machine in v0.1.
- Application data lives under `/home/agent` and `/srv/agent`. Back up both before
  upgrades. Restic is installed; no backup destination/credentials or scheduled
  backup are invented. T3 database backup/restore needs an application-consistent
  snapshot or a stopped service.
- Laptop power settings are defaults for fresh physical profiles. Existing XFCE
  user overrides can take precedence. Reboot after initial laptop setup. Automatic
  firmware updates, encrypted unattended boot, swap tuning, offline package
  bundles, and a transactional update/rollback controller are not in v0.1.
- An agent with unrestricted sudo can administer the entire host. Use a dedicated
  VM on a future shared homeserver. Separate operator/agent users organize sessions
  but do not isolate data from a root-capable agent.

See [validation status and sources](docs/content/docs/validation.md) for exact verification limits.

## License

MIT, see [LICENSE](LICENSE). The vendored `tui-input` crate under `cli/vendor`
carries its own MIT license.
