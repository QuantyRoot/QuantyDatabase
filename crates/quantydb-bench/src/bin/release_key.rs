//! The key that signs releases, and the signing.
//!
//! `quantydb update` refuses a release unless its `SHA256SUMS` carries an
//! Ed25519 signature from the key whose public half is built into the
//! tool (ADR-047). This makes the pair and makes the signature.
//!
//! ```text
//! quantydb-release-key new <file>        write a new private key to <file>
//! quantydb-release-key public <file>     print the public half of <file>
//! quantydb-release-key sign <SHA256SUMS> write <SHA256SUMS>.sig
//! ```
//!
//! `sign` reads the private key from `QUANTYDB_RELEASE_KEY`, as hex, so
//! the release workflow can hand it over from a secret without it ever
//! being written to the runner's disk. A key is 32 random bytes from the
//! operating system; nothing about it is derived from anything else.

#[cfg(unix)]
fn main() {
    if let Err(e) = run(&std::env::args().skip(1).collect::<Vec<_>>()) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("release signing runs on the Linux release runner");
    std::process::exit(1);
}

#[cfg(unix)]
fn run(args: &[String]) -> Result<(), String> {
    use quantycore::tls::crypto::ed25519::{verify, SigningKey};

    match args {
        [cmd, file] if cmd == "new" => {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let key = SigningKey::generate().map_err(|e| format!("no randomness: {e}"))?;
            let mut out = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(file)
                .map_err(|e| format!("{file}: {e} (it is never overwritten)"))?;
            writeln!(out, "{}", to_hex(key.seed())).map_err(|e| format!("{file}: {e}"))?;
            println!("private  {file}, readable by this user only");
            println!("public   {}", to_hex(&key.public_key()));
            Ok(())
        }
        [cmd, file] if cmd == "public" => {
            let text = std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
            let key = SigningKey::from_seed(&seed(&text)?);
            println!("{}", to_hex(&key.public_key()));
            Ok(())
        }
        [cmd, sums] if cmd == "sign" => {
            let text = std::env::var("QUANTYDB_RELEASE_KEY")
                .map_err(|_| "QUANTYDB_RELEASE_KEY is not set; a release is never unsigned")?;
            let key = SigningKey::from_seed(&seed(&text)?);
            let message = std::fs::read(sums).map_err(|e| format!("{sums}: {e}"))?;
            let signature = key.sign(&message);
            // Checked before it is written: a signature that does not verify
            // would break every update, and this is the last place to see it.
            if !verify(&key.public_key(), &message, &signature) {
                return Err("the signature does not verify against its own key".into());
            }
            let out = format!("{sums}.sig");
            std::fs::write(&out, format!("{}\n", to_hex(&signature)))
                .map_err(|e| format!("{out}: {e}"))?;
            println!("signed   {sums}");
            println!("key      {}", to_hex(&key.public_key()));
            Ok(())
        }
        _ => {
            Err("usage: quantydb-release-key new <file> | public <file> | sign <SHA256SUMS>".into())
        }
    }
}

/// Lowercase hex of any length.
#[cfg(unix)]
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 32 bytes from 64 hex digits, surrounding whitespace allowed.
#[cfg(unix)]
fn seed(text: &str) -> Result<[u8; 32], String> {
    let text = text.trim();
    if text.len() != 64 {
        return Err("a private key is 64 hex digits".into());
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16)
            .map_err(|_| "a private key is 64 hex digits")?;
    }
    Ok(out)
}
