"""Virtual machines for the boot tests: the Ubuntu images they start from, the
test side the guest reaches, QEMU, and what a run records.

The guest is on QEMU's user network. To it, this container is 10.0.2.2: there
it finds Ubuntu's mirrors through a cache and an HTTPS server for the release
archive, and QEMU forwards the container's port 22 to the guest's SSH. The
names of Ubuntu's mirrors resolve to the cache for the guest only.

Only meant for the container of ci/boot.compose.yml, which runs it as root.
"""
import contextlib
import hashlib
import json
import os
import shutil
import socket
import subprocess
import sys
import threading
import time
import urllib.request
import uuid
from pathlib import Path

WORK = Path(os.environ.get('BERTH_BOOT_WORK', '/work'))
IMAGES = WORK / 'images'
CACHE = WORK / 'cache'
# The container, as the guest sees it.
GATEWAY = '10.0.2.2'
# A port in the guest the firewall should keep closed, forwarded to this one.
PROBE_PORT, PROBED_PORT = 18080, 8080

CLOUD_RELEASE = 'https://cloud-images.ubuntu.com/releases/26.04/release/'
CLOUD_IMAGE = 'ubuntu-26.04-server-cloudimg-amd64.img'
ISO_RELEASE = 'https://releases.ubuntu.com/26.04/'
LOCAL_ISO = Path('/images/source.iso')
OVMF_CODE = Path('/usr/share/OVMF/OVMF_CODE_4M.fd')
OVMF_VARS = Path('/usr/share/OVMF/OVMF_VARS_4M.fd')
# Ubuntu's mirrors, and their country mirrors below them, go through the cache.
MIRRORS = ('archive.ubuntu.com', 'security.ubuntu.com')

STARTED = time.monotonic()


def say(text):
    minutes, seconds = divmod(int(time.monotonic() - STARTED), 60)
    print(f'[{minutes:3d}:{seconds:02d}] {text}', flush=True)


def run(*argv, **kwargs):
    kwargs.setdefault('check', True)
    return subprocess.run([str(arg) for arg in argv], **kwargs)


def digest(path):
    h = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1 << 20), b''):
            h.update(block)
    return h.hexdigest()


# ---------------------------------------------------------------------------
# Images, checked against Ubuntu's SHA256SUMS and kept in build/boot/images


def published(base):
    """{name: sha256} from a SHA256SUMS next to the images."""
    with urllib.request.urlopen(base + 'SHA256SUMS', timeout=60) as answer:
        lines = answer.read().decode().splitlines()
    sums = {}
    for line in lines:
        parts = line.split()
        if len(parts) == 2:
            sums[parts[1].lstrip('*')] = parts[0]
    return sums


def fetch(base, name, expected=None):
    """The image NAME from BASE, downloaded once and checked each time."""
    expected = expected or published(base).get(name)
    if not expected:
        raise AssertionError(f'{base}SHA256SUMS does not list {name}')
    IMAGES.mkdir(parents=True, exist_ok=True)
    target = IMAGES / name
    if target.is_file():
        if digest(target) == expected:
            say(f'{name}: cached, matches SHA256SUMS')
            return target
        say(f'{name}: cached copy is not the published one, downloading again')
        target.unlink()
    part = target.with_name(name + '.part')
    say(f'Downloading {base}{name}')
    run('curl', '-fL', '--retry', '5', '--silent', '--show-error', '-C', '-', '-o', part, base + name)
    if digest(part) != expected:
        part.unlink()
        raise AssertionError(f'{name} does not match its SHA-256; the download was removed')
    part.replace(target)
    return target


def cloud_image():
    return fetch(CLOUD_RELEASE, CLOUD_IMAGE)


def server_iso():
    """The Ubuntu Server installer: the one mounted from this machine, or a
    download. Either is checked against the given SHA-256 or SHA256SUMS."""
    expected = os.environ.get('BERTH_BOOT_ISO_SHA256') or None
    if LOCAL_ISO.is_file() and LOCAL_ISO.stat().st_size > 0:
        name = os.environ.get('BERTH_BOOT_ISO_NAME') or LOCAL_ISO.name
        if not expected:
            expected = published(ISO_RELEASE).get(name)
        if not expected:
            raise AssertionError(f'{name} is not in {ISO_RELEASE}SHA256SUMS: give BERTH_CI_BOOT_ISO_SHA256')
        if digest(LOCAL_ISO) != expected:
            raise AssertionError(f'{name} does not have the SHA-256 {expected}')
        say(f'{name}: from this machine, matches {expected[:12]}…')
        return LOCAL_ISO, expected
    sums = published(ISO_RELEASE)
    names = sorted((name for name in sums if name.endswith('-live-server-amd64.iso')),
                   key=lambda name: [int(part) for part in name.split('-')[1].split('.')])
    if not names:
        raise AssertionError(f'No server installer in {ISO_RELEASE}SHA256SUMS')
    name = os.environ.get('BERTH_BOOT_ISO_NAME') or names[-1]
    expected = expected or sums[name]
    return fetch(ISO_RELEASE, name, expected), expected


# ---------------------------------------------------------------------------
# The test side: a package cache, a resolver for the guest and the release server


def upstream_resolver():
    for line in Path('/etc/resolv.conf').read_text().splitlines():
        parts = line.split()
        if len(parts) >= 2 and parts[0] == 'nameserver' and parts[1] != '127.0.0.1':
            return parts[1]
    raise AssertionError('No nameserver in /etc/resolv.conf')


NGINX = """\
user root;
worker_processes 2;
pid {dir}/nginx.pid;
error_log {logs}/cache-error.log warn;
events {{ worker_connections 512; }}
http {{
  log_format cache '$time_iso8601 $upstream_cache_status $status $body_bytes_sent $host$request_uri';
  access_log {logs}/cache.log cache;
  client_body_temp_path {dir}/tmp/body;
  proxy_temp_path {dir}/tmp/proxy;
  fastcgi_temp_path {dir}/tmp/fastcgi;
  uwsgi_temp_path {dir}/tmp/uwsgi;
  scgi_temp_path {dir}/tmp/scgi;
  proxy_cache_path {cache} levels=1:2 keys_zone=apt:32m max_size=16g inactive=120d use_temp_path=off;
  resolver {resolver} ipv6=off valid=300s;
  resolver_timeout 10s;
  sendfile on;

  # Ubuntu's mirrors for the guest, which asks them by name.
  server {{
    listen 127.0.0.1:80;
    if ($host !~ ^([a-z0-9-]+\\.)*(archive|security)\\.ubuntu\\.com$) {{ return 403; }}
    proxy_http_version 1.1;
    proxy_set_header Host $host;
    proxy_set_header Connection "";
    # Packages, and indexes addressed by their hash, never change under a name,
    # and every mirror has the same ones under the same path.
    location ~ (\\.d?u?deb|/by-hash/SHA256/[0-9a-f]+)$ {{
      proxy_pass http://$host;
      proxy_cache apt;
      proxy_cache_key $uri;
      proxy_cache_valid 200 120d;
      proxy_cache_lock on;
      proxy_cache_lock_timeout 600s;
      proxy_ignore_headers Cache-Control Expires Set-Cookie X-Accel-Expires;
    }}
    location / {{ proxy_pass http://$host; }}
  }}

  # The release archive, over HTTPS as the seed asks for it.
  server {{
    listen 127.0.0.1:443 ssl;
    ssl_certificate {dir}/server.crt;
    ssl_certificate_key {dir}/server.key;
    root {www};
  }}
}}
"""


class TestSide:
    """What the guest reaches at 10.0.2.2, with a certificate authority of its own."""

    def __init__(self, run_):
        self.dir = run_.dir / 'side'
        self.logs = run_.logs
        self.www = self.dir / 'www'
        shutil.rmtree(self.dir, ignore_errors=True)
        for path in (self.www, self.dir / 'tmp', CACHE / 'apt'):
            path.mkdir(parents=True, exist_ok=True)
        self.resolver = upstream_resolver()
        self._certificates()
        conf = self.dir / 'nginx.conf'
        conf.write_text(NGINX.format(dir=self.dir, logs=self.logs, cache=CACHE / 'apt',
                                     resolver=self.resolver, www=self.www))
        self.cache_before = size_of(CACHE / 'apt')
        self.nginx = subprocess.Popen(['nginx', '-c', str(conf), '-g', 'daemon off;'])
        # Only these answers for the mirrors: a forwarded AAAA query would bring
        # the real names back as a CNAME, and the guest would go round the cache.
        addresses = [option for name in MIRRORS for option in (f'--address=/{name}/{GATEWAY}', f'--local=/{name}/')]
        self.dnsmasq = subprocess.Popen(
            ['dnsmasq', '--keep-in-foreground', '--listen-address=127.0.0.1', '--bind-interfaces',
             '--no-resolv', '--no-hosts', f'--server={self.resolver}', '--user=root',
             f'--log-facility={self.logs}/dns.log', '--log-queries', *addresses])
        # QEMU hands the guest the resolver of this container.
        self.resolv = Path('/etc/resolv.conf').read_text()
        Path('/etc/resolv.conf').write_text('nameserver 127.0.0.1\n')
        for port in (80, 443, 53):
            wait_port(port)

    def _certificates(self):
        d = self.dir
        run('openssl', 'req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256', '-nodes',
            '-keyout', d / 'ca.key', '-out', d / 'ca.crt', '-days', '3', '-subj', '/CN=berth boot tests',
            '-addext', 'basicConstraints=critical,CA:TRUE', '-addext', 'keyUsage=critical,keyCertSign,cRLSign',
            capture_output=True)
        run('openssl', 'req', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256', '-nodes',
            '-keyout', d / 'server.key', '-out', d / 'server.csr', '-subj', f'/CN={GATEWAY}', capture_output=True)
        (d / 'server.ext').write_text(f'subjectAltName=IP:{GATEWAY}\nextendedKeyUsage=serverAuth\n'
                                      'keyUsage=critical,digitalSignature\n')
        run('openssl', 'x509', '-req', '-in', d / 'server.csr', '-CA', d / 'ca.crt', '-CAkey', d / 'ca.key',
            '-CAcreateserial', '-days', '3', '-extfile', d / 'server.ext', '-out', d / 'server.crt',
            capture_output=True)

    @property
    def ca(self):
        return (self.dir / 'ca.crt').read_text()

    def publish(self, path):
        """Serve a file, and its address for the guest."""
        shutil.copyfile(path, self.www / Path(path).name)
        return f'https://{GATEWAY}/{Path(path).name}'

    def cache_stats(self):
        hits = misses = 0
        log = self.logs / 'cache.log'
        if log.exists():
            for line in log.read_text().splitlines():
                state = line.split()[1] if len(line.split()) > 1 else ''
                hits += state == 'HIT'
                misses += state == 'MISS'
        return {'cache_hits': hits, 'cache_misses': misses,
                'cache_mb_before': self.cache_before >> 20, 'cache_mb_after': size_of(CACHE / 'apt') >> 20}

    def stop(self):
        with contextlib.suppress(OSError):
            Path('/etc/resolv.conf').write_text(self.resolv)
        for process in (self.nginx, self.dnsmasq):
            process.terminate()
            with contextlib.suppress(subprocess.TimeoutExpired):
                process.wait(10)


def wait_port(port, host='127.0.0.1', timeout=20):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        with contextlib.suppress(OSError), socket.create_connection((host, port), timeout=1):
            return
        time.sleep(0.2)
    raise AssertionError(f'Nothing listens on {host}:{port}')


def seed_image(path, files):
    """A NoCloud seed: a volume labelled CIDATA with these {name: text} files."""
    folder = path.with_suffix('.d')
    folder.mkdir(parents=True, exist_ok=True)
    for name, text in files.items():
        (folder / name).write_text(text)
    run('xorriso', '-as', 'mkisofs', '-quiet', '-output', path, '-volid', 'CIDATA', '-joliet', '-rational-rock',
        *[folder / name for name in files])
    return path


# ---------------------------------------------------------------------------
# Disks


def disk(path, size='40G', backing=None):
    args = ['qemu-img', 'create', '-q', '-f', 'qcow2']
    if backing:
        args += ['-b', backing, '-F', 'qcow2' if str(backing).endswith(('.img', '.qcow2')) else 'raw']
    run(*args, path, size)
    return path


def filled_disk(path, size, pattern):
    """A disk that holds something already: a byte pattern in its first 64 MiB,
    and an overlay on top that takes every write the guest makes."""
    base = path.with_name(path.stem + '-base.qcow2')
    disk(base, size)
    run('qemu-io', '-f', 'qcow2', '-c', f'write -P {pattern} 0 64M', base, capture_output=True)
    return disk(path, size, backing=base)


def written(path):
    """Bytes the guest wrote to an overlay: those it holds itself, not its backing file."""
    mapped = json.loads(run('qemu-img', 'map', '-U', '--output=json', path, capture_output=True, text=True).stdout)
    return sum(entry['length'] for entry in mapped if entry.get('depth') == 0 and entry.get('data'))


def size_of(path):
    total = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            with contextlib.suppress(OSError):
                total += os.lstat(os.path.join(root, name)).st_blocks * 512
    return total


# ---------------------------------------------------------------------------
# QEMU


def kvm():
    return os.environ.get('BERTH_BOOT_TCG') != '1'


class Machine:
    """One guest: UEFI, virtio disks with serials, maybe a CD-ROM, the user
    network with SSH forwarded, the serial console in a log and a QMP socket."""

    def __init__(self, name, run_):
        self.name = name
        self.run = run_
        self.dir = run_.vm / name
        self.dir.mkdir(parents=True, exist_ok=True)
        self.vars = self.dir / 'OVMF_VARS.fd'
        shutil.copyfile(OVMF_VARS, self.vars)
        self.memory = int(os.environ.get('BERTH_BOOT_MEMORY') or 4096)
        self.cpus = int(os.environ.get('BERTH_BOOT_CPUS') or min(4, os.cpu_count() or 2))
        self.disks = []
        self.cdrom = None
        self.kernel = None
        self.no_reboot = False
        self.process = None
        self.label = None
        self.peak_rss = 0

    def add_disk(self, path, serial):
        self.disks.append((Path(path), serial))

    def argv(self):
        argv = ['qemu-system-x86_64', '-name', f'berth-boot-{self.name}', '-nodefaults',
                '-machine', 'q35', '-smp', str(self.cpus), '-m', str(self.memory)]
        argv += ['-accel', 'kvm', '-cpu', 'host'] if kvm() else ['-accel', 'tcg', '-cpu', 'max']
        argv += ['-drive', f'if=pflash,format=raw,readonly=on,file={OVMF_CODE}',
                 '-drive', f'if=pflash,format=raw,file={self.vars}']
        for number, (path, serial) in enumerate(self.disks):
            argv += ['-drive', f'if=none,id=disk{number},format=qcow2,file={path},discard=unmap',
                     '-device', f'virtio-blk-pci,drive=disk{number},serial={serial}']
        if self.cdrom:
            argv += ['-drive', f'if=none,id=cd0,media=cdrom,format=raw,readonly=on,file={self.cdrom}',
                     '-device', 'ide-cd,drive=cd0,bus=ide.0']
        argv += ['-netdev', f'user,id=net0,ipv6=off,hostfwd=tcp::22-:22,'
                 f'hostfwd=tcp:127.0.0.1:{PROBE_PORT}-:{PROBED_PORT}',
                 '-device', 'virtio-net-pci,netdev=net0',
                 '-object', 'rng-random,id=rng0,filename=/dev/urandom', '-device', 'virtio-rng-pci,rng=rng0',
                 '-vga', 'std', '-display', 'none',
                 '-chardev', f'socket,id=serial0,path={self.dir}/serial.sock,server=on,wait=off,'
                 f'logfile={self.run.logs}/serial-{self.label}.log,logappend=on',
                 '-serial', 'chardev:serial0',
                 '-qmp', f'unix:{self.dir}/qmp.sock,server=on,wait=off']
        if self.no_reboot:
            argv.append('-no-reboot')
        if self.kernel:
            kernel, initrd, append = self.kernel
            argv += ['-kernel', str(kernel), '-initrd', str(initrd), '-append', append]
        return argv

    def start(self, label):
        """Start it; its console goes to logs/serial-LABEL.log."""
        self.label = label
        argv = self.argv()
        (self.run.logs / f'qemu-{label}.argv').write_text(' '.join(argv) + '\n')
        log = (self.run.logs / f'qemu-{label}.log').open('ab')
        self.process = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        self.run.machine = self
        say(f'{self.name}: started ({label}), {self.cpus} vCPUs, {self.memory} MiB'
            + ('' if kvm() else ', without KVM'))

    def running(self):
        if self.process and self.process.poll() is None:
            self.sample()
            return True
        return False

    def sample(self):
        """The guest's peak memory on the container's side so far."""
        with contextlib.suppress(OSError, ValueError, AttributeError):
            for line in Path(f'/proc/{self.process.pid}/status').read_text().splitlines():
                if line.startswith('VmHWM:'):
                    self.peak_rss = max(self.peak_rss, int(line.split()[1]) * 1024)

    def wait(self, timeout, every=None):
        """Wait for QEMU to end, calling every() each minute; its exit code."""
        end = time.monotonic() + timeout
        last = time.monotonic()
        while time.monotonic() < end:
            if not self.running():
                return self.process.returncode
            if every and time.monotonic() - last >= 60:
                every()
                last = time.monotonic()
            time.sleep(2)
        raise AssertionError(f'{self.name} did not power off within {timeout / 60:.0f} minutes')

    def qmp(self, command, **arguments):
        with socket.socket(socket.AF_UNIX) as sock:
            sock.settimeout(30)
            sock.connect(str(self.dir / 'qmp.sock'))
            stream = sock.makefile('rw')
            stream.readline()
            for message in ({'execute': 'qmp_capabilities'}, {'execute': command, 'arguments': arguments}):
                stream.write(json.dumps(message) + '\n')
                stream.flush()
                while True:
                    answer = json.loads(stream.readline())
                    if 'return' in answer or 'error' in answer:
                        break
            return answer

    def screendump(self, name):
        with contextlib.suppress(OSError, ValueError):
            self.qmp('screendump', filename=str(self.run.logs / f'{name}.png'), format='png')

    def stop(self, grace=60):
        """Power it off, and end QEMU if the guest does not go."""
        if not self.running():
            return
        with contextlib.suppress(OSError, ValueError):
            self.qmp('system_powerdown')
        try:
            self.process.wait(grace)
        except subprocess.TimeoutExpired:
            self.kill()

    def kill(self):
        if self.process and self.process.poll() is None:
            self.sample()
            self.process.kill()
            self.process.wait()

    def serial(self):
        log = self.run.logs / f'serial-{self.label}.log'
        return log.read_text(errors='replace') if log.exists() else ''


# ---------------------------------------------------------------------------
# A run: its folders, its phases, the disk it used at most and what it noted


class Run:
    def __init__(self, kind):
        self.kind = kind
        self.dir = WORK / kind
        self.logs = self.dir / 'logs'
        self.vm = self.dir / 'vm'
        for path in (self.logs, self.vm):
            shutil.rmtree(path, ignore_errors=True)
            path.mkdir(parents=True)
        (WORK / 'tmp').mkdir(exist_ok=True)
        self.phases = []
        self.facts = {}
        self.machine = None
        self.peak_disk = 0
        self.cache_cold = not any((CACHE / 'apt').glob('*'))
        self._stop = threading.Event()
        threading.Thread(target=self._watch, daemon=True).start()

    def _watch(self):
        while not self._stop.wait(15):
            self.peak_disk = max(self.peak_disk, size_of(WORK))
            if self.machine:
                self.machine.running()

    @contextlib.contextmanager
    def phase(self, name):
        started = time.monotonic()
        say(f'▸ {name}')
        try:
            yield
        finally:
            seconds = time.monotonic() - started
            self.phases.append((name, round(seconds)))
            say(f'  {name}: {seconds / 60:.1f} min')

    def report(self, outcome, extra=None):
        self._stop.set()
        self.peak_disk = max(self.peak_disk, size_of(WORK))
        report = {
            'kind': self.kind, 'outcome': outcome, 'kvm': kvm(),
            'apt_cache': 'cold' if self.cache_cold else 'warm',
            'minutes': round((time.monotonic() - STARTED) / 60, 1),
            'phases_seconds': dict(self.phases),
            'peak_disk_gb_build_boot': round(self.peak_disk / 2**30, 2),
            'peak_guest_rss_gb': round((self.machine.peak_rss if self.machine else 0) / 2**30, 2),
            **(extra or {}), **self.facts,
        }
        (self.dir / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report, indent=2), file=sys.stderr, flush=True)
        return report


def meta_data(hostname):
    return json.dumps({'instance-id': f'berth-boot-{uuid.uuid4()}', 'local-hostname': hostname}) + '\n'
