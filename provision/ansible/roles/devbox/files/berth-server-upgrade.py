#!/usr/bin/env python3
"""Run the berth server in a container from the image of another version, or
make it again with options of its own, keeping the data and the options of
the running container.

    berth-server-upgrade CONTAINER                the version in BERTH_ARG_VERSION
    berth-server-upgrade CONTAINER VERSION
    berth-server-upgrade CONTAINER --image IMAGE [--pull] [--keep-image]
        [--publish P]… [--volume SRC:DST]… [--add-host NAME:ADDRESS]… [--group-add GROUP]…

The first two are the node's action upgrade-server: the image is the
container's own repository at VERSION. A server that runs an image built
from a berth, tagged berth:VERSION, goes back to the repository its image
names as its baseline, ghcr.io/helightdev/berth unless it names another.

With BERTH_ARG_SOURCE=here the action builds the image instead: standard
input is the build context a berth sends of itself, with its Dockerfile,
requirements.txt and the program as berth. It is checked and handed to
`docker build -t berth:VERSION -` as it is, and nothing is unpacked. The
program has to be a static Linux program for this machine's CPU. An image
of that tag that holds the same program is not built again.

The third is the server recipe's, and what `berth server deploy` runs once it
has built an image: it takes the image as given, or keeps the container's own
with --keep-image, and sets the options it names over those of the
container. An option it names replaces the container's one for the same
port, mount point or host name. Nothing changes when the container runs
that image with those options already.

Once a container runs, the images built from a berth that no container
uses are removed, except the two newest.

The new container gets the same name, mounts, published ports, restart
policy, groups, extra hosts, network, logging, user and environment as the
old one, so it keeps the server's data and what it reaches. An option this
does not know how to carry over stops it before anything changes. The old
container is renamed and stopped, not removed, until the new one runs; if the
new one does not come up, the old one is started again. Environment values
go to docker in a private file, never on its command line.
"""
import argparse
import hashlib
import io
import json
import os
import re
import subprocess
import sys
import tarfile
import tempfile
import time

DOCKER = os.environ.get('DEVBOX_DOCKER', 'docker')
VERSION = re.compile(r'^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$')
# How long the new container has to stay running before the old one goes.
SETTLE = float(os.environ.get('DEVBOX_UPGRADE_SETTLE', '10'))
# What `docker run` gives a container without --shm-size.
DEFAULT_SHM = 64 * 1024 * 1024
UNCHANGED = 'is unchanged'
# Where the published images are, and the labels an image of berth has.
BASELINE = 'ghcr.io/helightdev/berth'
SELF_BUILT_REPOSITORY = 'berth'
SOURCE_LABEL = 'dev.berth.source'
BASELINE_LABEL = 'dev.berth.baseline'
BINARY_LABEL = 'dev.berth.binary-sha256'
SELF_BUILT = 'self-built'
# How many images built from a berth stay when nothing uses them.
KEPT_BUILDS = 2
# What a build context of berth holds, and nothing else.
CONTEXT = {'Dockerfile', 'requirements.txt', 'berth'}


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


def labels_of(image):
    return (image.get('Config') or {}).get('Labels') or {}


def image_version(image, ref):
    return labels_of(image).get('org.opencontainers.image.version') or ref.rsplit(':', 1)[-1]


def release_repository(image, ref):
    """Where a release of the server is pulled from: the container's own
    repository, or for one built from a berth the baseline its image names."""
    labels = labels_of(image)
    own = repository(ref)
    if labels.get(SOURCE_LABEL) == SELF_BUILT or own in (SELF_BUILT_REPOSITORY, 'sha256'):
        return labels.get(BASELINE_LABEL) or BASELINE
    return own


# Options of a container that would change what it is if they were dropped,
# and which this does not carry over: one that is set refuses the upgrade.
# What docker itself fills in for every container, such as the masked paths,
# the cgroup namespace or the console size, is not among them.
UNCARRIED = {
    'Privileged': 'it runs privileged',
    'CapAdd': 'it has capabilities added',
    'CapDrop': 'it has capabilities dropped',
    'Devices': 'it has devices',
    'DeviceRequests': 'it asks for devices such as GPUs',
    'DeviceCgroupRules': 'it has device rules',
    'SecurityOpt': 'it has security options',
    'Sysctls': 'it has sysctls',
    'Tmpfs': 'it has tmpfs mounts',
    'VolumesFrom': 'it takes volumes from another container',
    'Links': 'it has links',
    'Dns': 'it has DNS servers of its own',
    'DnsSearch': 'it has DNS search domains of its own',
    'DnsOptions': 'it has DNS options of its own',
    'Ulimits': 'it has ulimits',
    'PidMode': 'it shares a process namespace',
    'UTSMode': 'it shares a UTS namespace',
    'UsernsMode': 'it has a user namespace mode',
    'CgroupParent': 'it has a cgroup parent of its own',
    'CpuShares': 'it has CPU shares',
    'CpuQuota': 'it has a CPU quota',
    'CpuPeriod': 'it has a CPU period',
    'CpusetCpus': 'it is pinned to CPUs',
    'CpusetMems': 'it is pinned to memory nodes',
    'BlkioWeight': 'it has a block I/O weight',
    'MemoryReservation': 'it has a memory reservation',
    'OomKillDisable': 'it is kept from the OOM killer',
    'OomScoreAdj': 'it has an OOM score of its own',
    'ReadonlyRootfs': 'its root file system is read-only',
    'AutoRemove': 'it is removed when it stops',
    'StorageOpt': 'it has storage options',
}


def refusals_of(container, image_config):
    config, host = container['Config'], container['HostConfig']
    refusals = [why for key, why in UNCARRIED.items()
                if host.get(key) not in (None, '', 0, False, [], {})]
    ipc = host.get('IpcMode')
    if ipc not in (None, '', 'private', 'shareable'):
        refusals.append(f'it shares an IPC namespace ({ipc})')
    swap = host.get('MemorySwap')
    if swap not in (None, 0, -1) and swap != 2 * (host.get('Memory') or 0):
        refusals.append('it has a swap limit of its own')
    runtime = host.get('Runtime') or 'runc'
    if runtime != 'runc':
        refusals.append(f'it runs under the runtime {runtime}')
    if (config.get('Entrypoint') or None) != (image_config.get('Entrypoint') or None):
        refusals.append('its entry point is not its image\'s')
    if (config.get('WorkingDir') or '') != (image_config.get('WorkingDir') or ''):
        refusals.append('its working directory is not its image\'s')
    if config.get('Healthcheck') and config.get('Healthcheck') != image_config.get('Healthcheck'):
        refusals.append('it has a health check of its own')
    if len(((container.get('NetworkSettings') or {}).get('Networks') or {})) > 1:
        refusals.append('it is on more than one network')
    for mount in container.get('Mounts') or []:
        if mount.get('Type') not in ('bind', 'volume'):
            refusals.append(f"it has a {mount.get('Type')} mount at {mount.get('Destination')}")
    return refusals


def mount_option(mount):
    source = mount['Source'] if mount.get('Type') == 'bind' else mount['Name']
    # An anonymous volume too is named, so the new container takes the same data.
    modes = [part for part in (mount.get('Mode') or '').split(',') if part and part not in ('rw', 'ro')]
    if not mount.get('RW', True):
        modes.insert(0, 'ro')
    return f"{source}:{mount['Destination']}" + (':' + ','.join(modes) if modes else '')


def run_options(container, old_image):
    """The options of `docker run` that make a container like this one, its
    environment beyond what its image sets, and its command when it is not
    the image's. Refused when it has one this does not carry over."""
    config, host = container['Config'], container['HostConfig']
    image_config = old_image.get('Config') or {}
    refusals = refusals_of(container, image_config)
    if refusals:
        raise Refused('The container cannot be carried over as it is: ' + '; '.join(refusals) +
                      '. Upgrade it by hand.')
    if (config.get('Cmd') or None) != (image_config.get('Cmd') or None) and config.get('Cmd'):
        command = config['Cmd']
    else:
        command = []
    mode = host.get('NetworkMode') or 'default'

    options = ['--detach', '--name', container['Name'].lstrip('/')]
    policy = host.get('RestartPolicy') or {}
    if policy.get('Name') and policy['Name'] != 'no':
        retries = policy.get('MaximumRetryCount') or 0
        options += ['--restart', policy['Name'] + (f':{retries}' if policy['Name'] == 'on-failure' and retries else '')]
    if mode not in ('default', 'bridge'):
        options += ['--network', mode]
    if config.get('Hostname') and not container['Id'].startswith(config['Hostname']) and mode != 'host':
        options += ['--hostname', config['Hostname']]
    if config.get('Domainname'):
        options += ['--domainname', config['Domainname']]
    for port, bindings in sorted((host.get('PortBindings') or {}).items()):
        target = port.removesuffix('/tcp')
        for binding in bindings or []:
            address, published = binding.get('HostIp') or '', binding.get('HostPort') or ''
            # An IPv6 address is published in brackets.
            if ':' in address:
                address = f'[{address}]'
            options += ['--publish', f'{address}:{published}:{target}' if address else
                        (f'{published}:{target}' if published else target)]
    if host.get('PublishAllPorts'):
        options += ['--publish-all']
    for mount in container.get('Mounts') or []:
        options += ['--volume', mount_option(mount)]
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
    if host.get('ShmSize') and host['ShmSize'] != DEFAULT_SHM:
        options += ['--shm-size', str(host['ShmSize'])]
    # The daemon's own log options, such as max-size in daemon.json, are set on
    # every container: they are carried as they are, like any other.
    log = host.get('LogConfig') or {}
    if log.get('Type'):
        options += ['--log-driver', log['Type']]
    for key, value in sorted((log.get('Config') or {}).items()):
        options += ['--log-opt', f'{key}={value}']
    if config.get('StopSignal') and config['StopSignal'] != image_config.get('StopSignal'):
        options += ['--stop-signal', config['StopSignal']]
    if config.get('StopTimeout') is not None and config.get('StopTimeout') != image_config.get('StopTimeout'):
        options += ['--stop-timeout', str(config['StopTimeout'])]
    if (config.get('User') or '') != (image_config.get('User') or ''):
        options += ['--user', config['User']]
    image_labels = image_config.get('Labels') or {}
    for key, value in sorted((config.get('Labels') or {}).items()):
        if image_labels.get(key) != value:
            options += ['--label', f'{key}={value}']
    image_env = set(image_config.get('Env') or [])
    environment = [line for line in config.get('Env') or [] if line not in image_env]
    return options, environment, command


def pairs(options):
    """The options after --detach and the name, as (flag, value) with None
    for a flag without one."""
    found, rest = [], options[3:]
    alone = {'--init', '--publish-all'}
    while rest:
        flag = rest.pop(0)
        found.append((flag, None if flag in alone else rest.pop(0)))
    return found


def key_of(flag, value):
    """What an option is about, so one asked for replaces the container's."""
    if flag == '--publish':
        return flag, value.rsplit(':', 1)[-1]
    if flag == '--volume':
        return flag, value.split(':')[1] if ':' in value else value
    if flag == '--add-host':
        return flag, re.split('[:=]', value, maxsplit=1)[0]
    return flag, value


def with_asked(options, asked):
    """The container's options with those asked for in place of the ones for
    the same port, mount point or host name, and the others added."""
    head, kept = options[:3], pairs(options)
    keys = {key_of(flag, value) for flag, value in asked}
    kept = [(flag, value) for flag, value in kept
            if flag not in ('--publish', '--volume', '--add-host', '--group-add') or key_of(flag, value) not in keys]
    merged = kept + [option for option in asked if option not in kept]
    out = list(head)
    for flag, value in merged:
        out += [flag] if value is None else [flag, value]
    return out


def running(name):
    done = docker('container', 'inspect', '--format', '{{.State.Running}}', name, check=False)
    return done.returncode == 0 and done.stdout.strip() == 'true'


def replace(name, container, current, target, options, environment, command, label):
    """Run `target` as `name` with these options, in place of the container."""
    previous = f'{name}-before-{label}'
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


def found_container(name):
    found = docker('container', 'inspect', name, check=False)
    if found.returncode != 0:
        raise Refused(f'There is no container {name} here.')
    return json.loads(found.stdout)[0]


def refuse_older(name, running_version, version):
    if version_key(running_version) and version_key(version) and version_key(version) < version_key(running_version):
        raise Refused(f'{name} runs berth {running_version}, which is newer than {version}. '
                      'An older server may not read what a newer one stored, so it is not done here.')


def upgrade(name, version):
    """The node's action: the container's repository at `version`."""
    if not VERSION.match(version):
        raise Refused(f'{version!r} is not a version of berth, such as 0.6.0')
    container = found_container(name)
    current = container['Config']['Image']
    old_image = inspect('image', container['Image'])
    target = f'{release_repository(old_image, current)}:{version}'
    refuse_older(name, image_version(old_image, current), version)
    options, environment, command = run_options(container, old_image)
    print(f'Pulling {target}', flush=True)
    docker('pull', target, capture=False)
    if inspect('image', target)['Id'] == container['Image']:
        print(f'{name} runs {target} already.')
        return
    replace(name, container, current, target, options, environment, command, version)
    print(f'{name} runs {target} now, with its data and options as they were.')
    prune()


def remake(name, image, pull, keep_image, asked):
    """The recipe's: `image`, or the container's own, with `asked` set over
    the container's options."""
    container = found_container(name)
    current = container['Config']['Image']
    old_image = inspect('image', container['Image'])
    options, environment, command = run_options(container, old_image)
    target = current if keep_image else image
    if pull and not keep_image:
        print(f'Pulling {target}', flush=True)
        docker('pull', target, capture=False)
    found = docker('image', 'inspect', target, check=False)
    if found.returncode != 0:
        if keep_image:
            raise Refused(f'The image {target} of {name} is not here any more. Update it with the image to run.')
        docker('pull', target, capture=False)
        found = docker('image', 'inspect', target)
    new_image = json.loads(found.stdout)[0]
    if keep_image and new_image['Id'] != container['Image']:
        # The tag was pulled again since: making the container again would update it unasked.
        target = container['Image']
        new_image = old_image
    refuse_older(name, image_version(old_image, current), image_version(new_image, target))
    wanted = with_asked(options, asked)
    same = sorted(pairs(wanted), key=str) == sorted(pairs(options), key=str)
    if new_image['Id'] == container['Image'] and same:
        print(f'{name} {UNCHANGED}: it runs {current} with the options asked for.')
        return
    replace(name, container, current, target, wanted, environment, command, 'update')
    asked_too = ' and those asked for' if asked else ''
    print(f'{name} runs {target} now, with its data and options as they were{asked_too}.')
    prune()


def unfit(program, machine):
    """Why `program` cannot run in the image on this machine, if it cannot:
    it has to be a 64-bit Linux program for this CPU that names no
    interpreter, as a static build does not."""
    if program[:4] != b'\x7fELF':
        return 'it is not a Linux program'
    if program[4:6] != b'\x02\x01' or len(program) < 64:
        return 'it is not a 64-bit Linux program'
    number = lambda at, size: int.from_bytes(program[at:at + size], 'little')  # noqa: E731
    wanted = {'x86_64': 62, 'amd64': 62, 'aarch64': 183, 'arm64': 183}.get(machine)
    if number(18, 2) != wanted:
        return f'it is built for another CPU than this machine\'s, {machine}'
    table, size, count = number(32, 8), number(54, 2), number(56, 2)
    for header in range(table, table + size * count, size or 1):
        if header + 4 > len(program):
            return 'its program header is cut short'
        if number(header, 4) == 3:
            return 'it loads the system\'s C library, as it is not a static build'
    return None


def read_context(context):
    """The program in a build context of berth, once the context is one."""
    try:
        with tarfile.open(fileobj=io.BytesIO(context), mode='r:*') as tar:
            members = tar.getmembers()
            names = sorted(member.name for member in members)
            if sorted(CONTEXT) != names or not all(member.isfile() for member in members):
                raise Refused(f'The build context holds {", ".join(names) or "nothing"}, '
                              f'not {", ".join(sorted(CONTEXT))} alone.')
            return tar.extractfile('berth').read()
    except tarfile.TarError as error:
        raise Refused(f'The build context is not a tar stream: {error}') from error


def build(name, version, context):
    """The node's action with source=here: the image of the berth that sent
    `context`, as berth:VERSION, and then the container from it."""
    if not VERSION.match(version):
        raise Refused(f'{version!r} is not a version of berth, such as 0.6.0')
    container = found_container(name)
    program = read_context(context)
    machine = os.uname().machine
    why = unfit(program, machine)
    if why:
        raise Refused(f'The berth sent cannot run in a server\'s image here: {why}. '
                      'A release of berth for Linux can.')
    digest = hashlib.sha256(program).hexdigest()
    target = f'{SELF_BUILT_REPOSITORY}:{version}'
    found = docker('image', 'inspect', target, check=False)
    held = labels_of(json.loads(found.stdout)[0]).get(BINARY_LABEL) if found.returncode == 0 else None
    if held == digest:
        print(f'{target} holds this berth already ({digest[:12]}).', flush=True)
    else:
        print(f'Building {target} of berth {version} ({digest[:12]}) on this machine', flush=True)
        built = subprocess.run([DOCKER, 'build', '--tag', target, '--build-arg', f'BERTH_VERSION={version}',
                                '--build-arg', f'BERTH_SHA256={digest}', '-'], input=context)
        if built.returncode != 0:
            raise Refused(f'docker build of {target} failed, so {name} runs '
                          f'{container["Config"]["Image"]} as before.')
    remake(name, target, False, False, [])


def prune():
    """Remove the images built from a berth that no container uses, except
    the newest few. One a container uses stays, whatever its age."""
    listed = docker('image', 'ls', '--all', '--quiet', '--no-trunc',
                    '--filter', f'label={SOURCE_LABEL}={SELF_BUILT}', check=False)
    ids = list(dict.fromkeys(listed.stdout.split())) if listed.returncode == 0 else []
    if len(ids) <= KEPT_BUILDS:
        return
    containers = docker('container', 'ls', '--all', '--quiet', '--no-trunc', check=False).stdout.split()
    used = {inspect('container', container).get('Image') for container in containers}
    images = sorted((inspect('image', image) for image in ids), key=lambda image: image.get('Created') or '',
                    reverse=True)
    for image in images[KEPT_BUILDS:]:
        if image['Id'] in used:
            continue
        tags = image.get('RepoTags') or []
        for reference in tags or [image['Id']]:
            docker('image', 'rm', reference, check=False)
        print(f'Removed the image {", ".join(tags) or image["Id"][:19]}, built from a berth and used by no container.')


def main():
    if len(sys.argv) > 2 and sys.argv[2].startswith('--'):
        parser = argparse.ArgumentParser(prog='berth-server-upgrade')
        parser.add_argument('container')
        parser.add_argument('--image', required=True)
        parser.add_argument('--pull', action='store_true')
        parser.add_argument('--keep-image', action='store_true')
        for flag in ('--publish', '--volume', '--add-host', '--group-add'):
            parser.add_argument(flag, action='append', default=[])
        args = parser.parse_args()
        asked = [(f'--{flag}', value) for flag in ('publish', 'volume', 'add-host', 'group-add')
                 for value in getattr(args, flag.replace('-', '_')) if value]
        work = lambda: remake(args.container, args.image, args.pull, args.keep_image, asked)  # noqa: E731
    elif len(sys.argv) == 2 and os.environ.get('BERTH_ARG_SOURCE', '').strip() == 'here':
        version = os.environ.get('BERTH_ARG_VERSION', '').strip()
        work = lambda: build(sys.argv[1], version, sys.stdin.buffer.read())  # noqa: E731
    elif len(sys.argv) in (2, 3):
        source = os.environ.get('BERTH_ARG_SOURCE', '').strip() or 'release'
        if len(sys.argv) == 2 and source != 'release':
            sys.exit(f'✗ source is release or here, not {source!r}')
        version = sys.argv[2] if len(sys.argv) == 3 else os.environ.get('BERTH_ARG_VERSION', '')
        work = lambda: upgrade(sys.argv[1], version.strip())  # noqa: E731
    else:
        sys.exit('Usage: berth-server-upgrade CONTAINER [VERSION | --image IMAGE …]')
    try:
        work()
    except Refused as refused:
        print(f'✗ {refused}', file=sys.stderr)
        sys.exit(1)


if __name__ == '__main__':
    main()
