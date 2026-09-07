# Installing QuantyDB

One file, no runtime, no service to configure. Download it, make it
executable, put it somewhere on your `PATH`.

There is no package in apt, AUR, homebrew or winget yet, and no install
script that pipes into a shell. Those come after the binaries have been
out long enough to be worth packaging.

## Which file

Every release attaches these, built and tested on the platform they name:

| File | For |
|---|---|
| `quanty-linux-x86_64` | Linux, glibc 2.35 or newer |
| `quanty-linux-x86_64-static` | Linux, anything older or unusual |
| `quanty-macos-arm64` | macOS on Apple silicon |
| `quanty-macos-x86_64` | macOS on Intel |
| `quanty-windows-x86_64.exe` | Windows |

Take `quanty-linux-x86_64` unless it refuses to start. The static one
exists for systems whose glibc is older than the one it was built
against, which announces itself with a message about `GLIBC_2.35` and
nothing else useful. It costs about 2.2x on the read path, measured, so
it is the fallback and not the default (ADR-040).

## Linux and macOS

```
curl -LO https://github.com/QuantyRoot/QuantyDatabase/releases/latest/download/quanty-linux-x86_64
chmod +x quanty-linux-x86_64
./quanty-linux-x86_64 about
sudo mv quanty-linux-x86_64 /usr/local/bin/quanty
```

Substitute the file name for your platform. On macOS the first run is
refused by Gatekeeper, because these binaries are not signed by an Apple
developer account and this project does not have one. Clear the quarantine
attribute yourself if you want to run it:

```
xattr -d com.apple.quarantine quanty-macos-arm64
```

## Windows

Download `quanty-windows-x86_64.exe`, rename it to `quanty.exe`, and put
it in a directory on your `PATH`. SmartScreen will warn about it for the
same reason Gatekeeper does.

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
cargo build --release -p quanty-cli
./target/release/quanty about
```

The binary lands in `target/release/quanty`.

## Updating

Download the new binary and point the tool at it:

```
curl -LO https://github.com/QuantyRoot/QuantyDatabase/releases/latest/download/quanty-linux-x86_64
quanty update --file quanty-linux-x86_64
```

It checks the file before it replaces anything: that the file is as long
as its own headers say it should be, so half a download is caught rather
than installed, and that it runs and reports a version. It prints that
version and the checksum and asks before going ahead. The binary it
replaces is kept next to the new one as `quanty.old`.

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

`quanty serve` is Linux only and says so when asked elsewhere. The reactor
has a kqueue backend and its tests run on macOS, but no kernel except
Linux spreads accepted connections across workers, so a macOS server would
run on one worker whatever shape it took (ADR-038, ADR-039). `quanty
connect` is a plain TCP client and runs everywhere, so a database served
from Linux can be used from anywhere.

## Uninstalling

Delete the binary. It writes nothing outside the database files you point
it at, keeps no configuration directory, and installs no service.
