#!/usr/bin/env python3
"""Run the end-to-end tests, in order: the first applies the test host's
profile, and the others use the host it set up.

    ci/run.sh e2e                          all of them
    ci/run.sh e2e --shard plain            the tests of one shard, on a host of their own, as CI runs them
    ci/run.sh e2e e2e_serve                one module
    ci/run.sh e2e e2e_serve.Roles          one test case

The tests of each test host run in a process of their own, at the same
time: the output of the first host comes as it happens, that of the others
when they are done. A shard is a run of the tests of one host that share
its states (SHARDS): CI runs each in a job of its own, on a fresh host,
where the first test case makes the state it needs. At the end each
prints how long each test and each test case took, slowest first, how many
playbook runs there were and which tests made them.

Only meant for the runner container of ci/compose.yml.
"""
import argparse
import glob
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

harness = None  # imported in the process of a host, which it is made for

# The tests of each test host, in the order they run: a module, or a test
# case of one. Applying is most of the time a run takes, so the cases that
# look at the same shared state of the host (see harness.py) follow each
# other, and that state is made once: first the host as provisioned() set it
# up, then extended(), plain(), served() and served_extended(). A new test
# case goes next to the ones that use the same state; one that is not named
# here stops the run.
HOSTS = {
    'studio': [
        # As provisioned() applied it.
        'e2e_apply.Apply',
        'e2e_host',
        'e2e_secrets',
        'e2e_local',
        'e2e_controller.OnAWorkstation',
        # Another berth adopts the host, applies the profile it kept and goes again.
        'e2e_adopt.Adopt',
        # extended(): three plugins and the tailnet, applied twice.
        'e2e_apply.Again',
        'e2e_plugins.Plugin',
        'e2e_tailnet.AutomaticKey',
        'e2e_update.Update',
        'e2e_update.ShippedChanged',
        # plain(): without them, joined by hand by an older berth.
        'e2e_plugins.PluginRemoved',
        'e2e_tailnet.JoinedByHand',
        # served(): berth serve, applying nothing.
        'e2e_serve.Routes',
        'e2e_serve.Portal',
        'e2e_serve.Roles',
        'e2e_serve.SetUp',
        'e2e_serve.ClosedSetUp',
        'e2e_client.Client',
        'e2e_client.AccessToken',
        # served_extended(): applied by the server, with a plugin and the tailnet.
        'e2e_tailnet.ThroughTheServer',
        'e2e_serve.ApplyJob',
        'e2e_client.ClientApply',
        'e2e_update.UpdateServed',
        # Its own berth serve on the same host, restored from its store: applies nothing.
        'e2e_restore',
    ],
    # The bare host first: the ways berth logs in, and berth's controller
    # alone, which leave it as they found it. Then a host that only ever
    # takes berth's own key. The provisioners' tests and the one of a store
    # of berth 0.5.2 need no host at all. Last, a node, which stays one.
    'dock': ['e2e_access', 'e2e_controller.Controller', 'e2e_own_key', 'e2e_provisioners',
             'e2e_update.FromBerth052', 'e2e_node'],
}


# The tests of each host cut into shards, in the order HOSTS has them. Each
# shard starts from a fresh host, so it pays for the first apply and the
# states it uses; one state is never cut in two. The shards `studio` and
# `dock` are the tests that make no apply but the first: ci/plan.py runs them
# for any change to berth, and the others only for changes to what the role
# is given (its SEAM) on a pull request.
SHARDS = {
    'studio': ['e2e_apply.Apply', 'e2e_host', 'e2e_secrets', 'e2e_local', 'e2e_controller.OnAWorkstation',
               'e2e_serve.Routes', 'e2e_serve.Portal', 'e2e_serve.Roles', 'e2e_serve.SetUp',
               'e2e_serve.ClosedSetUp', 'e2e_client.Client', 'e2e_client.AccessToken', 'e2e_restore'],
    'adopt': ['e2e_adopt.Adopt'],
    'extended': ['e2e_apply.Again', 'e2e_plugins.Plugin', 'e2e_tailnet.AutomaticKey', 'e2e_update.Update',
                 'e2e_update.ShippedChanged'],
    'plain': ['e2e_plugins.PluginRemoved', 'e2e_tailnet.JoinedByHand'],
    'served-extended': ['e2e_tailnet.ThroughTheServer', 'e2e_serve.ApplyJob', 'e2e_client.ClientApply',
                        'e2e_update.UpdateServed'],
    'dock': ['e2e_access', 'e2e_provisioners', 'e2e_update.FromBerth052'],
    'controller': ['e2e_controller.Controller'],
    'own-key': ['e2e_own_key'],
    'node': ['e2e_node'],
}


def host_of_shard(shard):
    return next(host for host, names in HOSTS.items() if SHARDS[shard][0] in names)


def unsharded():
    """What is wrong with SHARDS: a test of HOSTS in none or in two, or a shard of two hosts."""
    problems = []
    for host, names in HOSTS.items():
        for name in names:
            holding = [shard for shard, listed in SHARDS.items() if name in listed]
            if len(holding) != 1:
                problems.append(f'{name} is in {len(holding)} shards')
    for shard, listed in SHARDS.items():
        if len({host_of(name) for name in listed}) != 1 or any(
                name not in HOSTS[host_of(name)] for name in listed):
            problems.append(f'shard {shard} is not the tests of one host in HOSTS')
    return problems

def host_of_name_in(shard, name):
    """Whether a module or test case named on the command line is of the shard."""
    return any(listed == name or listed.split('.')[0] == name.split('.')[0] for listed in SHARDS[shard])


def host_of(name):
    # A test case named on its own goes where it is named; a module where its cases are.
    for host, names in HOSTS.items():
        if name in names:
            return host
    module = name.split('.')[0]
    for host, names in HOSTS.items():
        if any(known.split('.')[0] == module for known in names):
            return host
    return next(iter(HOSTS))


class TimedResult(unittest.TextTestResult):
    """A result that keeps how long each test took, and each test case with
    its setUpClass and tearDownClass."""

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.tests = {}
        self.classes = {}
        self.applies = {}
        self._class = None
        self._seen = 0

    def _case(self, test):
        cls = type(test)
        name = f'{cls.__module__}.{cls.__qualname__}'
        # A failed setUpClass is reported as an _ErrorHolder, not a test of the case.
        return None if name.startswith('unittest.') else name

    def _switch(self, name, now):
        if self._class and self._class[0] != name:
            label, began = self._class
            self.classes[label] = self.classes.get(label, 0) + now - began
            self._class = None
        if name and not self._class:
            self._class = (name, now)

    def startTest(self, test):
        self._started = time.monotonic()
        super().startTest(test)

    def stopTest(self, test):
        self.tests[test.id()] = time.monotonic() - self._started
        # Runs in a setUpClass count for the first test of its case.
        applied = harness.counts()['apply']
        if applied > self._seen:
            self.applies[test.id()] = applied - self._seen
            self._seen = applied
        super().stopTest(test)

    def finish(self):
        self._switch(None, time.monotonic())

    def report(self, stream):
        stream.writeln('\nTest cases, slowest first (with their set-up and tear-down):')
        for name, seconds in sorted(self.classes.items(), key=lambda item: -item[1]):
            stream.writeln(f'  {seconds:7.1f}s  {name}')
        stream.writeln('\nTests, slowest first (without the set-up of their case):')
        for name, seconds in sorted(self.tests.items(), key=lambda item: -item[1])[:25]:
            stream.writeln(f'  {seconds:7.1f}s  {name}')
        if len(self.tests) > 25:
            stream.writeln(f'  … and {len(self.tests) - 25} more')
        if self.applies:
            stream.writeln('\nPlaybook runs, by the test (or the set-up of its case) that made them:')
            for name, runs in self.applies.items():
                stream.writeln(f'  {runs:3d}  {name}')


class TimedSuite(unittest.TestSuite):
    """Marks when each test case begins, so the time of its setUpClass counts for it."""

    def _handleClassSetUp(self, test, result):  # noqa: N802 (unittest's name)
        if isinstance(result, TimedResult):
            result._switch(result._case(test), time.monotonic())
        return super()._handleClassSetUp(test, result)


def flatten(suite):
    for test in suite:
        if isinstance(test, unittest.TestSuite):
            yield from flatten(test)
        else:
            yield test


def unnamed():
    """The test cases of the modules here that HOSTS leaves out."""
    named = {name for names in HOSTS.values() for name in names}
    missing = []
    for path in sorted(glob.glob(str(HERE / 'e2e_*.py'))):
        module = Path(path).stem
        if module in named:
            continue
        for test in flatten(unittest.TestLoader().loadTestsFromName(module)):
            case = f'{module}.{type(test).__name__}'
            if case not in named and case not in missing:
                missing.append(case)
    return missing


def run_here(names, everything):
    """Run the tests in this process, against the host of BERTH_E2E_HOST."""
    global harness
    import harness  # noqa: PLC0415 (it reads the host from the environment)
    if not harness.BERTH.is_file():
        sys.exit(f'No {harness.BERTH}: build berth first')
    if everything and (missing := unnamed()):
        sys.exit('Not in HOSTS of tests/e2e/run.py: ' + ', '.join(missing))
    loader = unittest.TestLoader()
    suite = TimedSuite(flatten(loader.loadTestsFromName(name) for name in names))
    runner = unittest.TextTestRunner(verbosity=2, resultclass=TimedResult)
    started = time.monotonic()
    try:
        result = runner.run(suite)
        counted = harness.counts()
    finally:
        harness.cleanup()
    result.finish()
    result.report(runner.stream)
    if counted['tasks']:
        print('\nAnsible tasks that took longest, over all playbook runs:')
        for name, seconds in sorted(counted['tasks'].items(), key=lambda item: -item[1])[:10]:
            print(f'  {seconds:7.1f}s  {name}')
    print(f'\nEnd-to-end tests on {harness.HOST} took {time.monotonic() - started:.0f}s: '
          f'{counted["apply"]} playbook runs ({counted["apply_seconds"]:.0f}s), '
          f'{counted["workstation"]} workstations, {counted["serve"]} berth serve', flush=True)
    return result.wasSuccessful()


def main():
    if os.environ.get('BERTH_E2E_CHILD'):
        if os.environ.get('BERTH_E2E') != '1':
            sys.exit('The end-to-end tests run in their containers: ci/run.sh e2e')
        sys.exit(0 if run_here(sys.argv[1:], os.environ.get('BERTH_E2E_CHILD') == 'all') else 1)
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--shard', choices=SHARDS, help='run the tests of this shard only')
    parser.add_argument('--shards', action='store_true', help='print the names of the shards as JSON')
    parser.add_argument('--tests-of', choices=SHARDS, help='print the tests of this shard, one to a line')
    parser.add_argument('--host-of', choices=SHARDS, help='print the test host of this shard')
    parser.add_argument('names', nargs='*', help='modules or test cases, as unittest names them')
    args = parser.parse_args()
    if problems := unsharded():
        sys.exit('SHARDS of tests/e2e/run.py: ' + '; '.join(problems))
    if args.shards:
        print(json.dumps(list(SHARDS)))
        return
    if args.tests_of:
        print('\n'.join(SHARDS[args.tests_of]))
        return
    if args.host_of:
        print(host_of_shard(args.host_of))
        return
    if os.environ.get('BERTH_E2E') != '1':
        sys.exit('The end-to-end tests run in their containers: ci/run.sh e2e')
    names = args.names
    groups = {}
    for name in names:
        groups.setdefault(host_of(name), []).append(name)
    if not names:
        groups = {host: list(listed) for host, listed in HOSTS.items()}
    if args.shard:
        host = host_of_shard(args.shard)
        listed = [name for name in groups.get(host, []) if host_of_name_in(args.shard, name)] if names \
            else list(SHARDS[args.shard])
        groups = {host: listed}
    started = time.monotonic()
    running = []
    for host, listed in groups.items():
        if not listed:
            continue
        env = {**os.environ, 'BERTH_E2E_HOST': host, 'BERTH_E2E_CHILD': 'some' if names else 'all'}
        # The other hosts take the operator's keys from a folder of their own (ci/compose.yml).
        if host != next(iter(HOSTS)):
            env['BERTH_E2E_SEED'] = f'{env.get("BERTH_E2E_SEED", "/seed")}/{host}'
        output = None if not running else tempfile.TemporaryFile('w+')
        process = subprocess.Popen([sys.executable, __file__, *listed], env=env, stdout=output,
                                   stderr=subprocess.STDOUT if output else None)
        running.append((host, process, output))
    failed = []
    for host, process, output in running:
        if process.wait():
            failed.append(host)
        if output:
            output.seek(0)
            print(f'\n===== The tests on {host} =====\n{output.read()}', flush=True)
    print(f'\nEnd-to-end tests took {time.monotonic() - started:.0f}s'
          + (f'; they failed on {", ".join(failed)}' if failed else ''))
    sys.exit(1 if failed else 0)


if __name__ == '__main__':
    main()
