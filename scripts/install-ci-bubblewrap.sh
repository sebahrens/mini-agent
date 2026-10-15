#!/usr/bin/env bash
set -euo pipefail

# Hosted Ubuntu uses this deb822 source file. Keep unrelated preinstalled
# vendor repositories out of both refresh and package selection. An explicit
# source file also supports controlled installer tests and custom CI images.
pinned_native=0
if [[ "${1:-}" == --pinned-native ]]; then
  pinned_native=1
  shift
fi
# Ubuntu 22.04 retains sources.list; newer images use deb822 ubuntu.sources.
default_sources=/etc/apt/sources.list.d/ubuntu.sources
if [[ "$pinned_native" -eq 1 && ! -r "$default_sources" ]]; then
  default_sources=/etc/apt/sources.list
fi
ubuntu_sources=${1:-$default_sources}
if [[ "$ubuntu_sources" != /* || ! -r "$ubuntu_sources" ]]; then
  printf 'bubblewrap install requires a readable absolute Ubuntu source file: %s\n' "$ubuntu_sources" >&2
  exit 2
fi
apt_options=(-o "Dir::Etc::sourcelist=$ubuntu_sources" -o 'Dir::Etc::sourceparts=-')
sudo apt-get "${apt_options[@]}" update
if [[ "$pinned_native" -eq 0 ]]; then
  sudo apt-get "${apt_options[@]}" install -y bubblewrap
  bwrap --version
  exit 0
fi

# Source pin is required for BOTH descriptor-bind options and idempotent
# readonly spelling links. Do not change the host's namespace/security policy.
version=0.11.0
archive_sha256=cfeeb15fcc47d177d195f06fdf0847e93ee3aa6bf46f6ac0a141fa142759e2c3
sudo apt-get "${apt_options[@]}" install -y \
  build-essential meson ninja-build pkg-config libcap-dev ca-certificates curl
build_root=$(mktemp -d)
trap 'rm -rf -- "$build_root"' EXIT
archive="$build_root/bubblewrap.tar.gz"
curl --disable --fail --location --proto '=https' --proto-redir '=https' \
  --tlsv1.2 --connect-timeout 15 --max-time 120 --retry 2 --retry-max-time 240 \
  "https://github.com/containers/bubblewrap/archive/refs/tags/v${version}.tar.gz" \
  --output "$archive"
printf '%s  %s\n' "$archive_sha256" "$archive" | sha256sum --check --status
mkdir "$build_root/source"
tar -xzf "$archive" --strip-components=1 -C "$build_root/source"
meson setup "$build_root/build" "$build_root/source" --prefix=/usr \
  -Dtests=false -Dman=disabled -Dbash_completion=disabled -Dzsh_completion=disabled
ninja -C "$build_root/build" bwrap
# Only the verified compiled image is installed with root ownership. No
# setuid bit, privilege policy edits, or upstream install scripts run as root.
sudo install -o root -g root -m 0755 -- "$build_root/build/bwrap" /usr/bin/bwrap
bwrap --version
bwrap --bind-fd 0 / --ro-bind-fd 0 / --version
