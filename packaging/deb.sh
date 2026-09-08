#!/bin/sh
# Build a .deb around an already-built binary.
#
#   packaging/deb.sh <binary> <version> <arch> <output-dir>
#
# The binary is built by the release workflow, tested on the runner that
# built it, and then wrapped here. Nothing is compiled in this script: if
# it built and passed, that is the file that goes in the package.
#
# Debian's architecture names are not the target triple's. amd64, not
# x86_64, and arm64, not aarch64.
set -eu

binary=${1:?usage: deb.sh <binary> <version> <arch> <output-dir>}
version=${2:?}
arch=${3:?}
out=${4:?}

[ -f "$binary" ] || { echo "no such binary: $binary" >&2; exit 1; }

root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
# mktemp gives 700, and that mode ends up on the package's own root
# directory, where it would be unpacked over / on installation.
chmod 0755 "$root"

install -D -m 0755 "$binary" "$root/usr/bin/quantydb"
install -d "$root/usr/share/doc/quantydb"
install -d "$root/DEBIAN"

# Installed-Size is in kibibytes and apt shows it before downloading, so
# it is worth being true rather than absent.
size=$(du -ks "$root/usr" | cut -f1)

cat > "$root/DEBIAN/control" <<EOF
Package: quantydb
Version: $version
Section: database
Priority: optional
Architecture: $arch
Maintainer: Elchi <github@elchi.dev>
Installed-Size: $size
Homepage: https://github.com/QuantyRoot/QuantyDatabase
Description: One database that reshapes itself into whatever you need
 A single-file database with copy-on-write storage, branches and history,
 a query language of its own and a SQL front end, full text search, and a
 server mode. Written without a single third party dependency.
 .
 The tool works on any Linux; the server half of it is Linux only.
EOF

cat > "$root/usr/share/doc/quantydb/copyright" <<'EOF'
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: QuantyDB
Source: https://github.com/QuantyRoot/QuantyDatabase

Files: *
Copyright: Elchi <github@elchi.dev>
License: MIT
EOF

# Lintian wants a changelog and apt does not care; the release notes live
# in CHANGELOG.md and are not in Debian's format, so this stays out rather
# than shipping something that claims to be what it is not.

mkdir -p "$out"
name="quantydb_${version}_${arch}.deb"
dpkg-deb --root-owner-group --build "$root" "$out/$name" > /dev/null
echo "$out/$name"
