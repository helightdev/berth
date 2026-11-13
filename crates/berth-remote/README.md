# berth-remote

berth-remote shows the hosts of a `berth serve` on another machine, in a terminal. It has the screens of the page that `berth serve --web` gives, and it runs on Windows, macOS and Linux.

It is only a client. Setting up hosts, Ansible and SSH stay on the server.

## Install

On Windows, in PowerShell:

```powershell
irm https://raw.githubusercontent.com/helightdev/berth/main/install.ps1 | iex
```

On macOS or Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/helightdev/berth/main/install-remote.sh | sh
```

Each script checks the download against its SHA-256. It installs to `%LOCALAPPDATA%\Programs\berth-remote` on Windows and to `~/.local/bin` elsewhere. `BERTH_INSTALL_DIR` and `BERTH_VERSION` change where and which release.

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
| `--forget` | Remove the kept server and token, and keep nothing from this run |
| `BERTH_REMOTE_FILE` | The plugin package to send, on a system without a file dialog |
| `--version` | Say the version |

A token or a setup key never goes on the command line, where other users of the machine could read it, and berth-remote does not pass either on to the programs it starts.

The settings are in `settings.json` in `%LOCALAPPDATA%\berth-remote` on Windows, `~/Library/Application Support/berth-remote` on macOS and `~/.config/berth-remote` elsewhere (`$XDG_CONFIG_HOME` where it is set). On macOS and Linux the folder is mode 0700 and the file 0600. On Windows the file takes the access list of your profile, which lets in you, SYSTEM and the administrators.

A server over https needs a certificate that the system or the Mozilla roots trust. There is no way to turn the check off. Only `http` and `https` links are opened, in the system's browser.

Ctrl+Q quits.

## How it is built

The screens are in `crates/berth-page`, which the page for a browser (`crates/berth-web`) uses too. This crate gives them a terminal in place of a browser: `device.rs` has the clipboard, links, the file dialog and the kept settings, `net.rs` and `socket.rs` the HTTP requests and the portal's WebSocket over TCP or TLS.

On the system it is for:

```bash
cargo build --release --locked -p berth-remote
```

On Windows, that needs Rust from rustup and the Visual Studio build tools. Use Windows Terminal or the console of Windows 10 or later, which draw its colours.
