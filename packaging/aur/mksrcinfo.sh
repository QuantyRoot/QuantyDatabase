#!/bin/sh
# Write .SRCINFO from PKGBUILD.
#
# The AUR reads .SRCINFO and nothing else: the web page, the search and
# every helper take the version from there. A PKGBUILD updated without it
# shows the old version for ever and installs the new one, which is worse
# than either.
#
# makepkg --printsrcinfo does this on an Arch machine. This does the same
# thing from the fields, because CI is not an Arch machine and the file is
# a flat list rather than a format.
set -eu
cd "$(dirname "$0")"

# shellcheck disable=SC1091
. ./PKGBUILD

cat <<SRCINFO
pkgbase = $pkgname
	pkgdesc = $pkgdesc
	pkgver = $pkgver
	pkgrel = $pkgrel
	url = $url
	arch = ${arch[0]}
	license = ${license[0]}
	provides = ${provides[0]}
	conflicts = ${conflicts[0]}
	source = ${source[0]}
	sha256sums = ${sha256sums[0]}
	source = ${source[1]}
	sha256sums = ${sha256sums[1]}

pkgname = $pkgname
SRCINFO
