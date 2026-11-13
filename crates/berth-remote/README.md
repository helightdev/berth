# berth-remote

berth-remote shows the hosts of a `berth serve` on another machine, in a terminal. It has the screens of the page that `berth serve --web` gives, and it runs on Linux, macOS and Windows.

It is only a client. Setting up hosts, Ansible and SSH stay on the server.

## Install

On Windows, in PowerShell:

```powershell
irm https://raw.githubusercontent.com/helightdev/berth/main/install.ps1 | iex
```

The script checks the download against its SHA-256 and installs it to `%LOCALAPPDATA%\Programs\berth-remote`.

## Use

Start the server on an address of your network:

```bash
berth serve --listen 0.0.0.0:7878
```

Then on your own machine:

```bash
berth-remote --server http://192.0.2.10:7878
```

berth-remote asks for the token and keeps it for the next run. The token is in `serve.token` in the server's data directory. A setup key from `berth setup-key` works too.

| Option or variable | What it does |
| --- | --- |
| `--server ADDRESS`, `BERTH_SERVER` | The server. An address without a scheme is https, or http for `localhost` |
| `BERTH_TOKEN` | The token, so that it is not asked for |
| `BERTH_SETUP_KEY` | A setup key, which collects the token once |
| `--forget` | Keep the server and the token for this run only |
| `BERTH_REMOTE_FILE` | The plugin package to send, on a system without a file dialog |

A token or a setup key never goes on the command line, where other users of the machine could read it.

The settings are in `%APPDATA%\berth-remote\settings.json` on Windows and in `~/.config/berth-remote/settings.json` elsewhere. Only you can read the file.

Ctrl+Q quits.

## Build

On the system it is for:

```bash
cargo build --release --locked -p berth-remote
```

For Windows from Linux, with MinGW:

```bash
sudo apt-get install gcc-mingw-w64-x86-64
rustup target add x86_64-pc-windows-gnu
cargo build --release --locked -p berth-remote --target x86_64-pc-windows-gnu
```

The program is `target/x86_64-pc-windows-gnu/release/berth-remote.exe`. It needs nothing else installed.

On Windows itself, with Rust from rustup and the Visual Studio build tools, the first command builds `target\release\berth-remote.exe`.

Use Windows Terminal or the console of Windows 10 or later, which draw its colours.
