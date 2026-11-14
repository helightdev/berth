#!/bin/sh
# Installs berth-remote for this user on macOS or Linux: the released program
# for this machine, checked against its SHA-256, into ~/.local/bin (or
# BERTH_INSTALL_DIR). berth-remote uses the hosts of a berth serve elsewhere.
#
#   curl -fsSL https://raw.githubusercontent.com/helightdev/berth/main/install-remote.sh | sh
#
# BERTH_VERSION picks a release tag such as v0.5.0; the default is the latest.
set -eu

repo=helightdev/berth
dir=${BERTH_INSTALL_DIR:-$HOME/.local/bin}
version=${BERTH_VERSION:-latest}

fail() { printf 'install: %s\n' "$1" >&2; exit 1; }

case "$(uname -s)" in
  Linux) system=linux ;;
  Darwin) system=macos ;;
  *) fail "no released program for $(uname -s): on Windows use install.ps1" ;;
esac
case "$(uname -m)" in
  x86_64|amd64) arch=x86_64 ;;
  arm64|aarch64) arch=aarch64 ;;
  *) fail "no released program for $(uname -m): build it with cargo install --locked --git https://github.com/$repo berth-remote" ;;
esac
asset=berth-remote-$arch-$system.tar.gz
for tool in curl tar; do
  command -v "$tool" >/dev/null || fail "$tool is needed"
done
# macOS has shasum rather than sha256sum.
if command -v sha256sum >/dev/null; then
  check() { sha256sum -c "$1"; }
elif command -v shasum >/dev/null; then
  check() { shasum -a 256 -c "$1"; }
else
  fail "sha256sum or shasum is needed"
fi

if [ "$version" = latest ]; then
  url=https://github.com/$repo/releases/latest/download
else
  url=https://github.com/$repo/releases/download/$version
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
curl -fsSL -o "$work/$asset" "$url/$asset" || fail "could not download $url/$asset"
curl -fsSL -o "$work/$asset.sha256" "$url/$asset.sha256" || fail "could not download the checksum"
(cd "$work" && check "$asset.sha256" >/dev/null) || fail "the download does not match its checksum"
tar -xzf "$work/$asset" -C "$work" berth-remote
mkdir -p "$dir"
install -m 755 "$work/berth-remote" "$dir/berth-remote"

printf 'Installed %s to %s\n' "$("$dir/berth-remote" --version 2>/dev/null || echo berth-remote)" "$dir/berth-remote"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) printf 'Add %s to your PATH to run it as berth-remote.\n' "$dir" ;;
esac
