//! Talking to a `quanty serve` over the wire.
//!
//! The server has had a protocol since phase 5 began and nothing in this
//! repository spoke it except the load generator, which sends one statement
//! and throws the answer away. This is the other half: the handshake, the
//! token, one statement at a time, and the answer rendered.
//!
//! **The rendering is deliberately identical to the local path.** Whatever
//! `quanty run` prints for a statement, this prints for the same statement,
//! and a test holds the two outputs against each other. A client that
//! quietly formats differently is a client that hides protocol bugs.

use std::io::{BufRead, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use quanty_exec::render_value;
use quanty_proto::frame::{FrameHeader, HEADER_LEN};
use quanty_proto::{
    ClientHello, ClientMessage, ServerHello, ServerMessage, SERVER_HELLO_LEN, VERSION,
};

use crate::{emit, failed, Failure};

/// How long to wait for one answer before giving up on it.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// A connected, authenticated session.
pub struct Remote {
    socket: TcpStream,
    buf: Vec<u8>,
}

impl Remote {
    /// Connect, shake hands and show a token if there is one.
    pub fn connect(addr: &str, token: Option<&str>) -> Result<Remote, Failure> {
        let socket =
            TcpStream::connect(addr).map_err(|e| failed(format!("could not reach {addr}: {e}")))?;
        socket.set_nodelay(true).ok();
        socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .ok();

        let mut remote = Remote {
            socket,
            buf: Vec::new(),
        };
        remote
            .socket
            .write_all(&ClientHello { version: VERSION }.encode())
            .map_err(|e| failed(format!("could not send the hello: {e}")))?;

        let mut head = [0u8; SERVER_HELLO_LEN];
        remote.fill_exactly(&mut head)?;
        match ServerHello::decode(&head).map_err(|e| failed(format!("bad server hello: {e}")))? {
            ServerHello::Accepted { .. } => {}
            ServerHello::Refused(why) => {
                return Err(failed(format!(
                    "the server refused the handshake ({why:?}); this client speaks version {VERSION}"
                )));
            }
        }
        // An accepted handshake is followed by Ready.
        match remote.answer()? {
            Reply::Done => {}
            Reply::Error(message) => return Err(failed(message)),
            Reply::Text(_) => return Err(failed("the server said something unexpected")),
        }

        if let Some(token) = token {
            remote.send(ClientMessage::Auth(token.as_bytes().to_vec()))?;
            match remote.answer()? {
                Reply::Done => {}
                Reply::Error(message) => return Err(failed(format!("token refused: {message}"))),
                Reply::Text(_) => return Err(failed("the server said something unexpected")),
            }
        }

        Ok(remote)
    }

    /// Run one statement and print what comes back.
    ///
    /// A statement the server refuses is reported and is not fatal: the
    /// connection is still good and the next statement can go.
    pub fn statement(&mut self, source: &str, sql: bool) -> Result<bool, Failure> {
        let message = if sql {
            ClientMessage::QuerySql(source.to_string())
        } else {
            ClientMessage::Query(source.to_string())
        };
        self.send(message)?;
        match self.answer()? {
            Reply::Done => {
                emit("ok")?;
                Ok(true)
            }
            Reply::Text(text) => {
                if !text.is_empty() {
                    emit(&text)?;
                }
                Ok(true)
            }
            Reply::Error(message) => {
                eprintln!("{message}");
                Ok(false)
            }
        }
    }

    /// Say goodbye so the server closes the connection rather than seeing
    /// it disappear.
    pub fn close(mut self) {
        if let Ok(bytes) = ClientMessage::Close.encode() {
            let _ = self.socket.write_all(&bytes);
        }
    }

    fn send(&mut self, message: ClientMessage) -> Result<(), Failure> {
        let bytes = message
            .encode()
            .map_err(|e| failed(format!("could not encode the statement: {e}")))?;
        self.socket
            .write_all(&bytes)
            .map_err(|e| failed(format!("could not send the statement: {e}")))
    }

    /// Read messages until one whole answer has arrived.
    fn answer(&mut self) -> Result<Reply, Failure> {
        let mut rows: Option<Vec<String>> = None;
        loop {
            match self.message()? {
                ServerMessage::Ready | ServerMessage::Ok => return Ok(Reply::Done),
                ServerMessage::Count { verb, n } => return Ok(Reply::Text(format!("{verb} {n}"))),
                ServerMessage::Lines(lines) => return Ok(Reply::Text(lines.join("\n"))),
                // The message alone, without the numeric code. The server
                // writes these to be read by a person, and the local path
                // prints exactly this text for the same statement, which is
                // a property a test holds it to. The code is for programs,
                // and a program speaks the protocol rather than this.
                ServerMessage::Error { message, .. } => return Ok(Reply::Error(message)),
                // The column names arrive here and are not printed, because
                // the local path has none to print and the two outputs are
                // held against each other. Naming columns in what a user
                // sees is a change to both halves at once.
                ServerMessage::RowsBegin { .. } => rows = Some(Vec::new()),
                ServerMessage::RowBatch { rows: batch } => {
                    let Some(collected) = rows.as_mut() else {
                        return Err(failed("the server sent rows without beginning them"));
                    };
                    for row in batch {
                        // The same function the local path uses, so the
                        // two cannot drift apart by imitation.
                        let rendered: Vec<String> = row.iter().map(render_value).collect();
                        collected.push(rendered.join("|"));
                    }
                }
                ServerMessage::RowsEnd => {
                    let Some(collected) = rows.take() else {
                        return Err(failed("the server ended rows it never began"));
                    };
                    return Ok(Reply::Text(collected.join("\n")));
                }
            }
        }
    }

    fn message(&mut self) -> Result<ServerMessage, Failure> {
        let mut head = [0u8; HEADER_LEN];
        self.fill_exactly(&mut head)?;
        let header =
            FrameHeader::decode(&head).map_err(|e| failed(format!("bad frame header: {e}")))?;
        let mut body = vec![0u8; header.body_len];
        self.fill_exactly(&mut body)?;
        ServerMessage::decode(header.msg_type, &body)
            .map_err(|e| failed(format!("bad message from the server: {e}")))
    }

    fn fill_exactly(&mut self, out: &mut [u8]) -> Result<(), Failure> {
        let deadline = std::time::Instant::now() + REPLY_TIMEOUT;
        while self.buf.len() < out.len() {
            if std::time::Instant::now() >= deadline {
                return Err(failed("the server did not answer in time"));
            }
            let mut chunk = [0u8; 8192];
            match self.socket.read(&mut chunk) {
                Ok(0) => return Err(failed("the server closed the connection")),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut
                        || e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(failed(format!("could not read from the server: {e}"))),
            }
        }
        out.copy_from_slice(&self.buf[..out.len()]);
        self.buf.drain(..out.len());
        Ok(())
    }
}

enum Reply {
    /// Nothing to print beyond acknowledgement.
    Done,
    /// Something to print, possibly empty.
    Text(String),
    /// Something to complain about, without ending the session.
    Error(String),
}

/// One statement against a server, or a session reading from stdin.
pub fn connect(
    addr: &str,
    statement: Option<&str>,
    token: Option<&str>,
    sql: bool,
) -> Result<(), Failure> {
    let mut remote = Remote::connect(addr, token)?;

    if let Some(source) = statement {
        let ok = remote.statement(source, sql)?;
        remote.close();
        return if ok { Ok(()) } else { Err(Failure::Refused) };
    }

    let stdin = std::io::stdin();
    let mut failures = 0usize;
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| failed(format!("could not read stdin: {e}")))?;
        let source = line.trim();
        if source.is_empty() || source.starts_with('#') {
            continue;
        }
        if !remote.statement(source, sql)? {
            failures += 1;
        }
    }
    remote.close();
    if failures > 0 {
        return Err(Failure::Refused);
    }
    Ok(())
}
