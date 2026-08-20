//! The client and the local path, held against each other.
//!
//! `quanty connect` exists so a person can use the server this repository
//! builds, and the only way to know it is telling the truth is to ask the
//! same question twice: once of the file directly and once over the wire.
//! Anything the protocol loses or garbles on the way shows up here as a
//! difference, without the test needing to know what the right answer is.

#![cfg(target_os = "linux")]

mod common;

use std::io::Write;
use std::net::TcpStream;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::TestDir;

fn quanty(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_quanty"))
        .args(args)
        .output()
        .expect("the binary runs")
}

/// Everything the command said, whichever stream it said it on.
fn said(output: &Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    text
}

/// A port nothing is listening on, by asking the kernel for one and
/// letting it go again.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

struct Served {
    child: Child,
    addr: String,
}

impl Served {
    fn start(database: &str, extra: &[&str]) -> Served {
        for _ in 0..5 {
            let addr = format!("127.0.0.1:{}", free_port());
            let mut args = vec!["serve", database, "--listen", &addr, "--workers", "1"];
            args.extend_from_slice(extra);
            let child = Command::new(env!("CARGO_BIN_EXE_quanty"))
                .args(&args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("the binary runs");

            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if TcpStream::connect(&addr).is_ok() {
                    return Served { child, addr };
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            // The port was taken between asking for it and binding it.
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!("the server never came up");
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn setup(dir: &TestDir) -> String {
    let path = dir.path().join("remote.qdb");
    let path = path.to_str().expect("utf8 path").to_string();
    assert!(quanty(&["create", &path]).status.success());
    for statement in [
        "table users { id: int @key, name: text, score: int = 0, bio: text @null }",
        "table cities { id: int @key, name: text }",
        "put users { id: 1, name: \"elchi\" }, { id: 2, name: \"mira\", score: 7, bio: \"hi\" }",
        "put cities { id: 1, name: \"oslo\" }",
    ] {
        let out = quanty(&["run", &path, statement]);
        assert!(out.status.success(), "setup failed: {}", said(&out));
    }
    path
}

/// The statements are chosen to cover the shapes the protocol has to carry:
/// a result set, a projection, a join, a null, an empty result, a count, a
/// line list, and two different kinds of failure.
const SAME_EITHER_WAY: &[&str] = &[
    "get users",
    "get users { name, id }",
    "get users where score > 0",
    "get users where score > 999",
    "get users join cities on users.id = cities.id",
    "show tables",
    "explain get users",
    "log",
    "nonsense here",
    "get nosuchtable",
];

#[test]
fn the_wire_answers_exactly_what_the_file_answers() {
    let dir = TestDir::new();
    let database = setup(&dir);
    let server = Served::start(&database, &[]);

    let mut everything = String::new();
    for statement in SAME_EITHER_WAY {
        let local = quanty(&["run", &database, statement]);
        let remote = quanty(&["connect", &server.addr, statement]);
        assert_eq!(
            said(&local),
            said(&remote),
            "the two paths disagree about `{statement}`"
        );
        assert_eq!(
            local.status.success(),
            remote.status.success(),
            "the two paths disagree about whether `{statement}` worked"
        );
        everything.push_str(&said(&remote));
    }

    // Two identical silences would satisfy every assertion above, so the
    // run has to show that it carried something. A row value, a column
    // name reordered by a projection, a null, a plan, and an error.
    for needle in [
        "elchi",
        "mira",
        "oslo",
        "null",
        "SeqScan",
        "not a statement",
    ] {
        assert!(
            everything.contains(needle),
            "the comparison never carried {needle:?}, so it compared nothing"
        );
    }
}

#[test]
fn statements_can_be_fed_in_on_stdin() {
    let dir = TestDir::new();
    let database = setup(&dir);
    let server = Served::start(&database, &[]);

    let mut child = Command::new(env!("CARGO_BIN_EXE_quanty"))
        .args(["connect", &server.addr])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary runs");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"# a comment, skipped\n\nshow tables\nget users\n")
        .expect("write");
    let out = child.wait_with_output().expect("wait");

    let expected = format!(
        "{}{}",
        said(&quanty(&["run", &database, "show tables"])),
        said(&quanty(&["run", &database, "get users"]))
    );
    assert_eq!(said(&out), expected, "the session output differs");
    assert!(out.status.success());
}

/// A write over the wire has to actually land in the file.
#[test]
fn a_write_over_the_wire_reaches_the_database() {
    let dir = TestDir::new();
    let database = setup(&dir);
    let server = Served::start(&database, &[]);

    let out = quanty(&[
        "connect",
        &server.addr,
        "put users { id: 42, name: \"over the wire\" }",
    ]);
    assert!(out.status.success(), "{}", said(&out));

    let seen = quanty(&["connect", &server.addr, "get users where id = 42"]);
    assert!(
        said(&seen).contains("over the wire"),
        "the row is not there: {}",
        said(&seen)
    );
}

#[test]
fn a_server_that_wants_a_token_says_so_and_takes_one() {
    let dir = TestDir::new();
    let database = setup(&dir);

    let minted = quanty(&["token", "tester"]);
    assert!(minted.status.success(), "{}", said(&minted));
    let printed = said(&minted);
    let token = printed
        .lines()
        .find_map(|l| l.strip_prefix("token "))
        .expect("a token line")
        .to_string();
    let line = printed
        .lines()
        .find_map(|l| l.strip_prefix("line  "))
        .expect("a line line");

    let tokens = dir.path().join("tokens");
    std::fs::write(&tokens, format!("{line}\n")).expect("write tokens");
    let tokens = tokens.to_str().expect("utf8 path").to_string();
    let server = Served::start(&database, &["--tokens", &tokens]);

    let refused = quanty(&["connect", &server.addr, "show tables"]);
    assert!(
        !refused.status.success(),
        "a tokenless client was served: {}",
        said(&refused)
    );

    let allowed = quanty(&["connect", &server.addr, "show tables", "--token", &token]);
    assert!(
        allowed.status.success(),
        "the minted token was refused: {}",
        said(&allowed)
    );
    assert_eq!(
        said(&allowed),
        said(&quanty(&["run", &database, "show tables"])),
        "authenticating changed the answer"
    );

    let wrong = quanty(&[
        "connect",
        &server.addr,
        "show tables",
        "--token",
        "not the token",
    ]);
    assert!(!wrong.status.success(), "a wrong token was accepted");
}
