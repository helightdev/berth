SHELL := /bin/bash
PYTHON ?= python3
CONFIG ?= config/local.json
PROFILE ?= qemu
INVENTORY ?= localhost,
CONNECTION ?= local
ISO ?=
ISO_SHA256 ?=

# Template tests need Jinja and PyYAML, which the Ansible venv provides.
ANSIBLE_PLAYBOOK := $(if $(wildcard .venv/bin/ansible-playbook),.venv/bin/ansible-playbook,ansible-playbook)
TEST_PYTHON := $(if $(wildcard .venv/bin/python),.venv/bin/python,$(PYTHON))

.PHONY: help ui install link tools docs docs-dev prose validate syntax release render cloud-seed iso apply docker-build docker-up docker-down docker-test
help:
	@printf '%s\n' 'ui: terminal UI for configuring and running every deployment' 'install: install berth for this user' 'link: install berth and make it use this checkout' 'tools: local Ansible venv' 'validate: offline tests and shell checks' 'docs: build the documentation site into docs/out' 'docs-dev: serve it while writing' 'prose: Vale over the README and guides' 'syntax: Ansible syntax check' 'release: reproducible config tarball' 'render: USB seed from CONFIG' 'cloud-seed: cloud-init user-data from CONFIG' 'iso: build installer from ISO (ISO_SHA256 checks it)' 'apply: provision dedicated Ubuntu (PROFILE, INVENTORY, CONNECTION)' 'docker-build/up/test/down: container desktop'
ui:
	cargo run --locked --manifest-path cli/Cargo.toml -- '$(CONFIG)'
# Install berth for this user. The command carries the provisioning source and
# unpacks it into its data directory, so the checkout is not needed afterwards
# and the binary alone can be handed on. Profiles in config/ are
# copied to berth's hosts directory once. `make link` instead makes the
# installed command use this checkout, which is what you want while changing it.
BERTH := $(HOME)/.local/bin/berth
BERTH_DATA := $(or $(XDG_DATA_HOME),$(HOME)/.local/share)/berth
BERTH_HOSTS := $(or $(XDG_CONFIG_HOME),$(HOME)/.config)/berth/hosts
install:
	cargo build --release --locked --manifest-path cli/Cargo.toml
	install -Dm755 cli/target/release/berth '$(BERTH)'
	@mkdir -p -m 700 '$(BERTH_HOSTS)'
	@for profile in config/*.json; do case "$$profile" in config/example.json|'config/*.json') ;; *) cp -n --preserve=mode "$$profile" '$(BERTH_HOSTS)/' ;; esac; done
	@cd / && '$(BERTH)' config
link:
	cargo build --release --locked --manifest-path cli/Cargo.toml
	install -Dm755 cli/target/release/berth '$(BERTH)'
	'$(BERTH)' config source '$(CURDIR)'
tools:
	$(PYTHON) -m venv .venv
	.venv/bin/pip install -r requirements.txt
# The site is Fumadocs; the manual pages are gathered from the README on each build.
docs:
	cd docs && npm install --no-audit --no-fund && npm run build
docs-dev:
	cd docs && npm install --no-audit --no-fund && npm run dev
# Errors fail it, warnings are shown. Styles are committed in .vale/styles; `vale sync` refreshes them.
prose:
	vale --minAlertLevel=warning README.md docs/content/docs/*.md plugins/*/guide.md
validate:
	$(TEST_PYTHON) -m unittest discover -s tests -v
	@for script in bin/*.sh bootstrap/*.sh container/*.sh ansible/roles/devbox/files/*.sh; do bash -n "$$script" || exit 1; done
	cargo test --locked --manifest-path cli/Cargo.toml
syntax:
	$(ANSIBLE_PLAYBOOK) -i localhost, ansible/site.yml --syntax-check
release:
	$(PYTHON) bin/devbox.py release
render:
	$(PYTHON) bin/devbox.py render --config '$(CONFIG)'
cloud-seed:
	$(PYTHON) bin/devbox.py render --kind cloud --config '$(CONFIG)'
iso: render
	$(PYTHON) bin/devbox.py iso --config '$(CONFIG)' --iso '$(ISO)' $(if $(ISO_SHA256),--sha256 '$(ISO_SHA256)')
apply:
	$(ANSIBLE_PLAYBOOK) -i '$(INVENTORY)' -c '$(CONNECTION)' ansible/site.yml --become -e devbox_profile='$(PROFILE)' $(ARGS)
docker-build:
	docker compose -f container/compose.yml build
docker-up:
	docker compose -f container/compose.yml up -d
docker-test:
	docker compose -f container/compose.yml exec -T desktop /usr/local/bin/devbox-smoke
docker-down:
	docker compose -f container/compose.yml down
