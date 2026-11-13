# Working on berth

berth sets up and looks after agent workstations. These are Ubuntu machines with a desktop, coding agents and the tools around them. This file is for coding agents working in this repository.

## Layout

| Path | What it is |
| --- | --- |
| `crates/berth/` | The `berth` program: terminal UI, commands and `berth serve` (`crates/berth/src/serve/`). In `serve/`, `actions.rs` has the actions of hosts and of the installation, `keys.rs` the key ring's routes, `provisioners.rs` and `resources.rs` those of provisioners and resources |
| `crates/berth-api/` | Types and the operations table shared by every program (`ops.rs` names each operation, its HTTP route and its role) |
| `crates/berth-core/` | Hosts, profiles, links to hosts, registries and secrets, and plugins. The store is in `store/`. `kept.rs` and `plugin_state.rs` write files from it, and `import.rs` takes in the files of older versions. `kept_profile.rs` is the profile a host keeps of itself and `berth adopt`. `runner.rs` runs every action as a job. `keys/` is the key ring and how berth logs in to hosts. `level.rs` says what a host can do at each level. `provisioner.rs` and `resource.rs` are the provisioners of plugins and what they made |
| `crates/berth-tui/` | The parts of the UI that run in a terminal and in a browser |
| `crates/berth-pty/` | Terminals for sessions |
| `crates/berthd/`, `crates/box/` | The controller on each host and the agent's own tool there |
| `crates/berth-page/` | The screens of the page of `berth serve`, shared by `berth-web` and `berth-remote`. `device.rs` is what a browser and a terminal each give them, and `portal.rs` the server's portal over it |
| `crates/berth-web/` | The page of `berth serve --web`, built for WebAssembly: the browser's side of `berth-page`'s device |
| `crates/berth-remote/` | `berth-remote`, the same screens in a terminal on Linux, macOS and Windows, for a `berth serve` on another machine |
| `crates/vendor/` | The vendored `tui-input` crate |
| `provision/` | The installer side of the provisioning source: the role in `ansible/`, first-boot provisioning in `bootstrap/`, the builder of seeds and releases in `bin/devbox.py` and the example profile in `config/`. berth names these paths in `layout` in `crates/berth-core/src/paths.rs` |
| `plugins/` | The plugins that come with berth, each an npm-format package with a `guide.md`. With `provision/` they make the provisioning source, which berth carries and unpacks with the same layout |
| `recipes/` | Set-ups that go with berth but are not installed by it. Each is a folder with a guide and a script. `recipes/README.md` lists them |
| `.agents/skills/` | Skills for coding agents that work with berth, such as installing it. `.claude/skills` links here |
| `container/` | The desktop image and berth's own image |
| `ci/` | The images and scripts that run the checks and the end-to-end tests in containers |
| `docs/` | The documentation site (Fumadocs). `docs/AGENTS.md` has notes for its Next.js version |
| `tests/` | Python tests of the provisioning source |

## Skills

Skills for agents are in `.agents/skills/`, each a folder with a `SKILL.md`. `.claude/skills` is a link to the same folder.

| Skill | Use it when |
| --- | --- |
| `install-berth` | Someone asks you to install or set up berth, on their own machine or on a server |

A new skill gets its own folder and a row here and in `.agents/skills/README.md`. Its text has to be true to the docs and the recipes' guides, and it has to pass vale.

## Installing berth for someone

- Follow the `install-berth` skill. It says which recipe in `recipes/` fits and gives its command.
- A recipe that erases disks needs the user's word first, for that server and those disks.
- Do not write an enrollment key or a password into a file in the repository.
- When a berth server is installed, give the user its address and a setup key from `berth setup-key`. Do not read or print the token in `serve.token`.

## Build and check

Use cargo for everything Rust. These are what CI runs:

```bash
cargo test --locked
cargo test --locked -p berth --features web
cargo fmt --check -p berth-api -p berth-pty -p berth-core -p berth-tui -p berth -p berthd -p box -p berth-page -p berth-web -p berth-remote
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --all-targets -p berth --features web -- -D warnings
cargo clippy --locked -p berth-web --target wasm32-unknown-unknown -- -D warnings
python3 -m unittest discover -s tests -v
vale --minAlertLevel=warning README.md docs/content/docs plugins/*/guide.md
```

Development and test builds embed quick builds of berthd, box and the page (the `host-dev` and `web-dev` profiles). Only `--release` embeds the release builds. The `web` feature needs the `wasm32-unknown-unknown` target. The Python tests skip their YAML checks without PyYAML, so install `provision/requirements.txt` in a virtual environment to run them all.

### In containers

With only Docker or Podman, `ci/run.sh` runs the same checks in an image that has the toolchain. CI uses it too.

```bash
ci/run.sh check [CHECK…]   # fmt, shell-syntax, ansible-syntax, vale, python-tests, rust-tests, clippy
ci/run.sh e2e [TEST…]      # the end-to-end tests, such as e2e_serve.Roles
ci/run.sh e2e --shard HOST # the tests of one test host, as a CI job runs them
ci/run.sh shell            # a shell in the dev container
ci/run.sh boot cloud|iso   # real installs in QEMU, with KVM; slow
```

The end-to-end tests in `tests/e2e/` use throw-away containers over SSH, with berth, berthd and `berth serve`. They run on a network with nothing else on it, in scratch directories. There are two test hosts, `studio` and `dock`, and the two run in parallel. The modules need different amounts of that setup:

| Tests | Need |
| --- | --- |
| `e2e_access` | A bare host with only SSH, which they leave as they found it |
| `e2e_controller.Controller` | The same host and berth's controller alone, with one playbook run |
| `e2e_provisioners` | A provisioner of a made-up plugin, and no test host. With Docker, `ci/run.sh` starts an engine on the test network for the Docker plugin's cases |
| `e2e_restore` | The host `studio` as it is, and a server of its own with an encrypted store. It leaves the host as it is |
| The rest | A full apply of the profile |

That engine starts with Docker when the run is complete, names `e2e_provisioners` or is the shard that holds it. `BERTH_CI_DOCKER` (`1` or `0`) forces it on or off, and `BERTH_CI_DIND_IMAGES` lists images of your own engine to load into it, as it cannot pull. The header of `ci/run.sh` has the rest.

`tests/e2e/run.py` says which test cases run on which host and in what order. Test cases that look at the same state of a host follow each other, so the state is applied once (see the shared states in `tests/e2e/harness.py`). A run ends with the time of each test and test case and the number of playbook runs. The test hosts' logs from a failed run are in `build/ci/logs/`.

CI builds berth once and runs the tests of each test host as a shard, in a job of its own on a machine of its own. Each shard is its host's list in `HOSTS` of `tests/e2e/run.py`, so no state of a host is applied twice. A test case in none of the lists stops the run before it starts.

`ci/run.sh boot` boots berth's cloud seed and its USB installer in QEMU, lets the first boot set the host up, and then runs `tests/e2e/boot_*.py` and part of the container suite against it. Each takes 10 to 15 minutes on a machine with KVM, so run it when a change touches provisioning, the role or first boot. CI runs both for changes to provisioning (the Boot workflow, on a hosted runner with KVM). Logs, timings and disk use are in `build/boot/`.

## API changes

- Add the operation to `crates/berth-api/src/ops.rs` with the role it needs. A test fails when a route has no operation or the other way round.
- The API documents in `docs/openapi/` are committed. Regenerate them with `BERTH_UPDATE_DOCS=1 cargo test -p berth -p berthd`.
- The pages under `docs/content/docs/api/host` and `api/serve` are generated from those documents. Do not edit them by hand.

### Actions

Give a new thing a host, the installation, a provisioner or a resource can do an action in the matching registry. It does not get a route of its own. The routes `hosts.act`, `actions.act`, `provisioners.act` and `resources.act` run it, and each action says whether it can run now and what role it needs.

| Subject | Where its actions are |
| --- | --- |
| A host | `HOST` in `crates/berth/src/serve/actions.rs`, besides the tasks its controller lists |
| The installation | `INSTALLATION` in the same file |
| A provisioner | `provisioner::actions` in `crates/berth-core/src/provisioner.rs`: its plugin's actions and berth's own |
| A resource | `resource::actions` in `crates/berth-core/src/resource.rs`: those of its kind and its plugin's |

An action that changes something runs as a job through `runner.rs`.

## Versions and releases

- Every program says `berth_api::VERSION`. Do not read `CARGO_PKG_VERSION` anywhere else. A test checks.
- A release is a pushed tag `vX.Y.Z`. The Build workflow builds with `BERTH_BUILD_VERSION` set from it, publishes the release and the image, then opens a pull request that sets the version on `main` (`Cargo.toml`, `Cargo.lock`, `docs/openapi/`). Merge it when its checks have passed. Do not change the version by hand.
- A tag with a suffix, such as `v0.4.0-rc.1`, makes a pre-release and leaves `main` alone.

## Conventions

- Commits follow Conventional Commits (`feat(serve): …`, `fix(role): …`, `docs: …`, `ci: …`). Write a short subject, and add a body only when it explains something the diff does not.
- A comment gives the reason for the code in a sentence, and neither code nor docs refer to plans or decisions made elsewhere.
- Documentation is written for people, in short sentences and with tables for lookups. It has to pass vale.
- Tests, screenshots and examples use made-up hosts such as `studio` and addresses from `192.0.2.0/24`, never a real machine or a real configuration.
- A test that starts `berth serve` or `berthd` sets `XDG_RUNTIME_DIR`, `XDG_CONFIG_HOME` and `XDG_DATA_HOME` to scratch directories, so it cannot touch the user's own.
- Secrets never go into argv, logs or the repository. They travel in private files or on standard input.
- An installation's state goes through the store (`berth_core::store`), not through files of its own under the configuration or data directory, so that the database and its key file restore it. A file that has to stay, such as `id_ed25519` or `serve.token`, is written from the store.
