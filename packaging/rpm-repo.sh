#!/bin/sh
# Build a signed rpm repository around an already-built binary.
#
#   packaging/rpm-repo.sh <binary> <version> <arch> <out-dir>
#
# Two steps in one script because the rpm has to exist before the
# repository can index it, and neither is useful alone.
#
#   <out>/quantydb-<version>-1.<arch>.rpm
#   <out>/repodata/...
#
# rpm signs the package itself rather than the index, which is the other
# way round from apt. dnf checks that signature against the key named in
# quantydb.repo, so an unsigned package is refused with gpgcheck=1 even
# though the repository metadata is fine.
set -eu

binary=${1:?usage: rpm-repo.sh <binary> <version> <arch> <out-dir>}
version=${2:?}
arch=${3:?}
out=${4:?}

command -v rpmbuild > /dev/null 2>&1 || { echo "this needs rpmbuild" >&2; exit 1; }
command -v createrepo_c > /dev/null 2>&1 || { echo "this needs createrepo_c" >&2; exit 1; }
[ -f "$binary" ] || { echo "no such binary: $binary" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/BUILD" "$work/RPMS" "$work/SOURCES" "$work/SPECS" "$work/BUILDROOT"
cp "$binary" "$work/SOURCES/quantydb"

# Release is 1 and stays 1: a version is only ever built once here, and a
# rebuilt package with the same version is a different package wearing the
# same name.
cat > "$work/SPECS/quantydb.spec" <<EOF
Name:           quantydb
Version:        $version
Release:        1
Summary:        One database that reshapes itself into whatever you need
License:        MIT
URL:            https://github.com/QuantyRoot/QuantyDatabase
BuildArch:      $arch
# The binary is already built and already tested. Nothing is compiled
# here, so there is nothing to strip, debug or repack.
%global debug_package %{nil}
# rpm otherwise adds /usr/lib/.build-id symlinks nobody asked for, to a
# package that declares one file.
%define _build_id_links none
AutoReqProv:    no

%description
A single-file database with copy-on-write storage, branches and history,
a query language of its own and a SQL front end, full text search, and a
server mode. Written without a single third party dependency.

The tool works on any Linux; the server half of it is Linux only.

%install
install -D -m 0755 %{_sourcedir}/quantydb %{buildroot}%{_bindir}/quantydb

%files
%{_bindir}/quantydb

%changelog
EOF

rpmbuild --define "_topdir $work" -bb "$work/SPECS/quantydb.spec" > /dev/null
rpm_file=$(find "$work/RPMS" -name '*.rpm' | head -1)
[ -n "$rpm_file" ] || { echo "rpmbuild produced nothing" >&2; exit 1; }

mkdir -p "$out"
cp "$rpm_file" "$out/"
name=$(basename "$rpm_file")
echo "built $name"

key=$(gpg --list-secret-keys --with-colons 2>/dev/null | awk -F: '/^fpr/ {print $10; exit}')
if [ -n "${key:-}" ]; then
    # rpmsign shells out to gpg, and the macro is how it is told which key
    # and how to run it without asking for a passphrase on a terminal.
    rpmsign --define "_gpg_name $key" \
            --define "__gpg_sign_cmd %{__gpg} gpg --batch --pinentry-mode loopback --no-armor --no-secmem-warning -u \"%{_gpg_name}\" -sbo %{__signature_filename} %{__plaintext_filename}" \
            --addsign "$out/$name" > /dev/null
    echo "signed with $key"
else
    echo "NOT SIGNED: no secret key in the keyring" >&2
fi

createrepo_c --quiet "$out"
echo "$out"
