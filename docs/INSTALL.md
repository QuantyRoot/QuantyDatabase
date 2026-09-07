# Installing QuantyDB

One file, no runtime, no service to configure. Download it, make it
executable, put it somewhere on your `PATH`.

The fastest way, on Linux or macOS:

```
curl -fsSL https://quantyroot.github.io/QuantyDatabase/install.sh | sh
```

It works out which file it wants, checks it against the release's own
`SHA256SUMS`, runs it once before installing it, and puts it in
`/usr/local/bin`. Set `PREFIX` to install somewhere you own and `VERSION`
to pin one. It writes nothing else.

Homebrew, on macOS or Linux:

```
brew tap quantyroot/quantydb https://github.com/QuantyRoot/QuantyDatabase
brew install quantydb
```

Debian, Ubuntu and anything else with apt:

```
sudo apt install ./quantydb_0.4.0_amd64.deb
```

Fedora, RHEL and anything else with dnf:

```
sudo dnf install ./quantydb-0.4.0.x86_64.rpm
```

The AUR one is written and cannot be submitted: registration on
aur.archlinux.org has been paused since September 2026 while they deal
with a wave of automated account creation, and submitting a package needs
an account. It goes up when registration reopens, which is announced on
aur-general and the Arch news feed and nowhere else worth polling.

## Which file

Every release attaches these, built and tested on the platform they name:

| File | For |
|---|---|
| `quantydb-linux-x86_64` | Linux, glibc 2.35 or newer |
| `quantydb-linux-x86_64-static` | Linux, anything older or unusual |
| `quantydb-macos-arm64` | macOS on Apple silicon |
| `quantydb-macos-x86_64` | macOS on Intel |
| `quantydb-windows-x86_64.exe` | Windows |

Take `quantydb-linux-x86_64` unless it refuses to start. The static one
exists for systems whose glibc is older than the one it was built
against, which announces itself with a message about `GLIBC_2.35` and
nothing else useful. It costs about 2.2x on the read path, measured, so
it is the fallback and not the default (ADR-040).

## Linux and macOS

```
curl -LO https://github.com/QuantyRoot/QuantyDatabase/releases/latest/download/quantydb-linux-x86_64
chmod +x quantydb-linux-x86_64
./quantydb-linux-x86_64 about
sudo mv quantydb-linux-x86_64 /usr/local/bin/quantydb
```

Substitute the file name for your platform. On macOS the first run is
refused by Gatekeeper, because these binaries are not signed by an Apple
developer account and this project does not have one. Clear the quarantine
attribute yourself if you want to run it:

```
xattr -d com.apple.quarantine quantydb-macos-arm64
```

## Windows

Download `quantydb-windows-x86_64.exe`, rename it to `quantydb.exe`, and put
it in a directory on your `PATH`. SmartScreen will warn about it for the
same reason Gatekeeper does.

## Using it

[USING.md](USING.md) is the tour, from an empty file to a running server.

## Setting up a server

```
quantydb setup
```

It asks where the database should live, where the token file goes and
what address to listen on, and offers to write a systemd unit. Every
question has a default; pressing return takes it. It writes the files,
prints the token once and prints the exact command to start the server.
It starts nothing itself.

Give it the answers up front to skip the questions:

```
quantydb setup /var/lib/quantydb/main.qdb --tokens /etc/quantydb/tokens --service --yes
```

The token is printed once and stored nowhere: the token file keeps a hash
of it. Losing it means minting another with `quantydb token <label>`.

## Checking what you downloaded

Each release carries a `SHA256SUMS` file covering every binary in it.

```
sha256sum -c SHA256SUMS --ignore-missing
```

On macOS, `shasum -a 256 -c SHA256SUMS --ignore-missing`.

## From source

Needs Rust 1.89 or newer and nothing else. There is no C toolchain to
install, no system library to find, and the lock file holds this
workspace and not one package besides.

```
git clone https://github.com/QuantyRoot/QuantyDatabase.git
cd QuantyDatabase
cargo build --release -p quantydb-cli
./target/release/quantydb about
```

The binary lands in `target/release/quantydb`.

## Updating

Download the new binary and point the tool at it:

```
curl -LO https://github.com/QuantyRoot/QuantyDatabase/releases/latest/download/quantydb-linux-x86_64
quantydb update --file quantydb-linux-x86_64
```

It checks the file before it replaces anything: that the file is as long
as its own headers say it should be, so half a download is caught rather
than installed, and that it runs and reports a version. It prints that
version and the checksum and asks before going ahead. The binary it
replaces is kept next to the new one as `quantydb.old`.

Pass `--sha256 <hex>` from the release's `SHA256SUMS` to require a
particular file rather than merely a whole one, and `--yes` to skip the
question.

It does not fetch anything yet. Pulling a release off GitHub needs HTTPS,
and this project writes what it depends on (ADR-020), so the network path
arrives with TLS (ADR-042).

If the tool lives somewhere this user cannot write, such as
`/usr/local/bin`, run the update with the rights to change it. It will not
reach for `sudo` on its own.

## What runs where

The library, the tool and both query front ends build and are tested on
Linux, macOS and Windows, on every push.

`quantydb serve` is Linux only and says so when asked elsewhere. The reactor
has a kqueue backend and its tests run on macOS, but no kernel except
Linux spreads accepted connections across workers, so a macOS server would
run on one worker whatever shape it took (ADR-038, ADR-039). `quantydb
connect` is a plain TCP client and runs everywhere, so a database served
from Linux can be used from anywhere.

## Uninstalling

```
quantydb uninstall
```

It lists what it found, asks, then stops and removes the service unit and
the binary. It never removes a database or a token file: those are yours,
and it tells you where they are so you can decide.

It only removes a service unit that runs the binary you are asking, so a
copy sitting in a downloads folder cannot take out the one you installed.

By hand it is the same short list: the binary, `/etc/systemd/system/quantydb.service`
if you asked for one, and whatever database and token files you made.
There is no configuration directory and nothing else on disk.
