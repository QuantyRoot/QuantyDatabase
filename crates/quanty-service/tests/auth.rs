//! What a server with a token file refuses, and what it stops refusing.

#![cfg(target_os = "linux")]

mod harness;

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use quanty_auth::{sha256, to_hex, Tokens};
use quanty_proto::{ErrorCode, ServerMessage};
use quanty_service::Deadlines;

use harness::Server;

struct TokenFile(PathBuf);

impl TokenFile {
    fn with(name: &str, tokens: &[&str]) -> TokenFile {
        let path = std::env::temp_dir().join(format!("quanty-svc-auth-{name}"));
        let file = TokenFile(path);
        file.set(tokens);
        file
    }

    fn set(&self, tokens: &[&str]) {
        let body: String = tokens
            .iter()
            .map(|t| format!("{} {t}\n", to_hex(&sha256(t.as_bytes()))))
            .collect();
        fs::write(&self.0, body).expect("write token file");
    }

    fn load(&self) -> Tokens {
        Tokens::load(&self.0).expect("load token file")
    }
}

impl Drop for TokenFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn is_error(message: &ServerMessage, expected: ErrorCode) -> bool {
    matches!(message, ServerMessage::Error { code, .. } if *code == expected.as_u16())
}

fn patient() -> Deadlines {
    Deadlines {
        busy: Duration::from_secs(30),
        idle_in_txn: Duration::from_secs(30),
    }
}

#[test]
fn a_statement_before_a_token_is_refused() {
    let file = TokenFile::with("before", &["letmein"]);
    let server = Server::with_tokens(patient(), Some(file.load()));

    let mut client = server.client();
    let reply = client.ask("show tables");
    assert!(
        is_error(&reply, ErrorCode::NotAuthenticated),
        "expected to be told to authenticate, got {reply:?}"
    );

    // And the connection survives it, so a client can still authenticate.
    assert_eq!(client.authenticate("letmein"), ServerMessage::Ready);
    assert!(
        matches!(client.ask("show tables"), ServerMessage::Lines(_)),
        "the statement did not run after the token was accepted"
    );
}

#[test]
fn a_wrong_token_is_refused_and_leaves_the_connection_shut() {
    let file = TokenFile::with("wrong", &["letmein"]);
    let server = Server::with_tokens(patient(), Some(file.load()));

    let mut client = server.client();
    let reply = client.authenticate("letmeout");
    assert!(
        is_error(&reply, ErrorCode::AuthFailed),
        "expected the token to be refused, got {reply:?}"
    );

    let after = client.ask("show tables");
    assert!(
        is_error(&after, ErrorCode::NotAuthenticated),
        "a refused token still opened the door: {after:?}"
    );

    // A second try with the right token works: a refusal is not a ban.
    assert_eq!(client.authenticate("letmein"), ServerMessage::Ready);
    assert!(matches!(client.ask("show tables"), ServerMessage::Lines(_)));
}

/// Authentication is per connection, not per server.
#[test]
fn one_connection_authenticating_does_not_let_another_in() {
    let file = TokenFile::with("percon", &["letmein"]);
    let server = Server::with_tokens(patient(), Some(file.load()));

    let mut allowed = server.client_with("letmein");
    assert!(matches!(
        allowed.ask("show tables"),
        ServerMessage::Lines(_)
    ));

    let mut stranger = server.client();
    let reply = stranger.ask("show tables");
    assert!(
        is_error(&reply, ErrorCode::NotAuthenticated),
        "a stranger rode in on someone else's token: {reply:?}"
    );
}

/// The promise of ADR-026: revoking is deleting a line, on a running
/// server, without a restart.
#[test]
fn deleting_a_line_shuts_the_door_on_a_running_server() {
    let file = TokenFile::with("revoke", &["keep", "drop"]);
    let server = Server::with_tokens(patient(), Some(file.load()));

    let mut early = server.client_with("drop");
    assert!(matches!(early.ask("show tables"), ServerMessage::Lines(_)));

    // Second resolution on the modification time, so the change is made
    // unmistakable rather than trusted to the clock.
    std::thread::sleep(Duration::from_millis(1200));
    file.set(&["keep"]);
    std::thread::sleep(Duration::from_millis(1200));

    let mut refused = server.client();
    let reply = refused.authenticate("drop");
    assert!(
        is_error(&reply, ErrorCode::AuthFailed),
        "the revoked token still authenticates: {reply:?}"
    );
    assert_eq!(
        server.client().authenticate("keep"),
        ServerMessage::Ready,
        "revoking one token disarmed the others"
    );
}

/// Revocation does not reach back into a connection that is already in.
///
/// Worth writing down rather than discovering: the check happens at `Auth`,
/// so a session that authenticated before the line was deleted keeps
/// working until it disconnects.
#[test]
fn revoking_does_not_evict_a_connection_that_is_already_in() {
    let file = TokenFile::with("evict", &["keep", "drop"]);
    let server = Server::with_tokens(patient(), Some(file.load()));

    let mut inside = server.client_with("drop");
    std::thread::sleep(Duration::from_millis(1200));
    file.set(&["keep"]);
    std::thread::sleep(Duration::from_millis(1200));

    assert!(
        matches!(inside.ask("show tables"), ServerMessage::Lines(_)),
        "the open connection was cut off, which this server does not promise"
    );
    assert!(
        is_error(&server.client().authenticate("drop"), ErrorCode::AuthFailed),
        "but a new connection must be refused"
    );
}

/// Without a token file nothing changes for anyone.
#[test]
fn a_server_without_a_token_file_asks_for_nothing() {
    let server = Server::start(patient());
    assert!(!server.needs_auth());

    let mut client = server.client();
    assert!(matches!(client.ask("show tables"), ServerMessage::Lines(_)));
    assert_eq!(
        client.authenticate("anything at all"),
        ServerMessage::Ready,
        "a server requiring nothing must still answer Auth"
    );
}
