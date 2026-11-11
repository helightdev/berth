#!/usr/bin/env python3
"""Create a VNC credential on the target, never in a reusable image."""
import os
from pathlib import Path
import secrets
import subprocess

def main():
    folder = Path.home() / '.config/tigervnc'
    folder.mkdir(parents=True, exist_ok=True, mode=0o700)
    password = folder / 'password'
    encoded = folder / 'passwd'
    if not password.exists():
        # VncAuth uses only eight characters. Tailnet/SSH protects the transport.
        value = ''.join(secrets.choice('abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789') for _ in range(8))
        fd = os.open(password, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, 'w') as stream:
            stream.write(value + '\n')
    if not encoded.exists():
        value = password.read_bytes()
        result = subprocess.run(['tigervncpasswd', '-f'], input=value, check=True, capture_output=True)
        fd = os.open(encoded, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, 'wb') as stream:
            stream.write(result.stdout)

if __name__ == '__main__':
    main()
