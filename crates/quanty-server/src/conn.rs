//! One connection: handshake, then frames, one request in flight.

use std::io::{self, Read, Write};

use quanty_proto::frame::{FrameHeader, HEADER_LEN};
use quanty_proto::limits::MAX_BODY;
use quanty_proto::{
    negotiate, ClientHello, ClientMessage, ErrorCode, ServerHello, ServerMessage, CLIENT_HELLO_LEN,
};

/// Turns a request into the messages that answer it.
pub trait Service {
    /// Answer one statement. An empty reply closes the connection.
    fn call(&mut self, request: ClientMessage) -> Vec<ServerMessage>;
}

/// What the caller should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Keep going.
    Open,
    /// Everything is written and the peer asked to close.
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Hello,
    Frames,
    Draining,
}

/// Buffers and phase for one peer.
pub struct Conn {
    phase: Phase,
    inbuf: Vec<u8>,
    outbuf: Vec<u8>,
    outpos: usize,
}

impl Default for Conn {
    fn default() -> Self {
        Conn::new()
    }
}

impl Conn {
    /// A connection that has not yet seen a handshake.
    pub fn new() -> Self {
        Conn {
            phase: Phase::Hello,
            inbuf: Vec::with_capacity(1024),
            outbuf: Vec::new(),
            outpos: 0,
        }
    }

    /// Whether output is waiting for the socket to take it.
    pub fn wants_write(&self) -> bool {
        self.outpos < self.outbuf.len()
    }

    /// Whether the connection is finished and can be dropped.
    pub fn is_finished(&self) -> bool {
        self.phase == Phase::Draining && !self.wants_write()
    }

    /// Read what the socket has. Returns false when the peer is done sending.
    pub fn fill(&mut self, socket: &mut impl Read) -> io::Result<bool> {
        let mut chunk = [0u8; 8192];
        loop {
            if self.inbuf.len() > HEADER_LEN + MAX_BODY {
                return Ok(true);
            }
            match socket.read(&mut chunk) {
                Ok(0) => return Ok(false),
                Ok(n) => self.inbuf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(true),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Consume as much buffered input as the rules allow.
    pub fn step(&mut self, service: &mut impl Service) -> Step {
        loop {
            if self.phase == Phase::Draining || self.wants_write() {
                return self.state();
            }
            let progressed = match self.phase {
                Phase::Hello => self.step_hello(),
                Phase::Frames => self.step_frame(service),
                Phase::Draining => false,
            };
            if !progressed {
                return self.state();
            }
        }
    }

    fn state(&self) -> Step {
        if self.is_finished() {
            Step::Closed
        } else {
            Step::Open
        }
    }

    fn step_hello(&mut self) -> bool {
        if self.inbuf.len() < CLIENT_HELLO_LEN {
            return false;
        }
        let mut bytes = [0u8; CLIENT_HELLO_LEN];
        bytes.copy_from_slice(&self.inbuf[..CLIENT_HELLO_LEN]);
        self.inbuf.drain(..CLIENT_HELLO_LEN);

        let reply = match ClientHello::decode(&bytes) {
            Ok(hello) => negotiate(hello),
            Err(_) => ServerHello::Refused(quanty_proto::Refusal::BadMagic),
        };
        self.outbuf.extend_from_slice(&reply.encode());
        match reply {
            ServerHello::Accepted { .. } => {
                self.phase = Phase::Frames;
                self.push(ServerMessage::Ready);
                true
            }
            ServerHello::Refused(_) => {
                self.phase = Phase::Draining;
                false
            }
        }
    }

    fn step_frame(&mut self, service: &mut impl Service) -> bool {
        if self.inbuf.len() < HEADER_LEN {
            return false;
        }
        let mut head = [0u8; HEADER_LEN];
        head.copy_from_slice(&self.inbuf[..HEADER_LEN]);
        let header = match FrameHeader::decode(&head) {
            Ok(h) => h,
            Err(_) => return self.fail(ErrorCode::Protocol, "frame too large"),
        };
        if self.inbuf.len() < HEADER_LEN + header.body_len {
            return false;
        }

        let body: Vec<u8> = self.inbuf[HEADER_LEN..HEADER_LEN + header.body_len].to_vec();
        self.inbuf.drain(..HEADER_LEN + header.body_len);

        let request = match ClientMessage::decode(header.msg_type, &body) {
            Ok(m) => m,
            Err(_) => return self.fail(ErrorCode::Protocol, "malformed message"),
        };
        if request == ClientMessage::Close {
            self.phase = Phase::Draining;
            return false;
        }
        for message in service.call(request) {
            self.push(message);
        }
        true
    }

    fn fail(&mut self, code: ErrorCode, detail: &str) -> bool {
        self.push(ServerMessage::error(code, detail));
        self.phase = Phase::Draining;
        false
    }

    fn push(&mut self, message: ServerMessage) {
        match message.encode() {
            Ok(bytes) => self.outbuf.extend_from_slice(&bytes),
            Err(_) => self.phase = Phase::Draining,
        }
    }

    /// Push pending output at the socket, keeping whatever it would not take.
    pub fn flush(&mut self, socket: &mut impl Write) -> io::Result<()> {
        while self.outpos < self.outbuf.len() {
            match socket.write(&self.outbuf[self.outpos..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => self.outpos += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        self.outbuf.clear();
        self.outpos = 0;
        Ok(())
    }
}
