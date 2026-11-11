---
title: Validation and sources
description: "What has been checked, and how."
---

## Checked automatically

`make validate` runs, without network access:

- Python tests for disk selection and its refusal cases, profile validation,
  deterministic release packaging, USB and cloud seed generation, GRUB patching,
  the desktop MCP adapter, and (with the Ansible venv) the rendered shell,
  service and mise templates for every profile.
- `bash -n` over the bootstrap, container and role scripts.
- Rust tests for the terminal UI: which fields each deployment shows, edits
  reaching the profile, the pre-run checks in Review, SSH discovery, the
  generated inventory, and a rendered walkthrough from Deploy to Review.

`make syntax` runs the Ansible syntax check.

## Checked by hand

Run on 2026-11-11 against Ubuntu 26.04.1 amd64:

- **Container.** `make docker-build` runs the whole role with every feature on;
  `make docker-up` serves the XFCE desktop through noVNC and T3 on the published
  loopback ports; `make docker-test` passes. Under Docker's default seccomp
  profile Chromium cannot create its sandbox, so the smoke test reports the
  sandboxed browser launch as skipped there instead of turning the sandbox off.
- **Full host in QEMU/KVM.** `bin/qemu.py cloud` boots the Ubuntu cloud image, and
  the role is applied over SSH with the `qemu` profile, the path the UI's
  *Install over SSH* action takes. The install completes, a second apply changes
  nothing, `agent-env devbox-smoke` passes including the sandboxed browser and
  Cockpit, and after a reboot the desktop, noVNC, T3, Cockpit and Docker are up
  again without anyone logging in. The firewall keeps SSH open for the address
  the install came from.
- **Terminal UI.** The built binary was driven through a pseudo-terminal: set an
  operator password with F5, go to Review, run Save, and check the profile is
  written with mode 0600.

- **Unattended ISO in QEMU/KVM.** `bin/qemu.py install` boots the remastered ISO
  with disk mode `auto`: it selects the virtual disk, installs without input and
  powers off. The installed system has the configured hostname and operator key
  login, wired DHCP only (no wireless card in the VM, so the Wi-Fi section was
  dropped) and root-only bootstrap files. First-boot provisioning of that build
  then failed, which led to two fixes in `bootstrap/bootstrap.sh`, each checked
  on its own rather than with another full install: the service no longer
  orders itself after `cloud-final.service` (with the old unit systemd deleted
  the start job to break an ordering cycle; with the new one it runs at boot),
  and the playbook runs with umask 022 (under 077 the shared virtualenv in
  `/opt` was root-only and the agent could not run Playwright).
- **SSH hardening.** The hardening tasks applied to a fresh cloud-image VM:
  `sshd -T` reports password, keyboard-interactive and root logins off, key
  login still works, a password-only login is refused, and a second apply
  changes nothing.

- **A real laptop, start to finish.** A Huawei laptop with Intel Iris Xe
  graphics was installed from the USB installer written by berth: unattended
  install on Wi-Fi, first-boot provisioning without a failed task, tailnet
  enrollment from the profile's key, SSH hardening, and `devbox-smoke` passing.
  With software graphics off, the virtual desktop renders on the Iris Xe
  (`glxinfo` and `vulkaninfo` report it) after a re-apply over SSH and a reboot.
  The Manage tasks `status` and `publish` were run against it through berth.

## Still to qualify


- Secure Boot with the remastered image. Test the selected disk policy in
  QEMU before any physical machine.
- The cloud-init path end to end with a published release archive.
- Laptop-only behaviour: the LightDM login session by hand, and opening the lid
  again. With the lid closed the host stayed up, the virtual desktop kept its GPU
  and `devbox-lid` switched the panel off (`bl_power` 4); nobody has looked at the
  panel while this happens.
- The Manage tasks `smoke`, `update` and `reboot` through berth, the cloud seed
  from the `seed` command, and `berth iso --write`.
- Tailscale enrollment and Serve, and sign-in to the coding clients, which are
  interactive by design.

Package availability and external installers change over time. If a dependency
fails, provisioning records `failed`, retries from the timer, and never writes
the success marker.

## Primary references

- [Ubuntu autoinstall](https://canonical-subiquity.readthedocs-hosted.com/en/latest/reference/autoinstall-reference.html)
- [Autoinstall validation](https://canonical-subiquity.readthedocs-hosted.com/en/latest/howto/autoinstall-validation.html)
- [NoCloud datasource](https://docs.cloud-init.io/en/latest/reference/datasources/nocloud.html)
- [T3 v0.1.8 release](https://github.com/pingdotgg/t3code/releases/tag/v0.1.8):
  Linux x64 archive SHA256 `b9c49dc00c23fbbc5961bdaff2d88e556d96c8dbd638c4501e2937c70650a9e5`.
- [T3 installer at release commit](https://github.com/pingdotgg/t3code/blob/bf469bb19669671d0df2efda74ed73867a7137c0/scripts/install.sh)
- [T3 background services](https://github.com/pingdotgg/t3code/blob/bf469bb19669671d0df2efda74ed73867a7137c0/docs/user/background-service.md)
- [Claude Code installation](https://code.claude.com/docs/en/setup)
- [Claude Code v2.1.322](https://github.com/anthropics/claude-code/releases/tag/v2.1.322)
- [Playwright Python v1.65.0](https://github.com/microsoft/playwright-python/releases/tag/v1.65.0)
- [Ansible 2.21.6](https://pypi.org/project/ansible-core/2.21.6/)
- [Ansible Python support matrix](https://docs.ansible.com/projects/ansible/latest/reference_appendices/release_and_maintenance.html)
- [Mozilla native Firefox packages](https://support.mozilla.org/en-US/kb/install-firefox-linux)
- [Tool/runtime and graphics sources](tools.md#installation-references)
- [TigerVNC Xvnc](https://tigervnc.org/doc/Xvnc.html)
- [Tailscale Serve](https://tailscale.com/docs/reference/tailscale-cli/serve)

Docker comes from Ubuntu's packages rather than Docker's own repository, and the
cloud-init document is generated directly rather than from templates, which keeps
the seed builder dependency-free. A full package lock, release promotion, and
scheduled backups are not part of this version.
