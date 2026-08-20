//! What the token file promises, including when it is wrong.

use std::fs;
use std::path::PathBuf;

use quanty_auth::{mint, sha256, to_hex, Tokens};

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let path = std::env::temp_dir().join(format!("quanty-tokens-{name}"));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create");
        Dir(path)
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, contents).expect("write");
        path
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn line_for(token: &str, label: &str) -> String {
    format!("{} {}\n", to_hex(&sha256(token.as_bytes())), label)
}

#[test]
fn a_listed_token_is_accepted_and_an_unlisted_one_is_not() {
    let dir = Dir::new("accept");
    let path = dir.write("tokens", &line_for("secret", "elchi"));
    let tokens = Tokens::load(&path).expect("load");

    assert_eq!(tokens.len(), 1);
    assert!(tokens.accepts(b"secret"));
    assert!(!tokens.accepts(b"secre"));
    assert!(!tokens.accepts(b"secrets"));
    assert!(!tokens.accepts(b""));
}

#[test]
fn comments_and_blank_lines_are_not_tokens() {
    let dir = Dir::new("comments");
    let body = format!(
        "# the ops token\n\n{}   # rotated 2026-08-20\n\n",
        line_for("secret", "ops").trim()
    );
    let path = dir.write("tokens", &body);
    let tokens = Tokens::load(&path).expect("load");

    assert_eq!(tokens.len(), 1);
    assert!(tokens.accepts(b"secret"));
}

/// The whole point of ADR-026: revoking is deleting a line, and it takes
/// effect on a running server.
#[test]
fn deleting_a_line_revokes_it_without_a_restart() {
    let dir = Dir::new("revoke");
    let body = format!("{}{}", line_for("keep", "keep"), line_for("drop", "drop"));
    let path = dir.write("tokens", &body);
    let mut tokens = Tokens::load(&path).expect("load");
    assert!(tokens.accepts(b"keep") && tokens.accepts(b"drop"));

    // A modification time only has second resolution on some filesystems,
    // so the content change is made unmistakable rather than relying on the
    // clock having moved.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    fs::write(&path, line_for("keep", "keep")).expect("rewrite");

    assert!(tokens.reload_if_changed().expect("reload"), "no reload");
    assert!(tokens.accepts(b"keep"));
    assert!(!tokens.accepts(b"drop"), "the revoked token still works");
}

#[test]
fn an_unchanged_file_is_not_read_again() {
    let dir = Dir::new("unchanged");
    let path = dir.write("tokens", &line_for("secret", "one"));
    let mut tokens = Tokens::load(&path).expect("load");
    assert!(!tokens.reload_if_changed().expect("reload"));
}

/// A broken file must not fall open and must not fall over.
#[test]
fn a_malformed_line_is_refused_and_the_old_set_stays_in_force() {
    let dir = Dir::new("malformed");
    let path = dir.write("tokens", &line_for("secret", "one"));
    let mut tokens = Tokens::load(&path).expect("load");

    std::thread::sleep(std::time::Duration::from_millis(1100));
    fs::write(&path, "not-a-hash label\n").expect("rewrite");
    let err = tokens.reload_if_changed().expect_err("should have refused");
    assert!(err.to_string().contains("line 1"), "{err}");
    assert!(
        tokens.accepts(b"secret"),
        "a broken file must not disarm the tokens that were working"
    );
}

#[test]
fn a_hash_without_a_label_is_refused() {
    let dir = Dir::new("nolabel");
    let path = dir.write("tokens", &format!("{}\n", to_hex(&sha256(b"x"))));
    let err = Tokens::load(&path).expect_err("should have refused");
    assert!(err.to_string().contains("label"), "{err}");
}

#[test]
fn a_missing_file_is_an_error_rather_than_an_empty_allow_list() {
    let dir = Dir::new("missing");
    let err = Tokens::load(dir.0.join("nothing-here")).expect_err("should have refused");
    assert!(!err.to_string().is_empty());
}

/// A minted token verifies against the line minted with it, and two mints
/// do not collide.
#[test]
fn a_minted_token_verifies_against_its_own_line() {
    let dir = Dir::new("mint");
    let (token, line) = mint("elchi").expect("mint");
    let (other, _) = mint("elchi").expect("mint");
    assert_ne!(token, other, "two mints produced the same token");
    assert_eq!(token.len(), 64);

    let path = dir.write("tokens", &format!("{line}\n"));
    let tokens = Tokens::load(&path).expect("load");
    assert!(tokens.accepts(token.as_bytes()));
    assert!(!tokens.accepts(other.as_bytes()));
}

#[cfg(unix)]
mod permissions {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn chmod(path: &std::path::Path, mode: u32) {
        let mut perms = fs::metadata(path).expect("stat").permissions();
        perms.set_mode(mode);
        fs::set_permissions(path, perms).expect("chmod");
    }

    /// Anyone who can write to the file can add their own line, which is a
    /// way in rather than a credential store.
    #[test]
    fn a_file_others_can_write_to_is_refused() {
        let dir = Dir::new("writable");
        let path = dir.write("tokens", &line_for("secret", "one"));

        chmod(&path, 0o600);
        assert!(Tokens::load(&path).is_ok(), "0600 should be fine");

        chmod(&path, 0o666);
        let err = Tokens::load(&path).expect_err("0666 should be refused");
        assert!(err.to_string().contains("chmod"), "{err}");

        chmod(&path, 0o620);
        assert!(
            Tokens::load(&path).is_err(),
            "group writable should be refused too"
        );
    }

    /// Readable is not refused: the file holds hashes of full entropy
    /// tokens, which are not worth reading. It is worth saying though.
    #[test]
    fn a_file_others_can_read_is_allowed_and_noted() {
        let dir = Dir::new("readable");
        let path = dir.write("tokens", &line_for("secret", "one"));

        chmod(&path, 0o600);
        let tight = Tokens::load(&path).expect("load");
        assert!(!tight.permissive());

        chmod(&path, 0o644);
        let loose = Tokens::load(&path).expect("load");
        assert!(loose.permissive(), "0644 should have been noted");
        assert!(loose.accepts(b"secret"), "and it still works");
    }

    /// The check runs on every read, so loosening the file at runtime is
    /// caught rather than only at startup.
    #[test]
    fn loosening_it_later_is_caught_on_reload() {
        let dir = Dir::new("later");
        let path = dir.write("tokens", &line_for("secret", "one"));
        chmod(&path, 0o600);
        let mut tokens = Tokens::load(&path).expect("load");

        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(&path, line_for("secret", "one")).expect("rewrite");
        chmod(&path, 0o666);

        assert!(
            tokens.reload_if_changed().is_err(),
            "a file that became writable was reloaded anyway"
        );
        assert!(
            tokens.accepts(b"secret"),
            "and the last good set stays in force"
        );
    }
}
