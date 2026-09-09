#!/usr/bin/env bash
set -euo pipefail

# Hosted Ubuntu uses this deb822 source file. Keep unrelated preinstalled
# vendor repositories out of both refresh and package selection. An explicit
# source file also supports controlled installer tests and custom CI images.
ubuntu_sources=${1:-/etc/apt/sources.list.d/ubuntu.sources}
if [[ "$ubuntu_sources" != /* || ! -r "$ubuntu_sources" ]]; then
  printf 'bubblewrap install requires a readable absolute Ubuntu source file: %s\n' "$ubuntu_sources" >&2
  exit 2
fi
apt_options=(-o "Dir::Etc::sourcelist=$ubuntu_sources" -o 'Dir::Etc::sourceparts=-')
sudo apt-get "${apt_options[@]}" update
sudo apt-get "${apt_options[@]}" install -y bubblewrap
bwrap --version
