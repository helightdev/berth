#!/usr/bin/env python3
"""Run the end-to-end tests, in order. The first test that needs the test
host applies its profile (harness.provisioned()), and the others use the host
it set up.

    ci/run.sh e2e                          all of them
    ci/run.sh e2e --shard update           one shard, as a CI job runs it
    ci/run.sh e2e e2e_serve                one module
    ci/run.sh e2e e2e_serve.Roles          one test case

Only meant for the runner container of ci/compose.yml.
"""
import argparse
import json
import os
import sys
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

# A new module goes here, in the place it needs: after the ones whose state it uses.
MODULES = [
    'e2e_apply',
    'e2e_host',
    'e2e_secrets',
    'e2e_local',
    'e2e_plugins',
    'e2e_serve',
    'e2e_client',
    'e2e_tailnet',
    'e2e_update',
    'e2e_own_key',
]

# CI runs each shard on a host of its own, at the same time. Every module
# belongs to one, and a shard keeps the order of MODULES. The applies take
# nearly all the time, so each shard holds about the same number of them.
SHARDS = {
    'update': ['e2e_update.Update'],
    'update-other': ['e2e_update.ShippedChanged', 'e2e_update.UpdateServed'],
    'tailnet': ['e2e_tailnet'],
    'own-key': ['e2e_own_key'],
    'rest': ['e2e_apply', 'e2e_host', 'e2e_secrets', 'e2e_local', 'e2e_plugins', 'e2e_serve', 'e2e_client'],
}


def check_shards():
    """That the shards hold every module, in its place, and nothing twice."""
    held = [name for names in SHARDS.values() for name in names]
    modules = [name.split('.')[0] for name in held]
    if len(set(held)) != len(held) or set(modules) != set(MODULES):
        sys.exit(f'The shards hold {held}, not the modules {MODULES}')
    for names in SHARDS.values():
        places = [MODULES.index(name.split('.')[0]) for name in names]
        if places != sorted(places):
            sys.exit(f'A shard is out of the order of MODULES: {names}')


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--shard', choices=SHARDS, help='run the tests of this shard')
    parser.add_argument('--shards', action='store_true', help='print the names of the shards as JSON')
    parser.add_argument('names', nargs='*', help='modules or test cases, as unittest names them')
    args = parser.parse_args()
    check_shards()
    if args.shards:
        print(json.dumps(list(SHARDS)))
        return
    if os.environ.get('BERTH_E2E') != '1':
        sys.exit('The end-to-end tests run in their containers: ci/run.sh e2e')
    # Imported only here, since it makes a scratch directory.
    import harness  # noqa: PLC0415
    if not harness.BERTH.is_file():
        sys.exit(f'No {harness.BERTH}: build berth first')
    names = (SHARDS[args.shard] if args.shard else []) + args.names or MODULES
    loader = unittest.TestLoader()
    suite = unittest.TestSuite(loader.loadTestsFromName(name) for name in names)
    started = time.monotonic()
    try:
        result = unittest.TextTestRunner(verbosity=2).run(suite)
    finally:
        harness.cleanup()
    print(f'End-to-end tests took {time.monotonic() - started:.0f}s')
    sys.exit(0 if result.wasSuccessful() else 1)


if __name__ == '__main__':
    main()
