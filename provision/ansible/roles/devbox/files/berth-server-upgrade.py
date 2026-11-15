#!/usr/bin/env python3
"""Run the berth server in a container on this node from the image of another
version, with the data and the options of the running container.

    berth-server-upgrade CONTAINER        the version in BERTH_ARG_VERSION
    berth-server-upgrade CONTAINER VERSION

The new container gets the same name, mounts, published ports, restart
policy, groups, extra hosts, networks, user and environment as the old one, so
it keeps the server's data and what it reaches. An option this does not know
how to carry over stops it before anything changes. The old container is
renamed and stopped, not removed, until the new one runs; if the new one does
not come up, the old one is started again. Environment values go to docker in
a private file, never on its command line.
"""
import json
import os
import re
import subprocess
import sys
import tempfile
import time

DOCKER = os.environ.get('DEVBOX_DOCKER', 'docker')
VERSION = re.compile(r'^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$')
# How long the new container has to stay running before the old one goes.
SETTLE = float(os.environ.get('DEVBOX_UPGRADE_SETTLE', '10'))


class Refused(Exception):
    """A reason to change nothing, said in a sentence."""


def docker(*args, check=True, capture=True):
    done = subprocess.run([DOCKER, *args], text=True, capture_output=capture)
    if check and done.returncode != 0:
        raise Refused(f'docker {args[0]} {args[1] if len(args) > 1 else ""} failed: '
                      f'{(done.stderr or done.stdout or "").strip()}')
    return done


def inspect(kind, name):
    return json.loads(docker(kind, 'inspect', name).stdout)[0]


def repository(image):
    """An image reference without its tag or digest."""
    image = image.split('@', 1)[0]
    last = image.rsplit('/', 1)[-1]
    if ':' in last:
        image = image[:len(image) - len(last)] + last.split(':', 1)[0]
    return image


def version_key(text):
    numbers = re.match(r'^([0-9]+)\.([0-9]+)\.([0-9]+)', text or '')
    return tuple(int(part) for part in numbers.groups()) if numbers else None


# Options of a container that would change what it is if they were dropped,
# and which this does not carry over: one that is set refuses the upgrade.
UNCARRIED = {
    'Privileged': 'it runs privileged',
    'CapAdd': 'it has capabilities added',
    'CapDrop': 'it has capabilities dropped',
    'Devices': 'it has devices',
    'SecurityOpt': 'it has security options',
    'Sysctls': 'it has sysctls',
    'Tmpfs': 'it has tmpfs mounts',
    'VolumesFrom': 'it takes volumes from another container',
    'Links': 'it has links',
    'Dns': 'it has DNS servers of its own',
    'DnsSearch': 'it has DNS search domains of its own',
    'Ulimits': 'it has ulimits',
    'PidMode': 'it shares a process namespace',
    'IpcMode': None,
    'UTSMode': 'it shares a UTS namespace',
    'UsernsMode': 'it has a user namespace mode',
    'CpuShares': 'it has CPU shares',
    'CpusetCpus': 'it is pinned to CPUs',
    'MemoryReservation': 'it has a memory reservation',
    'MemorySwap': None,
    'PidsLimit': 'it has a limit of processes',
}


def run_options(container, old_image):
    """The options of `docker run` that make a container like this one, and
    its environment beyond what its image sets. Refused when it has one
    this does not carry over."""
    config, host = container['Config'], container['HostConfig']
    image_config = old_image.get('Config') or {}
    refusals = []
    for key, why in UNCARRIED.items():
        value = host.get(key)
        if key == 'IpcMode':
            if value not in (None, '', 'private', 'shareable'):
                refusals.append(f'it shares an IPC namespace ({value})')
            continue
        if key == 'MemorySwap':
            if value not in (None, 0, -1) and value != 2 * (host.get('Memory') or 0):
                refusals.append('it has a swap limit of its own')
            continue
        if value not in (None, '', 0, False, [], {}):
            refusals.append(why)
    runtime = host.get('Runtime') or 'runc'
    if runtime != 'runc':
        refusals.append(f'it runs under the runtime {runtime}')
    log = (host.get('LogConfig') or {}).get('Type') or 'json-file'
    if log != 'json-file' or (host.get('LogConfig') or {}).get('Config'):
        refusals.append('it logs otherwise than by default')
    if (config.get('Entrypoint') or None) != (image_config.get('Entrypoint') or None):
        refusals.append('its entry point is not its image\'s')
    if (config.get('Cmd') or None) != (image_config.get('Cmd') or None) and config.get('Cmd'):
        command = config['Cmd']
    else:
        command = []
    if (config.get('WorkingDir') or '') != (image_config.get('WorkingDir') or ''):
        refusals.append('its working directory is not its image\'s')
    if config.get('Healthcheck') and config.get('Healthcheck') != image_config.get('Healthcheck'):
        refusals.append('it has a health check of its own')
    networks = list(((container.get('NetworkSettings') or {}).get('Networks') or {}).keys())
    mode = host.get('NetworkMode') or 'default'
    if len(networks) > 1:
        refusals.append('it is on more than one network')
    if refusals:
        raise Refused('The container cannot be carried over as it is: ' + '; '.join(refusals) +
                      '. Upgrade it by hand.')

    options = ['--detach', '--name', container['Name'].lstrip('/')]
    policy = host.get('RestartPolicy') or {}
    if policy.get('Name') and policy['Name'] != 'no':
        retries = policy.get('MaximumRetryCount') or 0
        options += ['--restart', policy['Name'] + (f':{retries}' if policy['Name'] == 'on-failure' and retries else '')]
    if mode not in ('default', 'bridge'):
        options += ['--network', mode]
    if container['Config'].get('Hostname') and not container['Id'].startswith(container['Config']['Hostname']) \
            and mode != 'host':
        options += ['--hostname', container['Config']['Hostname']]
    for port, bindings in sorted((host.get('PortBindings') or {}).items()):
        target = port.removesuffix('/tcp')
        for binding in bindings or []:
            address, published = binding.get('HostIp') or '', binding.get('HostPort') or ''
            # An IPv6 address is published in brackets.
            if ':' in address:
                address = f'[{address}]'
            options += ['--publish', f'{address}:{published}:{target}' if address else
                        (f'{published}:{target}' if published else target)]
    for mount in container.get('Mounts') or []:
        read_only = '' if mount.get('RW', True) else ':ro'
        if mount.get('Type') == 'bind':
            options += ['--volume', f"{mount['Source']}:{mount['Destination']}{read_only}"]
        elif mount.get('Type') == 'volume':
            # An anonymous volume too is named, so the new container takes the same data.
            options += ['--volume', f"{mount['Name']}:{mount['Destination']}{read_only}"]
        else:
            raise Refused(f"The container has a {mount.get('Type')} mount at {mount.get('Destination')}, "
                          'which this does not carry over. Upgrade it by hand.')
    for group in host.get('GroupAdd') or []:
        options += ['--group-add', group]
    for extra in host.get('ExtraHosts') or []:
        options += ['--add-host', extra]
    if host.get('Init'):
        options += ['--init']
    if host.get('Memory'):
        options += ['--memory', str(host['Memory'])]
    if host.get('NanoCpus'):
        options += ['--cpus', str(host['NanoCpus'] / 1e9)]
    if (config.get('User') or '') != (image_config.get('User') or ''):
        options += ['--user', config['User']]
    image_labels = image_config.get('Labels') or {}
    for key, value in sorted((config.get('Labels') or {}).items()):
        if image_labels.get(key) != value:
            options += ['--label', f'{key}={value}']
    image_env = set(image_config.get('Env') or [])
    environment = [line for line in config.get('Env') or [] if line not in image_env]
    return options, environment, command


def running(name):
    done = docker('container', 'inspect', '--format', '{{.State.Running}}', name, check=False)
    return done.returncode == 0 and done.stdout.strip() == 'true'


def upgrade(name, version):
    if not VERSION.match(version):
        raise Refused(f'{version!r} is not a version of berth, such as 0.6.0')
    found = docker('container', 'inspect', name, check=False)
    if found.returncode != 0:
        raise Refused(f'There is no container {name} on this node.')
    container = json.loads(found.stdout)[0]
    current = container['Config']['Image']
    target = f'{repository(current)}:{version}'
    old_image = inspect('image', container['Image'])
    labels = (old_image.get('Config') or {}).get('Labels') or {}
    running_version = labels.get('org.opencontainers.image.version') or current.rsplit(':', 1)[-1]
    if version_key(running_version) and version_key(version) and version_key(version) < version_key(running_version):
        raise Refused(f'{name} runs berth {running_version}, which is newer than {version}. '
                      'An older server may not read what a newer one stored, so it is not done here.')
    options, environment, command = run_options(container, old_image)
    print(f'Pulling {target}', flush=True)
    docker('pull', target, capture=False)
    if inspect('image', target)['Id'] == container['Image']:
        print(f'{name} runs {target} already.')
        return
    previous = f'{name}-before-{version}'
    docker('container', 'rm', '--force', previous, check=False)
    with tempfile.NamedTemporaryFile('w', prefix='berth-env-', delete=False) as env_file:
        os.chmod(env_file.name, 0o600)
        env_file.write(''.join(line + '\n' for line in environment))
    try:
        print(f'Stopping {name}, which runs {current}', flush=True)
        docker('container', 'rename', name, previous)
        docker('container', 'stop', previous)
        started = docker('run', *options, '--env-file', env_file.name, target, *command, check=False)
        time.sleep(SETTLE)
        if started.returncode != 0 or not running(name):
            said = (started.stderr or '').strip()
            logs = docker('logs', '--tail', '20', name, check=False)
            docker('container', 'rm', '--force', name, check=False)
            docker('container', 'rename', previous, name)
            docker('container', 'start', name)
            raise Refused(f'The new container did not keep running, so {name} runs {current} again. '
                          f'{said}\n{(logs.stdout + logs.stderr).strip()}'.strip())
    finally:
        os.unlink(env_file.name)
    docker('container', 'rm', previous)
    print(f'{name} runs {target} now, with its data and options as they were.')


def main():
    if len(sys.argv) not in (2, 3):
        sys.exit(__doc__.strip().splitlines()[2].strip())
    name = sys.argv[1]
    version = sys.argv[2] if len(sys.argv) == 3 else os.environ.get('BERTH_ARG_VERSION', '')
    try:
        upgrade(name, version.strip())
    except Refused as refused:
        print(f'✗ {refused}', file=sys.stderr)
        sys.exit(1)


if __name__ == '__main__':
    main()
