//! A server on a real socket, driven from the outside like a client.
//!
//! Everything here goes over loopback and through the executor thread,
//! because the questions this slice raises, whether a connection is parked
//! rather than blocked and whether two connections keep their transactions
//! apart, cannot be asked of the pieces one at a time.

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use quanty_core::{Db, MemStorage};
use quanty_exec::Session;
use quanty_proto::frame::{FrameHeader, HEADER_LEN};
use quanty_proto::{
    ClientHello, ClientMessage, ServerHello, ServerMessage, SERVER_HELLO_LEN, VERSION,
};
use quanty_server::Worker;
use quanty_service::{Deadlines, Executor};

/// A running server, and the thread that turns its loop.
pub struct Server {
    addr: SocketAddr,
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    /// Dropped last, after the worker thread is joined, so no handle
    /// outlives the executor it points at.
    _executor: Executor,
}

impl Server {
    /// Start one on an in-memory database.
    pub fn start(deadlines: Deadlines) -> Server {
        let db = Db::in_memory().expect("in-memory db");
        let session: Session<MemStorage> = Session::new(db);
        let executor = Executor::spawn(session, deadlines);

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        listener.set_nonblocking(true).expect("nonblocking");

        let running = Arc::new(AtomicBool::new(true));
        let mut worker = Worker::owning(listener, running.clone()).expect("worker");
        let dispatch = executor.handle();
        let flag = running.clone();
        let handle = thread::spawn(move || {
            while flag.load(Ordering::Relaxed) {
                if worker.turn(20, &dispatch).is_err() {
                    break;
                }
            }
            worker.shutdown(&dispatch);
        });

        Server {
            addr,
            running,
            worker: Some(handle),
            _executor: executor,
        }
    }

    /// A connected client that has finished the handshake.
    pub fn client(&self) -> Client {
        let mut socket = TcpStream::connect(self.addr).expect("connect");
        socket.set_nodelay(true).expect("nodelay");
        socket
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("timeout");
        socket
            .write_all(&ClientHello { version: VERSION }.encode())
            .expect("hello");

        let mut client = Client {
            socket,
            buf: Vec::new(),
        };
        let mut head = [0u8; SERVER_HELLO_LEN];
        client.read_exactly(&mut head, Duration::from_secs(5));
        match ServerHello::decode(&head).expect("server hello") {
            ServerHello::Accepted { .. } => {}
            other => panic!("handshake refused: {other:?}"),
        }
        // Every accepted handshake is followed by Ready.
        assert_eq!(client.reply(Duration::from_secs(5)), ServerMessage::Ready);
        client
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// One connection, speaking the protocol by hand.
pub struct Client {
    socket: TcpStream,
    buf: Vec<u8>,
}

impl Client {
    /// Send a QQL statement without waiting for the answer.
    pub fn send(&mut self, statement: &str) {
        let bytes = ClientMessage::Query(statement.into())
            .encode()
            .expect("encode");
        self.socket.write_all(&bytes).expect("write");
    }

    /// Send a statement and read the one message that answers it.
    pub fn ask(&mut self, statement: &str) -> ServerMessage {
        self.send(statement);
        self.reply(Duration::from_secs(5))
    }

    /// The next message, waiting up to `within` for it.
    pub fn reply(&mut self, within: Duration) -> ServerMessage {
        let deadline = Instant::now() + within;
        loop {
            if let Some(message) = self.take_message() {
                return message;
            }
            assert!(
                Instant::now() < deadline,
                "no reply within {within:?}, {} bytes buffered",
                self.buf.len()
            );
            self.pull();
        }
    }

    /// Whether an answer has arrived yet, without waiting for one.
    pub fn answered(&mut self, within: Duration) -> Option<ServerMessage> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(message) = self.take_message() {
                return Some(message);
            }
            if Instant::now() >= deadline {
                return None;
            }
            self.pull();
        }
    }

    /// Drop the connection without saying goodbye.
    pub fn abandon(self) {
        drop(self.socket);
    }

    fn pull(&mut self) {
        let mut chunk = [0u8; 4096];
        match self.socket.read(&mut chunk) {
            Ok(0) => thread::sleep(Duration::from_millis(5)),
            Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
            Err(_) => thread::sleep(Duration::from_millis(5)),
        }
    }

    fn take_message(&mut self) -> Option<ServerMessage> {
        if self.buf.len() < HEADER_LEN {
            return None;
        }
        let mut head = [0u8; HEADER_LEN];
        head.copy_from_slice(&self.buf[..HEADER_LEN]);
        let header = FrameHeader::decode(&head).expect("header");
        if self.buf.len() < HEADER_LEN + header.body_len {
            return None;
        }
        let body = self.buf[HEADER_LEN..HEADER_LEN + header.body_len].to_vec();
        self.buf.drain(..HEADER_LEN + header.body_len);
        Some(ServerMessage::decode(header.msg_type, &body).expect("message"))
    }

    fn read_exactly(&mut self, out: &mut [u8], within: Duration) {
        let deadline = Instant::now() + within;
        while self.buf.len() < out.len() {
            assert!(Instant::now() < deadline, "handshake never arrived");
            self.pull();
        }
        out.copy_from_slice(&self.buf[..out.len()]);
        self.buf.drain(..out.len());
    }
}
