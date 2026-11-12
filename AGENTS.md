# Working on berth

berth sets up and looks after agent workstations. These are Ubuntu machines with a desktop, coding agents and the tools around them. This file is for coding agents working in this repository.

## Layout

| Path | What it is |
| --- | --- |
| `cli/` | The `berth` program: terminal UI, commands and `berth serve` (`cli/src/serve/`) |
| `crates/berth-api/` | Types and the operations table shared by every program (`ops.rs` names each operation, its HTTP route and its role) |
| `crates/berth-core/` | Hosts, profiles, links to hosts, registries and secrets, the store |
| `crates/berth-tui/` | The parts of the UI that run in a terminal and in a browser |
| `crates/berth-pty/` | Terminals for sessions |
| `host/berthd/`, `host/box/` | The controller on each host and the agent's own tool there |
| `web/` | The page of `berth serve --web`, built for WebAssembly |
| `ansible/`, `bootstrap/`, `bin/`, `config/` | The provisioning source that is applied to a host |
| `plugins/` | The plugins that come with berth, each an npm-format package with a `guide.md` |
| `docs/` | The documentation site (Fumadocs). `docs/AGENTS.md` has notes for its Next.js version |
| `tests/` | Python tests of the provisioning source |

## Build and check

Use cargo for everything Rust. These are what CI runs:

```bash
cargo test --locked
cargo test --locked -p berth --features web
cargo fmt --check -p berth-api -p berth-pty -p berth-core -p berth-tui -p berth -p berthd -p box -p berth-web
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --all-targets -p berth --features web -- -D warnings
cargo clippy --locked -p berth-web --target wasm32-unknown-unknown -- -D warnings
python3 -m unittest discover -s tests -v
vale --minAlertLevel=warning README.md docs/content/docs plugins/*/guide.md
```

Development and test builds embed quick builds of berthd, box and the page (the `host-dev` and `web-dev` profiles). Only `--release` embeds the release builds. The `web` feature needs the `wasm32-unknown-unknown` target. The Python tests skip their YAML checks without PyYAML, so install `requirements.txt` in a virtual environment to run them all.

### In containers

With only Docker or Podman, `container/ci.sh` runs the same checks in an image that has the toolchain. CI uses it too.

```bash
container/ci.sh check [CHECK…]   # fmt, shell-syntax, ansible-syntax, vale, python-tests, rust-tests, clippy
container/ci.sh e2e [TEST…]      # the end-to-end tests, such as e2e_serve.Roles
container/ci.sh shell            # a shell in the dev container
```

The end-to-end tests in `tests/e2e/` apply a profile to a throw-away Ubuntu container over SSH and then use it with berth, berthd and `berth serve`. They run on a network with nothing else on it, in scratch directories. The order of the modules is in `tests/e2e/run.py`. The test host's logs from a failed run are in `build/ci/logs/`.

## API changes

- Add the operation to `crates/berth-api/src/ops.rs` with the role it needs. A test fails when a route has no operation or the other way round.
- The API documents in `docs/openapi/` are committed. Regenerate them with `BERTH_UPDATE_DOCS=1 cargo test -p berth -p berthd`.
- The pages under `docs/content/docs/api/host` and `api/serve` are generated from those documents. Do not edit them by hand.

## Conventions

- Commits follow Conventional Commits (`feat(serve): …`, `fix(role): …`, `docs: …`, `ci: …`). Write a short subject, and add a body only when it explains something the diff does not.
- A comment gives the reason for the code in a sentence, and neither code nor docs refer to plans or decisions made elsewhere.
- Documentation is written for people, in short sentences and with tables for lookups. It has to pass vale.
- Tests, screenshots and examples use made-up hosts such as `studio` and addresses from `192.0.2.0/24`, never a real machine or a real configuration.
- A test that starts `berth serve` or `berthd` sets `XDG_RUNTIME_DIR`, `XDG_CONFIG_HOME` and `XDG_DATA_HOME` to scratch directories, so it cannot touch the user's own.
- Secrets never go into argv, logs or the repository. They travel in private files or on standard input.
