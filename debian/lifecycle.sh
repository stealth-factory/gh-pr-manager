#!/bin/bash
# Install, upgrade and remove the .deb. Ported from opscope's
# debian/lifecycle.sh. Run only in a disposable Linux container as root.
# Never on a workstation.
set -euo pipefail
package=$(readlink -f "$1")
mode=${2:-supported}
version=$(dpkg-deb -f "$package" Version)
test "$(dpkg-deb -f "$package" Package)" = gh-pr-manager
test "$(dpkg-deb -f "$package" Architecture)" = amd64
test "$(dpkg-deb -f "$package" Depends)" = 'libc6 (>= 2.35), libgcc-s1'
apt-get update -qq
if [ "$mode" = old-glibc ]; then
  if apt-get install -y "$package"; then
    echo 'ERROR: package installed below its glibc floor' >&2
    exit 1
  fi
  test ! -e /usr/bin/gh-pr-manager
  exit 0
fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# Upgrade from an older build of the same package.
dpkg-deb -R "$package" "$work/old"
sed -i 's/^Version:.*/Version: 0.0.0/' "$work/old/DEBIAN/control"
dpkg-deb --root-owner-group --build "$work/old" "$work/old.deb"
apt-get install -y "$work/old.deb"
mkdir -p /root/.config/gh-pr-manager
printf 'refresh_secs = 60\n' > /root/.config/gh-pr-manager/config.toml
apt-get install -y "$package"
test "$(dpkg-query -W -f='${Version}' gh-pr-manager)" = "$version"
stamp=$(gh-pr-manager --version)
case "$stamp" in "gh-pr-manager $version ("*) ;; *) echo "bad stamp: $stamp" >&2; exit 1;; esac
gh-pr-manager --help > "$work/help"
grep -qi 'pull request' "$work/help"
dpkg -V gh-pr-manager
dpkg -L gh-pr-manager | while read -r file; do [ ! -f "$file" ] || echo "$file"; done > "$work/files"
dpkg -r gh-pr-manager
while read -r file; do test ! -e "$file"; done < "$work/files"
# User settings are not the package's to remove.
test "$(cat /root/.config/gh-pr-manager/config.toml)" = 'refresh_secs = 60'
# A clean install, separately from the upgrade.
apt-get install -y "$package"
gh-pr-manager --version
dpkg -r gh-pr-manager
