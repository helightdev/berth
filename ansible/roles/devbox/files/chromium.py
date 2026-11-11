#!/opt/devbox-tools/venv/bin/python
import os
import sys
from pathlib import Path
from playwright.sync_api import sync_playwright

with sync_playwright() as p:
    executable = p.chromium.executable_path
profile = Path.home() / '.config/devbox-chromium'
os.execv(executable, [executable, '--no-first-run', f'--user-data-dir={profile}', *sys.argv[1:]])
