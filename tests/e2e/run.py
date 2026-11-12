#!/usr/bin/env python3
"""Run the end-to-end tests, in order: the first applies the test host's
profile, and the others use the host it set up.

    container/ci.sh e2e                          all of them
    container/ci.sh e2e e2e_serve                one module
    container/ci.sh e2e e2e_serve.Roles          one test case

Only meant for the runner container of container/ci.compose.yml.
"""
import os
import sys
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import harness  # noqa: E402

# A new module goes here, in the place it needs: after the ones whose state it uses.
MODULES = [
    'e2e_apply',
    'e2e_host',
    'e2e_secrets',
    'e2e_local',
    'e2e_plugins',
    'e2e_serve',
    'e2e_client',
]


def main():
    if os.environ.get('BERTH_E2E') != '1':
        sys.exit('The end-to-end tests run in their containers: container/ci.sh e2e')
    if not harness.BERTH.is_file():
        sys.exit(f'No {harness.BERTH}: build berth first')
    names = sys.argv[1:] or MODULES
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
