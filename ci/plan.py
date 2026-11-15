#!/usr/bin/env python3
"""What a run of CI has to do, and what it can skip.

Each unit of work (the Rust tests, clippy, the Python tests, each shard of
the end-to-end tests) has a key: a hash of the files in the commit it
depends on, by their git blob ids, with the week of the year so that what
floats outside the repository (base images, packages, the runner) is tried
again at least once a week. A unit that passed leaves a marker in GitHub's
cache under its key, and a later run with the same key skips it. A missing
or evicted marker only means the unit runs again.

A unit depends on every file except those it is known not to read, so a new
file or folder counts for all of them until it is ruled out here.

    ci/plan.py                 the plan as JSON, looking for markers with gh
    ci/plan.py --keys          the keys alone, without looking
    ci/plan.py --all           the plan with every unit to run

In CI it writes the plan to $GITHUB_OUTPUT. A pull request finds the markers
of its own runs and those of main.
"""
import argparse
import datetime
import hashlib
import json
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / 'tests/e2e'))
import run as e2e  # noqa: E402 (the shards of the end-to-end tests)

# Read by no unit: the documentation site, the other workflows, what agents
# read, and the files of the QEMU boots, which have a workflow of their own.
NOWHERE = ['docs/', '.agents/', '.claude/', '.github/', 'ci/boot', 'tests/e2e/boot']
# Except these, which some unit does read.
EVERYWHERE = ['.github/workflows/ci.yml']

# The Rust tests compare the API documents and two pages with the code.
RUST_READS = ['docs/openapi/', 'docs/content/docs/(guide)/reference/cli.mdx',
              'docs/content/docs/(guide)/use/upgrading.mdx']

# Where the Rust code decides what the role is given or what it finds on the
# host: the profile and its fields, applying, the controller and the agent's
# tool the role installs, keys and logins, plugins, nodes, seeds, updates and
# the tailnet. A pull request runs the shards that apply the role only for a
# change here (or to the role and the tests); main runs them for any change.
SEAM = [
    'crates/berth-api/', 'crates/berthd/', 'crates/box/', 'crates/berth-pty/',
    'crates/berth-core/build.rs', 'crates/berth-core/Cargo.toml',
    'crates/berth-core/tests/fixtures/ansible/',
    *(f'crates/berth-core/src/{name}' for name in (
        'apply.rs', 'rollout.rs', 'kept_profile.rs', 'node.rs', 'provision.rs', 'seed.rs', 'tailnet.rs',
        'keys/', 'plugins.rs', 'plugin_state.rs', 'paths.rs', 'profiles.rs', 'host_type.rs', 'runner.rs',
        'relay.rs', 'server_image.rs', 'import.rs')),
    'crates/berth/src/local/serve/actions.rs',
    'crates/berth-tui/src/fields.rs', 'crates/berth-tui/src/setup.rs', 'crates/berth-tui/src/flow.rs',
]

# What each kind of unit leaves out, beyond NOWHERE, and what it keeps of that.
UNITS = {
    'rust': dict(out=['recipes/'], keep=RUST_READS),
    'python': dict(out=['crates/', 'Cargo.toml', 'Cargo.lock', 'tests/e2e/']),
    # berth carries the provisioning source, so the end-to-end tests read
    # most of the tree; the Python tests and the recipes are not theirs.
    'e2e': dict(out=['recipes/', 'tests/test_', 'tests/fixtures/']),
    'role': dict(out=['recipes/', 'tests/test_', 'tests/fixtures/', 'crates/'], keep=SEAM),
}

# The shards that apply nothing but the first apply: they test berth's own
# code, so they run for any change berth is built from.
SMOKE = {'studio', 'dock'}


def files():
    listed = subprocess.run(['git', 'ls-tree', '-r', '-z', 'HEAD'], cwd=ROOT, check=True,
                            capture_output=True, text=True).stdout
    for entry in filter(None, listed.split('\0')):
        meta, path = entry.split('\t', 1)
        yield path, meta.split()[2]


def week():
    year, number, _ = datetime.date.today().isocalendar()
    return f'{year}-{number:02d}'


def keys():
    tree = list(files())
    found = {}
    for unit, rule in UNITS.items():
        out, keep = NOWHERE + rule.get('out', []), EVERYWHERE + rule.get('keep', [])
        digest = hashlib.sha256(f'{unit} {week()}\n'.encode())
        for path, blob in tree:
            if any(path.startswith(prefix) for prefix in out) and not any(path.startswith(k) for k in keep):
                continue
            digest.update(f'{blob} {path}\n'.encode())
        found[unit] = digest.hexdigest()[:20]
    return found


def markers(event, repo, number):
    """The keys of the markers this run can see: its own ref's and main's."""
    refs = ['refs/heads/main']
    if event == 'pull_request' and number:
        refs.append(f'refs/pull/{number}/merge')
    seen = set()
    for ref in refs:
        page = 1
        while True:
            listed = subprocess.run(
                ['gh', 'api', '-X', 'GET', f'repos/{repo}/actions/caches', '-f', 'key=green-', '-f', f'ref={ref}',
                 '-f', 'per_page=100', '-f', f'page={page}', '--jq', '.actions_caches[].key'],
                check=True, capture_output=True, text=True).stdout.split()
            seen.update(listed)
            if len(listed) < 100:
                break
            page += 1
    return seen


def plan(found, event, seen):
    def unit(name, key, also=()):
        return {'name': name, 'key': f'green-{name}-{key}', 'also': [f'green-{name}-{k}' for k in also]}

    # On main the role's shards run for any change berth is built from, and
    # leave the marker a pull request looks for too.
    role, role_also = (found['e2e'], [found['role']]) if event == 'push' else (found['role'], [])
    jobs = [unit('rust-tests', found['rust']), unit('clippy', found['rust']), unit('python-tests', found['python'])]
    shards = []
    for name in e2e.SHARDS:
        shard = unit(f'e2e-{name}', found['e2e']) if name in SMOKE else unit(f'e2e-{name}', role, role_also)
        shard.update(shard=name, host=e2e.host_of_shard(name))
        shards.append(shard)
    todo = lambda item: item['key'] not in seen  # noqa: E731
    result = {item['name']: json.dumps(item) if todo(item) else '' for item in jobs}
    result['shards'] = json.dumps([item for item in shards if todo(item)])
    result['skipped'] = ' '.join(item['name'] for item in jobs + shards if not todo(item))
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--keys', action='store_true', help='print the keys and stop')
    parser.add_argument('--all', action='store_true', help='run every unit, looking for no markers')
    args = parser.parse_args()
    found = keys()
    if args.keys:
        print(json.dumps(found, indent=2))
        return
    event = os.environ.get('GITHUB_EVENT_NAME', '')
    seen = set()
    if not args.all and event in ('push', 'pull_request'):
        seen = markers(event, os.environ['GITHUB_REPOSITORY'], os.environ.get('BERTH_CI_PR', ''))
    result = plan(found, event, seen)
    if output := os.environ.get('GITHUB_OUTPUT'):
        with open(output, 'a') as out:
            for name, value in result.items():
                out.write(f'{name}={value}\n')
    print(json.dumps({'keys': found, **result}, indent=2))


if __name__ == '__main__':
    main()
