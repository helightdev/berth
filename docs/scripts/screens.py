#!/usr/bin/env python3
"""Terminal screenshots for the documentation, in docs/public/screens/.

Runs the built berth binary (target/debug/berth) in a pseudo-terminal against an
invented set-up and writes what the terminal shows as PNG, in the UI's colours.
Each picture is drawn as SVG first, then rendered by headless Chrome at twice the
size with JetBrains Mono (docs/scripts/fonts, SIL OFL) embedded, so the text looks
the same wherever the PNG is shown. Only the PNGs are kept.

Nothing comes from your own machine: HOME, the XDG directories and the runtime
directory point into a scratch directory, the PATH starts with stand-ins for
tailscale, nmcli, lsblk, ssh-add and sudo that report invented machines, and the
one host the pictures connect to, `studio`, is a throw-away Ubuntu container
running berthd. No other host is contacted.

    python3 -m venv /tmp/screens-venv && /tmp/screens-venv/bin/pip install pyte pyyaml
    cargo build                     # berth, and the static berthd and box it embeds
    /tmp/screens-venv/bin/python docs/scripts/screens.py [--only NAME…] [--keep]

Needs Chrome or Chromium (set CHROME to its path if it is not on the PATH). To
render SVG files that already exist, without berth or Docker:

    python3 docs/scripts/screens.py --from-svg DIR [--only NAME…]

Needs Docker and the ubuntu:26.04 image for the pictures of a connected host;
without them only the others are made.
"""

from __future__ import annotations

import argparse
import base64
import html
import json
import os
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import fcntl
import time
from pathlib import Path

try:
    import pyte
except ImportError:  # only the --from-svg mode works without it
    pyte = None

REPO = Path(__file__).resolve().parents[2]
BERTH = REPO / "target/debug/berth"
OUT = REPO / "docs/public/screens"
CONTAINER = "berth-demo-studio"
IMAGE = "ubuntu:26.04"

# The theme of the UI (crates/berth-tui/src/theme.rs); used for the frame and for cells that
# keep the terminal's default colours, such as plain command output.
BASE = "#0d111c"
TEXT = "#cdd6f4"
MUTED = "#7c89b0"
LINE = "#344264"
ACCENT = "#74b2ff"

FONTS = Path(__file__).resolve().parent / "fonts"
SCALE = 2

ANSI = {
    "black": "#1b2235", "red": "#f28096", "green": "#9ed88c", "brown": "#f0c674",
    "yellow": "#f0c674", "blue": "#74b2ff", "magenta": "#c7a0f5", "cyan": "#7fd4e0",
    "white": "#cdd6f4", "brightblack": "#7c89b0", "brightred": "#f28096",
    "brightgreen": "#9ed88c", "brightyellow": "#f0c674", "brightblue": "#74b2ff",
    "brightmagenta": "#c7a0f5", "brightcyan": "#7fd4e0", "brightwhite": "#e3e9fb",
}

# ---------------------------------------------------------------------------
# The invented set-up


def demo_env(root: Path) -> dict:
    home = root
    env = {
        "HOME": str(home),
        "USER": "demo",
        "LOGNAME": "demo",
        "XDG_CONFIG_HOME": str(home / ".config"),
        "XDG_DATA_HOME": str(home / ".local/share"),
        "XDG_RUNTIME_DIR": str(root / ".run"),
        "PATH": f"{root / '.stand-ins'}:/usr/local/bin:/usr/bin:/bin",
        "TERM": "xterm-256color",
        "COLORTERM": "truecolor",
        "LANG": "C.UTF-8",
        "LC_ALL": "C.UTF-8",
    }
    return env


TAILSCALE_STATUS = {
    "Self": {"HostName": "workstation", "DNSName": "workstation.example.ts.net.", "OS": "linux",
             "TailscaleIPs": ["100.64.0.2"]},
    "Peer": {
        "nodekey:1": {"HostName": "studio", "DNSName": "studio.example.ts.net.", "OS": "linux",
                      "TailscaleIPs": ["100.64.0.12"]},
        "nodekey:2": {"HostName": "cloud-1", "DNSName": "cloud-1.example.ts.net.", "OS": "linux",
                      "TailscaleIPs": ["100.64.0.21"]},
        "nodekey:3": {"HostName": "phone", "DNSName": "phone.example.ts.net.", "OS": "android",
                      "TailscaleIPs": ["100.64.0.30"]},
    },
}

STAND_INS = {
    # Address suggestions come from here.
    "tailscale": f"""#!/bin/sh
[ "$1 $2" = "status --json" ] && cat <<'EOF'
{json.dumps(TAILSCALE_STATUS)}
EOF
""",
    # One USB stick, for the drive suggestions of the USB installer.
    "lsblk": """#!/bin/sh
echo 'PATH="/dev/sdb" TYPE="disk" TRAN="usb" SIZE="57.3G" MODEL="Demo Flash Drive"'
""",
    # No saved Wi-Fi networks, no agent keys, no sudo, no browser.
    "nmcli": "#!/bin/sh\nexit 1\n",
    "ssh-add": "#!/bin/sh\nexit 1\n",
    "sudo": "#!/bin/sh\necho 'sudo is not available in the demo' >&2\nexit 1\n",
    "xdg-open": "#!/bin/sh\nexit 0\n",
}


def profile(name: str, **values) -> dict:
    base = json.loads((REPO / "provision/config/example.json").read_text())
    base.update({"hostname": name})
    base.update(values)
    return base


def make_demo(root: Path) -> dict:
    """Write the invented workstation: keys, SSH config, hosts, registries."""
    if root.exists():
        shutil.rmtree(root)
    env = demo_env(root)
    home = Path(env["HOME"])
    (root / ".stand-ins").mkdir(parents=True)
    (root / ".run").mkdir(mode=0o700)
    for name, text in STAND_INS.items():
        path = root / ".stand-ins" / name
        path.write_text(text)
        path.chmod(0o755)
    ssh = home / ".ssh"
    ssh.mkdir(parents=True, mode=0o700)
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "demo@workstation",
                    "-f", str(ssh / "id_ed25519")], check=True)
    (ssh / "config").write_text("Host cloud-1\n  HostName 203.0.113.10\n  User ops\n\n"
                                "Host build-vm\n  HostName 192.0.2.40\n  User ops\n")
    key = (ssh / "id_ed25519.pub").read_text().strip()

    # The fake hash stands in for what F5 or `mkpasswd -m sha-512` would store.
    fake_hash = "$6$demo$" + "x" * 86
    features = ["claude/cli", "codex/cli", "t3/server", "github/cli", "backup/restic"]
    studio = profile(
        "studio", profile="laptop", operator_ssh_public_keys=[key], operator_password_hash=fake_hash,
        keyboard="us",
        ansible_vars={"devbox_plugin_features": features},
        builder={},
        access={"address": "100.64.0.12", "user": "ops", "sudo": "none",
                # The demo host is the container; a real profile reaches berthd over SSH.
                "link": ["docker", "exec", "-i", CONTAINER, "berthd", "connect"]},
    )
    cloud = profile(
        "cloud-1", profile="cloud", operator_ssh_public_keys=[key], keyboard="us",
        ansible_vars={"devbox_plugin_features": features[:4]},
        builder={},
    )
    laptop = profile(
        "garage-laptop", profile="laptop", operator_ssh_public_keys=[key], keyboard="us",
        ansible_vars={"devbox_plugin_features": features[:4]},
        builder={},
        access={"address": "192.0.2.31", "user": "ops"},
    )
    for host in (studio, cloud, laptop):
        run(env, "profile", host["hostname"], "--set", "-", stdin=json.dumps(host, indent=2) + "\n")
    run(env, "config", "default_host", "studio")
    # Registries and a secret, through the commands; nothing is contacted.
    run(env, "registry", "add", "nexus", "nexus", "https://nexus.example.com", "ci-user")
    run(env, "registry", "repo", "nexus", "npm", "npm-group")
    run(env, "registry", "repo", "nexus", "raw", "berth-releases")
    run(env, "registry", "repo", "nexus", "docker", "docker-hosted", "nexus.example.com:8083")
    run(env, "registry", "add", "ghcr", "docker", "ghcr.io", "demo-user")
    run(env, "secret", "set", "github", stdin="github_pat_demo_not_a_real_token\n")
    run(env, "secret", "set", "backup-password", stdin="correct-horse-demo\n")
    for host in ("studio", "cloud-1"):
        run(env, "host", host, "secret", "github", "env", "GH_TOKEN")
        run(env, "host", host, "registry", "nexus", "on")
    run(env, "host", "studio", "secret", "backup-password", "file", "~/.config/berth-backup/password", "0600")
    return env


def run(env: dict, *args: str, check=True, stdin=None) -> str:
    done = subprocess.run([str(BERTH), *args], env=env, cwd=env["HOME"], input=stdin,
                          capture_output=True, text=True)
    if check and done.returncode != 0:
        raise SystemExit(f"berth {' '.join(args)} failed: {done.stderr}")
    return done.stdout


# ---------------------------------------------------------------------------
# The host: a container with berthd and what the role would have written


def host_files() -> dict:
    """tasks.json, services.json and channels.json as the role writes them for
    the plugin features of the demo profile."""
    import yaml  # noqa: PLC0415 (only needed here)

    task_dir = "/usr/local/lib/berth/tasks"
    core = yaml.safe_load((REPO / "provision/ansible/roles/devbox/vars/main.yml").read_text())["devbox_core_tasks"]
    for task in core:
        task["run"] = task["run"].replace("{{ devbox_task_dir }}", task_dir)
    tasks, services, channels = core, [], []
    features = ["claude/cli", "codex/cli", "t3/server", "github/cli", "backup/restic"]
    for chosen in features:
        plugin_id, feature_id = chosen.split("/")
        package = json.loads((REPO / "plugins" / plugin_id / "package.json").read_text())
        label = package.get("displayName", plugin_id)
        feature = next(f for f in package["berth"]["features"] if f["id"] == feature_id)
        # The same as templates/berth-feature.json.j2.
        for action in feature.get("actions", []):
            extends = action.get("extends", [])
            tasks.append({
                "name": f"{plugin_id}:{action['name']}",
                "label": action.get("label", action["name"]),
                "help": action.get("help", ""),
                "group": action.get("group") or label,
                "user": action["user"] if action.get("user") in ("operator", "root") else "agent",
                "terminal": action.get("terminal") is True,
                "agent": action.get("agent") is True,
                "extends": [extends] if isinstance(extends, str) else extends,
                "confirm": action.get("confirm"),
                "input": None,
                "arguments": action.get("arguments", []),
                "run": action["run"],
            })
        for service in feature.get("services", []):
            services.append({"name": f"{plugin_id}/{service['name']}", **service})
        for channel in feature.get("channels", []):
            channels.append({**channel, "topic": f"{plugin_id}/{channel['topic']}"})
    berthd = {"agent": "agent", "operator": "ops", "roots": ["/home/agent", "/srv/agent"],
              "units": ["ssh", "tailscaled", "berthd"], "agent_env": "/usr/local/bin/agent-env"}
    return {"berthd": berthd, "tasks": tasks, "services": services, "channels": channels}


def docker(*args: str, check=True, **kwargs):
    return subprocess.run(["docker", *args], check=check, capture_output=True, text=True, **kwargs)


def start_host(scratch: Path) -> bool:
    if not shutil.which("docker") or docker("image", "inspect", IMAGE, check=False).returncode:
        print(f"no docker or no {IMAGE} image: skipping the pictures of a connected host")
        return False
    binaries = REPO / "provision/ansible/roles/devbox/files/bin"
    if not (binaries / "berthd").is_file():
        print("no static berthd (cargo build): skipping the pictures of a connected host")
        return False
    docker("rm", "-f", CONTAINER, check=False)
    docker("run", "-d", "--name", CONTAINER, "--hostname", "studio", IMAGE, "sleep", "infinity")
    sh = lambda script: docker("exec", CONTAINER, "sh", "-c", script)  # noqa: E731
    sh("useradd -m -s /bin/bash agent && useradd -m -s /bin/bash ops"
       " && mkdir -p /etc/berth /srv/agent/workspaces /srv/agent/artifacts /usr/local/lib/berth/tasks"
       " && chown -R agent: /srv/agent")
    for program in ("berthd", "box"):
        docker("cp", str(binaries / program), f"{CONTAINER}:/usr/local/bin/{program}")
    files = scratch / "etc-berth"
    files.mkdir(exist_ok=True)
    for name, value in host_files().items():
        (files / f"{name}.json").write_text(json.dumps(value, indent=2) + "\n")
        docker("cp", str(files / f"{name}.json"), f"{CONTAINER}:/etc/berth/{name}.json")
    docker("exec", "-d", CONTAINER, "berthd")
    for _ in range(100):
        if docker("exec", CONTAINER, "test", "-S", "/run/berth/control.sock", check=False).returncode == 0:
            return True
        time.sleep(0.1)
    raise SystemExit("berthd did not start in the container")


def stop_host():
    if shutil.which("docker"):
        docker("rm", "-f", CONTAINER, check=False)


# ---------------------------------------------------------------------------
# Driving a terminal


class Terminal:
    def __init__(self, argv, env, cols, rows, cwd):
        self.cols, self.rows = cols, rows
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(cwd)
            os.execve(argv[0], argv, env)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    def pump(self, seconds):
        end = time.time() + seconds
        while True:
            left = end - time.time()
            if left <= 0:
                return
            ready, _, _ = select.select([self.fd], [], [], left)
            if not ready:
                continue
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                return
            if not data:
                return
            # Answer what crossterm asks the terminal, so it does not wait.
            if b"\x1b[6n" in data:
                y, x = self.screen.cursor.y + 1, self.screen.cursor.x + 1
                os.write(self.fd, f"\x1b[{y};{x}R".encode())
            if b"\x1b[c" in data:
                os.write(self.fd, b"\x1b[?62;22c")
            self.stream.feed(data)

    def send(self, keys, wait=0.6):
        for key in keys:
            os.write(self.fd, KEYS.get(key, key).encode() if isinstance(key, str) else key)
            self.pump(0.15)
        self.pump(wait)

    def close(self):
        try:
            os.kill(self.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            os.waitpid(self.pid, 0)
        except ChildProcessError:
            pass


KEYS = {
    "<enter>": "\r", "<esc>": "\x1b", "<tab>": "\t", "<backtab>": "\x1b[Z",
    "<up>": "\x1b[A", "<down>": "\x1b[B", "<right>": "\x1b[C", "<left>": "\x1b[D",
    "<space>": " ", "<f2>": "\x1bOQ", "<f4>": "\x1bOS", "<ctrl-q>": "\x11", "<ctrl-s>": "\x13",
}


# ---------------------------------------------------------------------------
# SVG


def colour(value, default):
    if value in (None, "default"):
        return default
    if re.fullmatch(r"[0-9a-fA-F]{6}", value):
        return "#" + value.lower()
    return ANSI.get(value, default)


def svg(screen: pyte.Screen, title: str, trim=True) -> str:
    cw, ch, pad, bar = 8.4, 18, 14, 30
    rows = screen.lines
    columns = screen.columns
    if trim:
        # Plain output: as wide as its longest line, within reason.
        used = max((x + 1 for y in range(rows) for x in range(screen.columns)
                    if screen.buffer[y][x].data.strip()), default=40)
        columns = max(64, min(screen.columns, used + 2))
    lines = [screen.buffer[y] for y in range(rows)]
    if trim:
        # Drop empty rows at the bottom, as plain command output leaves them.
        def blank(y):
            return all(lines[y][x].data == " " and colour(lines[y][x].bg, None) is None
                       for x in range(screen.columns))
        while rows > 1 and blank(rows - 1):
            rows -= 1
    width = columns * cw + 2 * pad
    height = rows * ch + 2 * pad + bar
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width:.0f} {height:.0f}" '
        f'width="{width:.0f}" height="{height:.0f}" role="img" aria-label="{html.escape(title)}">',
        "<style>text{font-family:'JetBrains Mono','Cascadia Mono','DejaVu Sans Mono',Menlo,Consolas,monospace;"
        "font-size:14px;white-space:pre}.b{font-weight:700}</style>",
        f'<rect width="100%" height="100%" rx="10" fill="{BASE}" stroke="{LINE}"/>',
        f'<circle cx="{pad + 6}" cy="{bar / 2 + 2}" r="5" fill="{LINE}"/>'
        f'<circle cx="{pad + 24}" cy="{bar / 2 + 2}" r="5" fill="{LINE}"/>'
        f'<circle cx="{pad + 42}" cy="{bar / 2 + 2}" r="5" fill="{LINE}"/>',
        f'<text x="{width / 2:.1f}" y="{bar / 2 + 7}" fill="{MUTED}" text-anchor="middle">{html.escape(title)}</text>',
    ]
    top = bar + pad
    for y in range(rows):
        row = lines[y]
        # Backgrounds, merged into runs.
        x = 0
        while x < columns:
            cell = row[x]
            bg = colour(cell.fg if cell.reverse else cell.bg, None) if not cell.reverse else colour(cell.fg, TEXT)
            start = x
            while x < columns:
                c = row[x]
                b = colour(c.bg, None) if not c.reverse else colour(c.fg, TEXT)
                if b != bg:
                    break
                x += 1
            if bg and bg != BASE:
                out.append(f'<rect x="{pad + start * cw:.1f}" y="{top + y * ch}" width="{(x - start) * cw + 0.5:.1f}" '
                           f'height="{ch + 0.5}" fill="{bg}"/>')
        # Text, in runs of one style; every run is pinned to its columns.
        x = 0
        while x < columns:
            cell = row[x]
            style = (colour(cell.fg, TEXT) if not cell.reverse else colour(cell.bg, BASE), cell.bold)
            start, text = x, ""
            while x < columns:
                c = row[x]
                s = (colour(c.fg, TEXT) if not c.reverse else colour(c.bg, BASE), c.bold)
                if s != style:
                    break
                text += c.data or " "
                x += 1
            if text.strip():
                cls = ' class="b"' if style[1] else ""
                out.append(
                    f'<text x="{pad + start * cw:.1f}" y="{top + y * ch + 14}" fill="{style[0]}"{cls} '
                    f'textLength="{len(text) * cw:.1f}" lengthAdjust="spacingAndGlyphs">{html.escape(text)}</text>')
    out.append("</svg>")
    return "\n".join(out) + "\n"


FORBIDDEN = [re.compile(p) for p in (r"helight", r"/home/", r"/phome", r"\bovh\b")]


def chrome() -> str:
    for candidate in (os.environ.get("CHROME"), "google-chrome", "google-chrome-stable",
                      "chromium", "chromium-browser", "chrome"):
        if candidate and shutil.which(candidate):
            return shutil.which(candidate)
    raise SystemExit("no Chrome or Chromium found: install one, or set CHROME to its path")


def render_png(svg_text: str, target: Path):
    """Render an SVG picture to a PNG at SCALE times its size, on a transparent
    background (the picture has rounded corners), with the mono font embedded."""
    size = re.search(r'viewBox="0 0 (\d+) (\d+)"', svg_text)
    if not size:
        raise SystemExit(f"{target.name}: the SVG has no viewBox")
    width, height = int(size[1]), int(size[2])
    faces = ""
    for weight, file in ((400, "JetBrainsMono-Regular.woff2"), (700, "JetBrainsMono-Bold.woff2")):
        path = FONTS / file
        if not path.is_file():
            raise SystemExit(f"missing font {path}")
        data = base64.b64encode(path.read_bytes()).decode()
        faces += (f"@font-face{{font-family:'JetBrains Mono';font-weight:{weight};"
                  f"src:url(data:font/woff2;base64,{data}) format('woff2')}}")
    page = (f"<!doctype html><meta charset=utf-8><style>{faces}"
            "html,body{margin:0;background:transparent}svg{display:block}</style>" + svg_text)
    with tempfile.TemporaryDirectory() as tmp:
        source, shot = Path(tmp) / "page.html", Path(tmp) / "shot.png"
        source.write_text(page)
        result = subprocess.run(
            [chrome(), "--headless", "--no-sandbox", "--disable-gpu", "--hide-scrollbars",
             "--disable-lcd-text", "--font-render-hinting=none",
             "--default-background-color=00000000", f"--force-device-scale-factor={SCALE}",
             f"--window-size={width},{height}", "--virtual-time-budget=3000",
             f"--screenshot={shot}", source.as_uri()],
            capture_output=True, text=True, timeout=120)
        if not shot.is_file():
            raise SystemExit(f"{target.name}: Chrome made no picture\n{result.stderr[-800:]}")
        optimise(shot, target)
    print(f"wrote {target.relative_to(REPO)}")


def optimise(source: Path, target: Path):
    """Smaller PNG: oxipng or pngquant when installed, else a plain re-save."""
    shutil.copyfile(source, target)
    if shutil.which("pngquant"):
        subprocess.run(["pngquant", "--force", "--quality=90-100", "--output", str(target), str(source)],
                       check=False)
    if shutil.which("oxipng"):
        subprocess.run(["oxipng", "-q", "-o", "4", "--strip", "safe", str(target)], check=False)


def save(name: str, screen: pyte.Screen, title: str, trim=True):
    text = svg(screen, title, trim)
    for pattern in FORBIDDEN:
        if pattern.search(text):
            raise SystemExit(f"{name}: the picture contains {pattern.pattern!r}; not saved")
    OUT.mkdir(parents=True, exist_ok=True)
    render_png(text, OUT / f"{name}.png")


# ---------------------------------------------------------------------------
# The pictures


def tui(env, name, args, keys, title, cols=120, rows=30, settle=1.5):
    term = Terminal([str(BERTH), *args], env, cols, rows, env["HOME"])
    try:
        term.pump(settle)
        for step in keys:
            if isinstance(step, (int, float)):
                term.pump(step)
            else:
                term.send([step])
        term.pump(0.8)
        save(name, term.screen, title, trim=False)
    finally:
        term.close()


def command(env, name, args, cols=96, rows=40, title=None, wait=6.0):
    """Plain output of a command, shown as typed at a prompt."""
    shown = "berth " + " ".join(args)
    script = f"printf '\\033[38;2;116;178;255m$\\033[0m %s\\n' {shlex_quote(shown)}; exec {BERTH} {' '.join(shlex_quote(a) for a in args)}"
    term = Terminal(["/bin/sh", "-c", script], env, cols, rows, env["HOME"])
    try:
        term.pump(wait)
        save(name, term.screen, title or shown)
    finally:
        term.close()


def shlex_quote(text):
    import shlex  # noqa: PLC0415
    return shlex.quote(text)


# Downs from the top of a host's Settings tab to the fourth row of its registries
# and secrets: past the six rows of the operator with its one key, the five of
# the agent, and SSH hardening.
SECRETS_DOWN = 15


def pictures(env):
    """Name: (needs the host, how to make it). The order matters where one
    picture shows what an earlier one left behind, such as sessions."""
    return {
        "home": (False, lambda: tui(env, "home", [], [], "berth", rows=20)),
        "home-plugins": (False, lambda: tui(env, "home-plugins", [], ["5"], "berth: plugins")),
        "features": (False, lambda: tui(env, "features", ["cloud-1"], ["2"], "berth cloud-1: features", rows=34)),
        "accounts": (False, lambda: tui(env, "accounts", ["cloud-1"], ["3"], "berth cloud-1: settings", rows=26)),
        "secrets": (False, lambda: tui(env, "secrets", ["cloud-1"], ["3"] + ["<down>"] * SECRETS_DOWN,
                                       "berth cloud-1: settings", rows=22)),
        "install-cloud": (False, lambda: tui(env, "install-cloud", ["cloud-1"], ["<down>", "<down>", "<enter>", "4"],
                                             "berth cloud-1: run", rows=34)),
        "list": (False, lambda: command(env, "list", ["list"])),
        "overview": (True, lambda: tui(env, "overview", ["studio"], [2.5, "<up>"], "berth studio", rows=46)),
        "terminal": (True, lambda: tui(env, "terminal", ["studio"],
                                       [2.5, "<enter>", 1.5, "id\r", "ls --color=never /srv/agent\r", 3.0],
                                       "berth studio: terminal", rows=20)),
        "tasks": (True, lambda: command(env, "tasks", ["tasks", "studio"])),
        "exec": (True, lambda: command(env, "exec", ["exec", "studio", "--", "id"])),
        "sessions": (True, lambda: command(env, "sessions", ["sessions", "studio"])),
        "host": (False, lambda: command(env, "host", ["host", "studio"])),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--only", nargs="*", help="make only these pictures")
    parser.add_argument("--demo", default="/tmp/berth-demo",
                        help="scratch directory for the invented set-up (it is replaced)")
    parser.add_argument("--keep", action="store_true", help="leave the demo container running")
    parser.add_argument("--list", action="store_true", help="list the pictures")
    parser.add_argument("--from-svg", metavar="DIR", help="only render the NAME.svg files in DIR to PNG")
    args = parser.parse_args()
    if pyte is None and not args.from_svg:
        raise SystemExit("pip install pyte pyyaml first (see the top of this file)")
    if args.from_svg:
        OUT.mkdir(parents=True, exist_ok=True)
        for file in sorted(Path(args.from_svg).glob("*.svg")):
            if args.only and file.stem not in args.only:
                continue
            text = file.read_text()
            for pattern in FORBIDDEN:
                if pattern.search(text):
                    raise SystemExit(f"{file.name}: the picture contains {pattern.pattern!r}; not rendered")
            render_png(text, OUT / f"{file.stem}.png")
        return
    root = Path(args.demo)
    real = Path.home() / ".config"
    if root.resolve() == Path.home() or real in root.resolve().parents:
        raise SystemExit("--demo must be a scratch directory, not your home or configuration")
    if not BERTH.is_file():
        raise SystemExit("build berth first: cargo build")
    env = make_demo(root)
    chosen = pictures(env)
    if args.list:
        print("\n".join(chosen))
        return
    names = args.only or list(chosen)
    connected = any(chosen[n][0] for n in names) and start_host(root)
    try:
        for name in names:
            needs_host, make = chosen[name]
            if needs_host and not connected:
                print(f"skipped {name}: no host")
                continue
            make()
    finally:
        if not args.keep:
            stop_host()


if __name__ == "__main__":
    main()
