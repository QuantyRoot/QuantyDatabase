#!/bin/sh
# Install QuantyDB.
#
#   curl -fsSL https://quantyroot.github.io/QuantyDatabase/install.sh | sh
#
# Works out which file it wants, downloads it, checks it against the
# release's own SHA256SUMS, and puts it on the PATH. Nothing else: no
# configuration, no service, no directory anywhere. Run `quantydb setup`
# afterwards if you want a server.
#
# Set VERSION to install a particular one, and PREFIX to install
# somewhere other than /usr/local/bin.
set -eu

repo=QuantyRoot/QuantyDatabase
version=${VERSION:-latest}
prefix=${PREFIX:-/usr/local/bin}

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

need() {
    command -v "$1" > /dev/null 2>&1 || die "this needs $1 and cannot find it"
}
need curl
need uname

# The names on the release say what machine they are for, not what target
# triple built them, so this maps rather than guesses.
os=$(uname -s)
arch=$(uname -m)
case "$os/$arch" in
    Linux/x86_64|Linux/amd64)   asset=quantydb-linux-x86_64 ;;
    Linux/aarch64|Linux/arm64)  asset=quantydb-linux-arm64 ;;
    Darwin/arm64|Darwin/aarch64) asset=quantydb-macos-arm64 ;;
    Darwin/x86_64)              asset=quantydb-macos-x86_64 ;;
    *)
        die "no build for $os on $arch. Build from source: \
cargo build --release -p quantydb-cli" ;;
esac

if [ "$version" = latest ]; then
    base="https://github.com/$repo/releases/latest/download"
else
    base="https://github.com/$repo/releases/download/$version"
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

say "downloading $asset"
curl -fsSL "$base/$asset" -o "$work/$asset" \
    || die "could not download $base/$asset"

# The checksum file covers every binary in the release, so the download is
# checked against what was published rather than against itself.
if curl -fsSL "$base/SHA256SUMS" -o "$work/SHA256SUMS" 2>/dev/null; then
    if command -v sha256sum > /dev/null 2>&1; then
        got=$(sha256sum "$work/$asset" | cut -d' ' -f1)
    elif command -v shasum > /dev/null 2>&1; then
        got=$(shasum -a 256 "$work/$asset" | cut -d' ' -f1)
    else
        got=""
    fi
    want=$(grep "  $asset\$" "$work/SHA256SUMS" | cut -d' ' -f1 || true)
    if [ -n "$got" ] && [ -n "$want" ] && [ "$got" != "$want" ]; then
        die "checksum does not match: wanted $want, got $got"
    fi
    [ -n "$got" ] && [ -n "$want" ] && say "checksum ok"
else
    say "note: no SHA256SUMS in this release, so nothing was checked"
fi

chmod +x "$work/$asset"

# Run it before installing it. A file that does not start here is not one
# to put on the PATH.
"$work/$asset" about > /dev/null 2>&1 \
    || die "the downloaded file does not run on this machine"

target="$prefix/quantydb"
if [ -w "$prefix" ]; then
    mv "$work/$asset" "$target"
elif command -v sudo > /dev/null 2>&1; then
    say "$prefix needs root, using sudo"
    sudo mv "$work/$asset" "$target"
    sudo chmod 0755 "$target"
else
    die "cannot write to $prefix and there is no sudo. Set PREFIX to a \
directory you own: PREFIX=\$HOME/.local/bin"
fi

say ""
say "installed $("$target" about | head -1) at $target"
say ""
say "  quantydb setup     make a database, a token and a service"
say "  quantydb --help    everything else"
say ""
say "The server speaks plaintext: TLS is not written yet, so keep it on"
say "loopback or a network you trust."
