#!/usr/bin/env python3
"""Install what berth sends on standard input for the calling account.

The input is one JSON object:
  env     {NAME: value}                 written to ~/.config/devbox/secrets.env
                                        and secrets.service.env (for systemd)
  files   [{path, content, mode}]       files below the home directory
  npm     [{registry, auth | token}]    a managed block in ~/.npmrc
  docker  [{address, auth}]             entries in ~/.docker/config.json
Each run replaces what the previous one installed for env and npm. Values are
never printed.
"""
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
from pathlib import Path

HOME = Path.home()
BEGIN, END = '# berth: managed, do not edit below', '# berth: end'


def write(path, text, mode=0o600):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + '.berth-tmp')
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(descriptor, 'w') as stream:
        stream.write(text)
    os.chmod(temporary, mode)
    os.replace(temporary, path)


def main():
    data = json.load(sys.stdin)
    state = HOME / '.config/devbox'
    state.mkdir(parents=True, exist_ok=True)
    os.chmod(state, 0o700)

    env = data.get('env') or {}
    for name in env:
        if not re.fullmatch(r'[A-Za-z_][A-Za-z0-9_]*', name):
            sys.exit(f'Not a variable name: {name}')
        if '\n' in env[name]:
            sys.exit(f'{name} has more than one line; install it as a file instead')
    write(state / 'secrets.env',
          ''.join(f'export {name}={shlex.quote(value)}\n' for name, value in sorted(env.items())))
    # The same for services, in the form systemd reads.
    quoted = {name: value.replace('\\', '\\\\').replace('"', '\\"') for name, value in env.items()}
    write(state / 'secrets.service.env',
          ''.join(f'{name}="{value}"\n' for name, value in sorted(quoted.items())))
    print('Environment: ' + (', '.join(sorted(env)) or 'none'))
    if env:
        print('Programs that are already running, such as T3 Code, see it after a restart.')

    for entry in data.get('files') or []:
        path = Path(os.path.expanduser(entry['path']))
        if not path.is_absolute():
            path = HOME / path
        path = path.parent.resolve() / path.name
        if HOME.resolve() not in path.parents:
            sys.exit(f'Not below the home directory: {entry["path"]}')
        write(path, entry['content'], int(str(entry.get('mode') or '0600'), 8))
        print(f'File: {path}')

    npmrc = HOME / '.npmrc'
    lines = npmrc.read_text().splitlines() if npmrc.exists() else []
    if BEGIN in lines:
        end = lines.index(END, lines.index(BEGIN)) + 1 if END in lines else len(lines)
        del lines[lines.index(BEGIN):end]
    npm = data.get('npm') or []
    if npm:
        lines.append(BEGIN)
        lines.append(f'registry={npm[0]["registry"]}')
        for entry in npm:
            place = entry['registry'].split('://', 1)[1]
            if entry.get('auth'):
                lines.append(f'//{place}:_auth={entry["auth"]}')
            elif entry.get('token'):
                lines.append(f'//{place}:_authToken={entry["token"]}')
        lines.append(END)
    if npm or npmrc.exists():
        write(npmrc, ''.join(line + '\n' for line in lines))
    print('npm: ' + (', '.join(entry['registry'] for entry in npm) or 'unchanged'))

    docker = data.get('docker') or []
    if docker:
        config = HOME / '.docker/config.json'
        settings = json.loads(config.read_text()) if config.exists() else {}
        for entry in docker:
            settings.setdefault('auths', {})[entry['address']] = {'auth': entry['auth']}
        write(config, json.dumps(settings, indent=2) + '\n')
    print('Docker: ' + (', '.join(entry['address'] for entry in docker) or 'unchanged'))

    # git asks gh for the GitHub login, which reads the token from the environment.
    gh = shutil.which('gh')
    if gh and ('GH_TOKEN' in env or 'GITHUB_TOKEN' in env):
        key = 'credential.https://github.com.helper'
        subprocess.run(['git', 'config', '--global', '--replace-all', key, ''], check=True)
        subprocess.run(['git', 'config', '--global', '--add', key, f'!{gh} auth git-credential'],
                       check=True)
        print('git: GitHub login through gh')


if __name__ == '__main__':
    main()
