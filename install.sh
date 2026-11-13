#!/bin/sh
# Installs berth for this user: the released program for this machine, checked
# against its SHA-256, into ~/.local/bin (or BERTH_INSTALL_DIR).
#
#   curl -fsSL https://raw.githubusercontent.com/helightdev/berth/main/install.sh | sh
#
# BERTH_VERSION picks a release tag such as v0.2.0; the default is the latest.
set -eu

repo=helightdev/berth
dir=${BERTH_INSTALL_DIR:-$HOME/.local/bin}
version=${BERTH_VERSION:-latest}
# BERTH_VARIANT=web takes the build that carries the page for a browser.
variant=${BERTH_VARIANT:-}

fail() { printf 'install: %s\n' "$1" >&2; exit 1; }

[ "$(uname -s)" = Linux ] || fail "berth runs on Linux; on another system use the container image ghcr.io/$repo, or install-remote.sh for berth-remote, which uses a berth server elsewhere"
case "$(uname -m)" in
  x86_64|amd64) arch=x86_64 ;;
  *) fail "no released program for $(uname -m): build it with cargo install --locked --git https://github.com/$repo berth" ;;
esac
case "$variant" in
  "") asset=berth-$arch-linux.tar.gz ;;
  web) asset=berth-web-$arch-linux.tar.gz ;;
  *) fail "no variant $variant: leave BERTH_VARIANT unset, or set it to web" ;;
esac
for tool in curl tar sha256sum; do
  command -v "$tool" >/dev/null || fail "$tool is needed"
done

if [ "$version" = latest ]; then
  url=https://github.com/$repo/releases/latest/download
else
  url=https://github.com/$repo/releases/download/$version
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
curl -fsSL -o "$work/$asset" "$url/$asset" || fail "could not download $url/$asset"
curl -fsSL -o "$work/$asset.sha256" "$url/$asset.sha256" || fail "could not download the checksum"
(cd "$work" && sha256sum -c "$asset.sha256" >/dev/null) || fail "the download does not match its checksum"
tar -xzf "$work/$asset" -C "$work" berth
mkdir -p "$dir"
install -m 755 "$work/berth" "$dir/berth"

printf 'Installed %s to %s\n' "$("$dir/berth" --version 2>/dev/null || echo berth)" "$dir/berth"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) printf 'Add %s to your PATH to run it as berth.\n' "$dir" ;;
esac
missing=
for tool in ssh python3 ansible-playbook; do
  command -v "$tool" >/dev/null || missing="$missing $tool"
done
[ -z "$missing" ] || printf 'berth also uses:%s. Install them before setting up a host.\n' "$missing"
