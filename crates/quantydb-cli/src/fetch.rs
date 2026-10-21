//! Getting a release over the network.
//!
//! This is the half ADR-042 left waiting for TLS. It produces bytes and
//! hands them to the same install path `update --file` uses, so the
//! checks that protect the running binary are exactly the ones that
//! already have tests.
//!
//! ## What is trusted, and for what
//!
//! The connection is TLS from QuantyCore, validated against the system's
//! certificate bundle. That proves the bytes came from GitHub. The release
//! publishes `SHA256SUMS`, and the binary is refused unless its digest
//! matches the line for it, which proves the file is the one that was
//! published and not a cut or damaged copy. `SHA256SUMS.sig` proves who
//! published it: an Ed25519 signature over the checksum file, checked
//! against [`RELEASE_KEY`], which is built into this binary (ADR-047).
//! Nothing from the release is trusted before that check passes.
//!
//! The TLS is unreviewed, which QuantyCore's own ADR-003 asks every
//! product using it to say, and `update` says so when it runs.
//!
//! ## What it will not do
//!
//! Follow a redirect to plain HTTP, follow more than a few, or read more
//! than [`MAX_BODY`] bytes into memory. GitHub answers a release download
//! with two redirects, the second to a storage host whose name it chooses,
//! so the host is not pinned: what is pinned is that every hop is HTTPS
//! with a certificate that validates for that host.

use std::io::Read;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use quantycore::http1::{self, Request, Url};
use quantycore::tls::x509::RootStore;
use quantycore::tls::{ClientConfig, ClientConnection, Stream};

/// Where releases come from. The `latest` form is the one `install.sh`
/// uses, so the two always agree on what "the newest release" means.
pub const RELEASES: &str = "https://github.com/QuantyRoot/QuantyDatabase/releases";

/// The public half of the key that signs releases. `None` in a build made
/// before the key existed, and then fetching refuses rather than trusting
/// a checksum file anyone holding the repository could have written.
pub const RELEASE_KEY: Option<[u8; 32]> = None;

/// Check `sig`, the text of `SHA256SUMS.sig`, over `sums`, the exact bytes
/// of `SHA256SUMS`, against `key`.
pub fn verify_sums(sums: &[u8], sig: &str, key: &[u8; 32]) -> Result<(), String> {
    let sig = sig.trim();
    if sig.len() != 128 || !sig.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("SHA256SUMS.sig is not a signature".into());
    }
    let mut bytes = [0u8; 64];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&sig[2 * i..2 * i + 2], 16)
            .map_err(|_| "SHA256SUMS.sig is not a signature")?;
    }
    if quantycore::tls::crypto::ed25519::verify(key, sums, &bytes) {
        Ok(())
    } else {
        Err(
            "SHA256SUMS is not signed by the release key. It may have been \
             changed after it was published; nothing from this release is \
             installed."
                .into(),
        )
    }
}

/// The largest response read into memory. Release binaries are about two
/// megabytes; this leaves room to grow without letting a misbehaving
/// server fill the machine.
pub const MAX_BODY: usize = 64 << 20;

/// Redirects followed before giving up. GitHub uses two.
const MAX_REDIRECTS: usize = 5;

/// How long a connection may sit without progress.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The release asset built for this machine, named the way `install.sh`
/// names it: by machine, not by target triple.
pub fn asset_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        // The static build is the one for machines too old for the other,
        // so a static binary updates to a static binary.
        ("linux", "x86_64") if cfg!(target_env = "musl") => Some("quantydb-linux-x86_64-static"),
        ("linux", "x86_64") => Some("quantydb-linux-x86_64"),
        ("linux", "aarch64") => Some("quantydb-linux-arm64"),
        ("macos", "aarch64") => Some("quantydb-macos-arm64"),
        ("macos", "x86_64") => Some("quantydb-macos-x86_64"),
        ("windows", "x86_64") => Some("quantydb-windows-x86_64.exe"),
        _ => None,
    }
}

/// The line for `asset` in a `SHA256SUMS` file, as lowercase hex.
///
/// The format is what `sha256sum` writes: the digest, a space, a space or
/// a star for binary mode, and the name. Anything else on the line, or a
/// name listed twice, is refused rather than guessed at.
pub fn digest_for(sums: &str, asset: &str) -> Result<String, String> {
    let mut found = None;
    for line in sums.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let (digest, rest) = line
            .split_once(' ')
            .ok_or_else(|| format!("SHA256SUMS has a line it cannot read: {line:?}"))?;
        let name = rest
            .strip_prefix(' ')
            .or_else(|| rest.strip_prefix('*'))
            .ok_or_else(|| format!("SHA256SUMS has a line it cannot read: {line:?}"))?;
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("SHA256SUMS has a digest that is not one: {line:?}"));
        }
        if name == asset {
            if found.is_some() {
                return Err(format!("SHA256SUMS lists {asset} twice"));
            }
            found = Some(digest.to_ascii_lowercase());
        }
    }
    found.ok_or_else(|| format!("SHA256SUMS has no line for {asset}"))
}

/// Fetch `url` over HTTPS and return the body, following redirects.
///
/// `roots` is passed in rather than read here so a test can trust a
/// certificate of its own, and so the bundle is read once per update
/// rather than once per file.
pub fn get(url: &str, roots: &Arc<ClientConfig>) -> Result<Vec<u8>, String> {
    let mut url = http1::parse_url(url).map_err(|e| format!("{url}: {e}"))?;
    for _ in 0..=MAX_REDIRECTS {
        if url.scheme != "https" {
            return Err(format!(
                "refusing to follow a redirect to {}://{}: only https is fetched",
                url.scheme, url.host
            ));
        }
        let (status, location, body) = once(&url, roots)?;
        match status {
            200 => return Ok(body),
            301 | 302 | 303 | 307 | 308 => {
                let location = location
                    .ok_or_else(|| format!("{status} from {} without a Location", url.host))?;
                url = http1::resolve(&url, &location)
                    .map_err(|e| format!("redirect to {location:?}: {e}"))?;
            }
            404 => return Err(format!("{}{} does not exist (404)", url.host, url.target)),
            other => return Err(format!("{} answered {other}", url.host)),
        }
    }
    Err(format!("more than {MAX_REDIRECTS} redirects"))
}

/// One request on one connection. A connection per hop is simpler than
/// keeping one alive, and every hop after the first is a different host.
fn once(url: &Url, config: &Arc<ClientConfig>) -> Result<(u16, Option<String>, Vec<u8>), String> {
    let tcp = TcpStream::connect((url.host.as_str(), url.port))
        .map_err(|e| format!("cannot reach {}: {e}", url.host))?;
    let _ = tcp.set_read_timeout(Some(TIMEOUT));
    let _ = tcp.set_write_timeout(Some(TIMEOUT));
    let conn = ClientConnection::new(config.clone(), &url.host)
        .map_err(|e| format!("{}: {e}", url.host))?;
    let mut stream = Stream::new(conn, tcp);
    stream
        .handshake()
        .map_err(|e| format!("TLS with {} failed: {e}", url.host))?;

    let agent = concat!("quantydb/", env!("CARGO_PKG_VERSION"));
    let request = Request {
        method: "GET",
        target: &url.target,
        host: &url.authority(),
        headers: &[("User-Agent", agent), ("Accept", "*/*")],
    };
    let mut response =
        http1::send(&mut stream, &request).map_err(|e| format!("{}: {e}", url.host))?;
    let status = response.status;
    let location = response.header("location").map(str::to_string);
    if (300..400).contains(&status) {
        return Ok((status, location, Vec::new()));
    }
    let mut body = Vec::new();
    (&mut response)
        .take(MAX_BODY as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|e| format!("reading from {}: {e}", url.host))?;
    if body.len() > MAX_BODY {
        return Err(format!("{} sent more than {MAX_BODY} bytes", url.host));
    }
    Ok((status, location, body))
}

/// A client configuration trusting the system's certificate bundle.
pub fn system_config() -> Result<Arc<ClientConfig>, String> {
    let roots = RootStore::system().map_err(|e| {
        format!(
            "{e}. Fetching a release needs the system's CA certificates; \
             on Linux that is the ca-certificates package. `update --file` \
             works without them."
        )
    })?;
    Ok(Arc::new(ClientConfig::new(roots)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUMS: &str = "\
1111111111111111111111111111111111111111111111111111111111111111  quantydb-linux-x86_64
2222222222222222222222222222222222222222222222222222222222222222  quantydb-linux-x86_64-static
AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA *quantydb-windows-x86_64.exe
";

    #[test]
    fn the_line_for_the_asset_and_not_its_neighbour() {
        assert_eq!(
            digest_for(SUMS, "quantydb-linux-x86_64").unwrap(),
            "1".repeat(64)
        );
        assert_eq!(
            digest_for(SUMS, "quantydb-linux-x86_64-static").unwrap(),
            "2".repeat(64)
        );
    }

    #[test]
    fn binary_mode_and_uppercase_are_read() {
        assert_eq!(
            digest_for(SUMS, "quantydb-windows-x86_64.exe").unwrap(),
            "a".repeat(64)
        );
    }

    #[test]
    fn a_missing_asset_is_an_error_and_not_an_empty_digest() {
        assert!(digest_for(SUMS, "quantydb-plan9-mips").is_err());
    }

    #[test]
    fn a_name_listed_twice_is_refused() {
        let twice = format!("{SUMS}{}  quantydb-linux-x86_64\n", "3".repeat(64));
        assert!(digest_for(&twice, "quantydb-linux-x86_64").is_err());
    }

    #[test]
    fn a_short_digest_is_refused() {
        assert!(digest_for("abc  quantydb-linux-x86_64\n", "quantydb-linux-x86_64").is_err());
    }

    /// A server of our own on loopback, with a certificate of our own,
    /// answering a fixed table of paths. Every connection gets one
    /// response, which is all the client asks for.
    mod local {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::Arc;

        use quantycore::tls::crypto::ed25519::SigningKey;
        use quantycore::tls::x509::build::{issue, Issuer, Params, SanEntry};
        use quantycore::tls::x509::{PrivateKey, RootStore};
        use quantycore::tls::{ClientConfig, ServerConfig, ServerConnection, Stream};

        pub struct Server {
            pub port: u16,
            pub client: Arc<ClientConfig>,
        }

        fn now() -> i64 {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
        }

        pub fn start(routes: Vec<(&'static str, String)>) -> Server {
            let day = 86_400;
            let root_key = PrivateKey::Ed25519(SigningKey::from_seed(&[7; 32]));
            let root = issue(
                &Params::ca("test root", now() - day, now() + day),
                &root_key.public_spki(),
                None,
                &root_key,
            )
            .unwrap();
            let leaf_key = PrivateKey::Ed25519(SigningKey::from_seed(&[8; 32]));
            let leaf = issue(
                &Params::end_entity(
                    "localhost",
                    vec![SanEntry::Dns("localhost".into())],
                    now() - day,
                    now() + day,
                ),
                &leaf_key.public_spki(),
                Some(&Issuer::from_certificate(&root, &root_key).unwrap()),
                &leaf_key,
            )
            .unwrap();
            let server = Arc::new(ServerConfig::new(vec![leaf, root.clone()], leaf_key).unwrap());
            let mut roots = RootStore::new();
            roots.add_der(&root).unwrap();

            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                for tcp in listener.incoming().flatten() {
                    let server = server.clone();
                    let routes = routes.clone();
                    std::thread::spawn(move || {
                        let mut s = Stream::new(ServerConnection::new(server), tcp);
                        if s.handshake().is_err() {
                            return;
                        }
                        let mut head = Vec::new();
                        let mut byte = [0u8; 1];
                        while !head.ends_with(b"\r\n\r\n") {
                            if s.read(&mut byte).unwrap_or(0) == 0 {
                                return;
                            }
                            head.push(byte[0]);
                        }
                        let head = String::from_utf8_lossy(&head).to_string();
                        let path = head.split(' ').nth(1).unwrap_or("").to_string();
                        let reply = routes
                            .iter()
                            .find(|(p, _)| *p == path)
                            .map(|(_, r)| r.clone())
                            .unwrap_or_else(|| {
                                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".into()
                            });
                        let _ = s.write_all(reply.as_bytes());
                        let _ = s.flush();
                    });
                }
            });
            Server {
                port,
                client: Arc::new(ClientConfig::new(roots)),
            }
        }

        pub fn ok(body: &str) -> String {
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        }

        pub fn redirect(to: &str) -> String {
            format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\n\r\n")
        }
    }

    #[test]
    fn a_body_arrives_through_redirects_over_tls() {
        let server = local::start(vec![
            ("/latest", local::redirect("/v1/asset")),
            ("/v1/asset", local::redirect("/storage/asset")),
            ("/storage/asset", local::ok("the release")),
        ]);
        let url = format!("https://localhost:{}/latest", server.port);
        assert_eq!(get(&url, &server.client).unwrap(), b"the release");
    }

    #[test]
    fn a_redirect_to_plain_http_is_not_followed() {
        let server = local::start(vec![(
            "/latest",
            local::redirect("http://localhost:1/asset"),
        )]);
        let url = format!("https://localhost:{}/latest", server.port);
        let e = get(&url, &server.client).unwrap_err();
        assert!(e.contains("only https"), "{e}");
    }

    #[test]
    fn a_redirect_loop_ends() {
        let server = local::start(vec![
            ("/a", local::redirect("/b")),
            ("/b", local::redirect("/a")),
        ]);
        let url = format!("https://localhost:{}/a", server.port);
        let e = get(&url, &server.client).unwrap_err();
        assert!(e.contains("redirects"), "{e}");
    }

    #[test]
    fn a_missing_file_says_so() {
        let server = local::start(vec![]);
        let url = format!("https://localhost:{}/nothing", server.port);
        let e = get(&url, &server.client).unwrap_err();
        assert!(e.contains("404"), "{e}");
    }

    #[test]
    fn a_server_the_client_does_not_trust_is_refused() {
        // The same server, and a client that trusts the system bundle
        // instead of the test root: the handshake has to fail, because a
        // fetch that accepts any certificate proves nothing about where
        // the bytes came from.
        let server = local::start(vec![("/latest", local::ok("the release"))]);
        let stranger = Arc::new(ClientConfig::new(RootStore::new()));
        let url = format!("https://localhost:{}/latest", server.port);
        let e = get(&url, &stranger).unwrap_err();
        assert!(e.contains("TLS"), "{e}");
    }

    /// The real thing, for a machine whose network reaches GitHub
    /// directly. `cargo test -p quantydb-cli -- --ignored github`
    #[test]
    #[ignore = "needs the network, and a path to GitHub nobody rewrites"]
    fn github_serves_the_checksums_of_the_latest_release() {
        let config = system_config().unwrap();
        let sums = get(&format!("{RELEASES}/latest/download/SHA256SUMS"), &config).unwrap();
        let sums = String::from_utf8(sums).unwrap();
        let asset = asset_name().expect("a machine the release builds for");
        let digest = digest_for(&sums, asset).unwrap();
        let binary = get(&format!("{RELEASES}/latest/download/{asset}"), &config).unwrap();
        assert_eq!(
            quantydb_core::to_hex(&quantydb_core::sha256(&binary)),
            digest
        );
    }

    mod signature {
        use super::super::verify_sums;
        use quantycore::tls::crypto::ed25519::SigningKey;

        const SUMS: &[u8] = b"1111  quantydb-linux-x86_64\n";

        fn hex(bytes: &[u8]) -> String {
            bytes.iter().map(|b| format!("{b:02x}")).collect()
        }

        fn signed(seed: u8, message: &[u8]) -> ([u8; 32], String) {
            let key = SigningKey::from_seed(&[seed; 32]);
            (key.public_key(), hex(&key.sign(message)))
        }

        #[test]
        fn a_signature_by_the_key_is_accepted() {
            let (public, sig) = signed(1, SUMS);
            verify_sums(SUMS, &format!("{sig}\n"), &public).unwrap();
        }

        #[test]
        fn one_changed_byte_in_the_checksums_is_refused() {
            let (public, sig) = signed(1, SUMS);
            let mut changed = SUMS.to_vec();
            changed[0] = b'2';
            assert!(verify_sums(&changed, &sig, &public).is_err());
        }

        #[test]
        fn a_signature_by_another_key_is_refused() {
            let (public, _) = signed(1, SUMS);
            let (_, other) = signed(2, SUMS);
            assert!(verify_sums(SUMS, &other, &public).is_err());
        }

        #[test]
        fn something_that_is_not_a_signature_is_refused() {
            let (public, sig) = signed(1, SUMS);
            assert!(verify_sums(SUMS, "", &public).is_err());
            assert!(verify_sums(SUMS, &sig[..126], &public).is_err());
            assert!(verify_sums(
                SUMS,
                &sig.replace(|c: char| c.is_ascii_digit(), "g"),
                &public
            )
            .is_err());
        }
    }

    #[test]
    fn every_platform_the_release_builds_has_an_asset() {
        // The release workflow builds these five. If this machine is one of
        // them, there must be a name for it; a build the updater cannot
        // name is a build it cannot update.
        let built = [
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("macos", "aarch64"),
            ("macos", "x86_64"),
            ("windows", "x86_64"),
        ];
        let here = (std::env::consts::OS, std::env::consts::ARCH);
        if built.contains(&here) {
            assert!(asset_name().is_some());
        }
    }
}
