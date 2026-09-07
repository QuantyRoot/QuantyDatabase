#!/bin/sh
# Build a signed apt repository out of .deb files that already exist.
#
#   packaging/apt-repo.sh <deb-dir> <out-dir>
#
# The layout is the plain one apt has understood for twenty years:
#
#   <out>/pool/main/q/quantydb/*.deb
#   <out>/dists/stable/main/binary-amd64/Packages{,.gz}
#   <out>/dists/stable/Release          the index, with hashes
#   <out>/dists/stable/Release.gpg      a detached signature of it
#   <out>/dists/stable/InRelease        the same thing, signed inline
#
# Both signatures are written because apt takes either, and which one it
# reaches for depends on how old it is.
#
# Signing needs a secret key already in the keyring; CI imports it from
# the GPG_PRIVATE_KEY secret first. Without one this builds the repository
# and says it did not sign it, rather than producing something apt will
# reject with a message about missing keys.
set -eu

debs=${1:?usage: apt-repo.sh <deb-dir> <out-dir>}
out=${2:?}

command -v apt-ftparchive > /dev/null 2>&1 \
    || { echo "this needs apt-ftparchive (apt-utils)" >&2; exit 1; }

suite=stable
component=main
arch=amd64

pool="$out/pool/$component/q/quantydb"
dist="$out/dists/$suite"
binary="$dist/$component/binary-$arch"

mkdir -p "$pool" "$binary"
found=0
for deb in "$debs"/*.deb; do
    [ -f "$deb" ] || continue
    cp "$deb" "$pool/"
    found=$((found + 1))
done
[ "$found" -gt 0 ] || { echo "no .deb files in $debs" >&2; exit 1; }
echo "$found package(s) in the pool"

# Paths inside Packages have to be relative to the repository root, which
# is what apt appends to the base URL, so this runs from there.
( cd "$out" && apt-ftparchive packages "pool/$component" > "dists/$suite/$component/binary-$arch/Packages" )
gzip -9 -k -f "$binary/Packages"

cat > "$out/apt-release.conf" <<EOF
APT::FTPArchive::Release::Origin "QuantyDB";
APT::FTPArchive::Release::Label "QuantyDB";
APT::FTPArchive::Release::Suite "$suite";
APT::FTPArchive::Release::Codename "$suite";
APT::FTPArchive::Release::Architectures "$arch";
APT::FTPArchive::Release::Components "$component";
APT::FTPArchive::Release::Description "QuantyDB packages";
EOF
( cd "$out" && apt-ftparchive -c apt-release.conf release "dists/$suite" > /tmp/Release.$$ )
mv "/tmp/Release.$$" "$dist/Release"
rm -f "$out/apt-release.conf"

if gpg --list-secret-keys > /dev/null 2>&1 && [ -n "$(gpg --list-secret-keys --with-colons 2>/dev/null | grep '^sec')" ]; then
    rm -f "$dist/Release.gpg" "$dist/InRelease"
    gpg --batch --yes --armor --detach-sign -o "$dist/Release.gpg" "$dist/Release"
    gpg --batch --yes --clearsign -o "$dist/InRelease" "$dist/Release"
    echo "signed"
else
    echo "NOT SIGNED: no secret key in the keyring" >&2
fi

echo "$out"
