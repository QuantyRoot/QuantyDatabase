//! Getting a server started, and getting rid of it again.
//!
//! ## Why there is no manifest
//!
//! The tool keeps no configuration directory and writes nothing outside
//! the files it is pointed at. That is the property that makes removing it
//! easy, and a setup that recorded what it had done in a state file
//! somewhere would spend that property to buy back a worse version of it.
//!
//! So `setup` writes to **one well-known path** for the only thing that is
//! genuinely installed, the service unit, and `uninstall` looks there. The
//! unit itself says where the database and the token file are, on its
//! `ExecStart` line, so the two halves agree without a third file to keep
//! in step. Everything else `setup` makes is either a database, which is
//! data, or a token file, which is a secret: neither is an installation
//! and neither is removed by `uninstall`.
//!
//! ## What setup will not do
//!
//! It does not start anything. It writes the unit and prints the two
//! commands that would enable it, because a program that quietly starts a
//! network service during what the user thought was a questionnaire is a
//! surprise, and surprises on a server are expensive.
//!
//! It does not overwrite. An existing database is used as it is, an
//! existing token file is appended to, and an existing unit stops the
//! whole thing with a message rather than being replaced.

use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{emit, failed, Failure};

/// Where a system service goes.
///
/// One name rather than a manifest: this is how `uninstall` finds what
/// `setup` wrote.
const UNIT: &str = "/etc/systemd/system/quanty.service";

/// Walk through what a server needs and write it.
pub fn setup(
    database: Option<&str>,
    tokens: Option<&str>,
    listen: Option<&str>,
    service: Option<bool>,
    yes: bool,
) -> Result<(), Failure> {
    emit("This writes a database, a token file and optionally a service")?;
    emit("unit. It starts nothing and overwrites nothing.")?;
    emit("")?;

    let database = match database {
        Some(given) => PathBuf::from(given),
        None => PathBuf::from(ask("database file", "quanty.qdb", yes)?),
    };
    let tokens = match tokens {
        Some(given) => PathBuf::from(given),
        None => PathBuf::from(ask("token file", "quanty.tokens", yes)?),
    };
    let listen = match listen {
        Some(given) => given.to_string(),
        None => ask("listen address", "127.0.0.1:7878", yes)?,
    };

    let mut made: Vec<String> = Vec::new();

    if database.exists() {
        emit(&format!("using the database at {}", database.display()))?;
    } else {
        crate::create(&database)?;
        made.push(format!("{}", database.display()));
    }

    let (token, line) = quanty_auth::mint("first")
        .map_err(|e| failed(format!("could not read the system's randomness: {e}")))?;
    append_token(&tokens, &line)?;
    made.push(format!("{} (chmod 600)", tokens.display()));

    let wants_service = match service {
        Some(want) => want,
        None if cfg!(target_os = "linux") => asks("install a systemd service", false, yes)?,
        None => false,
    };
    if wants_service {
        write_unit(&database, &tokens, &listen)?;
        made.push(UNIT.to_string());
    }

    emit("")?;
    emit("  token")?;
    emit(&format!("  {token}"))?;
    emit("")?;
    emit("  That is the only copy. It is stored nowhere, here or anywhere")?;
    emit("  else: the token file keeps a hash of it and not the token.")?;
    emit("  Losing it means minting another with `quanty token <label>`.")?;
    emit("")?;

    emit("wrote:")?;
    for path in &made {
        emit(&format!("  {path}"))?;
    }
    emit("")?;

    if !listen_is_loopback(&listen) {
        emit("  !!  the wire is not encrypted, and this address is not")?;
        emit("  !!  loopback. Tokens cross it in the clear. Put it behind")?;
        emit("  !!  wireguard, an ssh tunnel or a TLS proxy.")?;
        emit("")?;
    }

    if wants_service {
        emit("to start it:")?;
        emit("  systemctl daemon-reload")?;
        emit("  systemctl enable --now quanty")?;
    } else {
        emit("to start it:")?;
        emit(&format!(
            "  quanty serve {} --listen {listen} --tokens {}",
            database.display(),
            tokens.display()
        ))?;
    }
    emit("")?;
    emit("to connect:")?;
    emit(&format!("  quanty connect {listen} --token <token>"))?;
    emit("")?;
    emit("`quanty uninstall` removes the service and the binary, and never")?;
    emit("touches the database or the token file.")
}

/// Take away what setup installed, and nothing that holds data.
///
/// The unit is only removed when it runs *this* binary. A copy of the tool
/// sitting in a downloads folder must not be able to take out the service
/// that runs the one in `/usr/local/bin`, and the unit says which binary it
/// runs, so it does not have to be guessed.
pub fn uninstall(yes: bool) -> Result<(), Failure> {
    let binary = std::env::current_exe()
        .map_err(|e| failed(format!("cannot tell where this binary lives: {e}")))?;
    let unit = Path::new(UNIT);

    emit("found:")?;
    let described = unit.exists().then(|| unit_says(unit)).flatten();
    let mut has_unit = false;
    if let Some(service) = &described {
        emit(&format!("  {UNIT}"))?;
        emit(&format!("    runs    {}", service.binary))?;
        emit(&format!("    serving {}", service.database))?;
        if let Some(tokens) = &service.tokens {
            emit(&format!("    tokens  {tokens}"))?;
        }
        has_unit = Path::new(&service.binary) == binary;
        if !has_unit {
            emit("    left alone: it runs a different binary than this one")?;
        }
    } else if unit.exists() {
        emit(&format!("  {UNIT}"))?;
        emit("    left alone: cannot tell which binary it runs")?;
    }
    let previous = binary.with_extension("old");
    let has_previous = previous.exists();
    if has_previous {
        emit(&format!("  {}", previous.display()))?;
    }
    emit(&format!("  {}", binary.display()))?;
    emit("")?;
    emit("the database and the token file are yours and stay where they")?;
    emit("are. Delete them yourself if you want them gone.")?;
    emit("")?;

    if !yes && !confirm("remove the service and the binary")? {
        return emit("left alone");
    }

    if has_unit {
        // Told to stop before the unit goes, or systemd is left holding a
        // service whose definition no longer exists.
        for args in [["disable", "--now"], ["reset-failed", ""]] {
            let mut command = Command::new("systemctl");
            for arg in args.iter().filter(|a| !a.is_empty()) {
                command.arg(arg);
            }
            let _ = command.arg("quanty").status();
        }
        fs::remove_file(unit).map_err(|e| {
            failed(format!(
                "cannot remove {UNIT}: {e}\nRun this again with the rights to change it."
            ))
        })?;
        let _ = Command::new("systemctl").arg("daemon-reload").status();
        emit(&format!("removed {UNIT}"))?;
    }

    if has_previous {
        let _ = fs::remove_file(&previous);
        emit(&format!("removed {}", previous.display()))?;
    }

    remove_self(&binary)
}

/// Unlink the running binary, where that is a thing that works.
#[cfg(unix)]
fn remove_self(binary: &Path) -> Result<(), Failure> {
    // The name goes now and the inode lives until this process exits,
    // which is why the rest of this function still runs.
    fs::remove_file(binary).map_err(|e| {
        failed(format!(
            "cannot remove {}: {e}\nRun this again with the rights to change it.",
            binary.display()
        ))
    })?;
    emit(&format!("removed {}", binary.display()))?;
    emit("")?;
    emit("that is all of it.")
}

/// Windows holds a running image open, so this one has to be asked for.
#[cfg(not(unix))]
fn remove_self(binary: &Path) -> Result<(), Failure> {
    emit("")?;
    emit(&format!(
        "delete {} yourself: Windows keeps a running program's file",
        binary.display()
    ))?;
    emit("open, so it cannot remove itself.")
}

/// What a unit's `ExecStart` says it runs.
struct Service {
    binary: String,
    database: String,
    tokens: Option<String>,
}

/// Read it back out of the unit, which is why there is no manifest.
fn unit_says(unit: &Path) -> Option<Service> {
    let text = fs::read_to_string(unit).ok()?;
    let line = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("ExecStart="))?;
    let mut words = line.split_whitespace();
    let binary = words.next()?.to_string();
    if words.next()? != "serve" {
        return None;
    }
    let database = words.next()?.to_string();
    let mut tokens = None;
    while let Some(word) = words.next() {
        if word == "--tokens" {
            tokens = words.next().map(|t| t.to_string());
        }
    }
    Some(Service {
        binary,
        database,
        tokens,
    })
}

/// Write a unit that runs as a person rather than as root.
fn write_unit(database: &Path, tokens: &Path, listen: &str) -> Result<(), Failure> {
    let unit = Path::new(UNIT);
    if unit.exists() {
        return Err(failed(format!(
            "{UNIT} already exists. Remove it with `quanty uninstall`, or \
             edit it, rather than having this overwrite it."
        )));
    }
    let binary = std::env::current_exe()
        .map_err(|e| failed(format!("cannot tell where this binary lives: {e}")))?;
    let user = std::env::var("SUDO_USER")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "root".to_string());
    let data = database
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ".".to_string());

    let text = format!(
        "[Unit]\n\
         Description=QuantyDB\n\
         After=network.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         User={user}\n\
         ExecStart={} serve {} --listen {listen} --tokens {}\n\
         Restart=on-failure\n\
         \n\
         # It answers a signal now, so systemd can stop it rather than\n\
         # killing it and leaving the next start to recover the file.\n\
         KillSignal=SIGTERM\n\
         TimeoutStopSec=30\n\
         \n\
         NoNewPrivileges=true\n\
         PrivateTmp=true\n\
         ProtectHome=true\n\
         ProtectSystem=strict\n\
         ReadWritePaths={data}\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        binary.display(),
        database.display(),
        tokens.display()
    );
    fs::write(unit, text).map_err(|e| {
        failed(format!(
            "cannot write {UNIT}: {e}\nRun this again with the rights to write there."
        ))
    })?;
    Ok(())
}

/// Append a token line, creating the file private if it is new.
fn append_token(path: &Path, line: &str) -> Result<(), Failure> {
    let existed = path.exists();
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| failed(format!("cannot write {}: {e}", path.display())))?;
    writeln!(file, "{line}").map_err(|e| failed(format!("writing {}: {e}", path.display())))?;
    drop(file);
    private(path)?;
    if existed {
        emit(&format!("added a token to {}", path.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn private(path: &Path) -> Result<(), Failure> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|e| failed(format!("cannot chmod {}: {e}", path.display())))
}

#[cfg(not(unix))]
fn private(_path: &Path) -> Result<(), Failure> {
    Ok(())
}

fn listen_is_loopback(listen: &str) -> bool {
    listen
        .parse::<std::net::SocketAddr>()
        .map(|a| a.ip().is_loopback())
        .unwrap_or(false)
}

/// Ask, with a default. End of input means the default, which is what
/// makes this scriptable without a second code path.
fn ask(what: &str, fallback: &str, yes: bool) -> Result<String, Failure> {
    if yes {
        emit(&format!("{what}: {fallback}"))?;
        return Ok(fallback.to_string());
    }
    print!("{what} [{fallback}]: ");
    std::io::stdout()
        .flush()
        .map_err(|e| failed(format!("stdout: {e}")))?;
    let mut answer = String::new();
    if std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|e| failed(format!("stdin: {e}")))?
        == 0
    {
        emit(fallback)?;
        return Ok(fallback.to_string());
    }
    let answer = answer.trim();
    Ok(if answer.is_empty() {
        fallback.to_string()
    } else {
        answer.to_string()
    })
}

fn asks(what: &str, fallback: bool, yes: bool) -> Result<bool, Failure> {
    let shown = if fallback { "Y/n" } else { "y/N" };
    if yes {
        return Ok(fallback);
    }
    print!("{what}? [{shown}]: ");
    std::io::stdout()
        .flush()
        .map_err(|e| failed(format!("stdout: {e}")))?;
    let mut answer = String::new();
    if std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|e| failed(format!("stdin: {e}")))?
        == 0
    {
        return Ok(fallback);
    }
    match answer.trim().to_ascii_lowercase().as_str() {
        "" => Ok(fallback),
        "y" | "yes" => Ok(true),
        _ => Ok(false),
    }
}

fn confirm(what: &str) -> Result<bool, Failure> {
    print!("{what}? [y/N] ");
    std::io::stdout()
        .flush()
        .map_err(|e| failed(format!("stdout: {e}")))?;
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|e| failed(format!("stdin: {e}")))?;
    let answer = answer.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}
