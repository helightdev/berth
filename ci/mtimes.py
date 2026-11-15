#!/usr/bin/env python3
"""Make a target directory from GitHub's cache count for a fresh checkout.

cargo tells a changed source by its time, and a checkout gives every file
the time it was made, which is newer than any build in the cache: it would
build all of berth again. So the cache keeps the blob ids of the tree it was
built from, and after a restore every file whose blob is the same, and every
folder whose entries are the same, gets a time long past. What changed keeps
the time of the checkout and is built again.

    ci/mtimes.py save TARGET      after a build, before the cache is saved
    ci/mtimes.py restore TARGET   after the checkout and the cache's restore
"""
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# Older than any build: 2001-09-09.
PAST = 1_000_000_000
NAME = '.berth-ci-tree'


def tree():
    listed = subprocess.run(['git', 'ls-tree', '-r', '-z', 'HEAD'], cwd=ROOT, check=True,
                            capture_output=True, text=True).stdout
    found = {}
    for entry in filter(None, listed.split('\0')):
        meta, path = entry.split('\t', 1)
        found[path] = meta.split()[2]
    return found


def folders(paths):
    """Each folder of the tree with the names in it."""
    found = {'.': set()}
    for path in paths:
        parts = path.split('/')
        for depth in range(len(parts)):
            parent = '/'.join(parts[:depth]) or '.'
            found.setdefault(parent, set()).add(parts[depth])
    return found


def save(target):
    (target / NAME).write_text(''.join(f'{blob} {path}\n' for path, blob in sorted(tree().items())))


def restore(target):
    kept = target / NAME
    if not kept.is_file():
        print('No tree with the cache: everything is built', file=sys.stderr)
        return
    then = dict(reversed(line.split(' ', 1)) for line in kept.read_text().splitlines())
    now = tree()
    same = [path for path, blob in now.items() if then.get(path) == blob]
    for path in same:
        file = ROOT / path
        if file.is_file() and not file.is_symlink():
            os.utime(file, (PAST, PAST))
    before, after = folders(then), folders(now)
    for folder, names in after.items():
        if before.get(folder) == names:
            os.utime(ROOT / folder, (PAST, PAST))
    print(f'{len(same)} of {len(now)} files as the cache was built from them', file=sys.stderr)


if __name__ == '__main__':
    if len(sys.argv) != 3 or sys.argv[1] not in ('save', 'restore'):
        sys.exit(__doc__)
    {'save': save, 'restore': restore}[sys.argv[1]](Path(sys.argv[2]))
