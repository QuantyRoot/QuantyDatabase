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

Download the new file over the old one. There is no `quanty update` and
no self-updater: a database that rewrites its own binary is a way to lose
an afternoon, and once there are packages, the package manager is the
thing that should be doing this.

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
