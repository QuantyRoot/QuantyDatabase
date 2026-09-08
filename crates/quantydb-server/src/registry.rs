//! Where a worker keeps the connections it owns.

use std::net::TcpStream;

use crate::conn::Conn;
use crate::dispatch::ConnId;
use crate::poll::{Interest, Token};

/// A connection and the state a worker keeps beside it.
pub struct Connection {
    /// The socket. Non-blocking; the worker never blocks on it.
    pub socket: TcpStream,
    /// Which slot this is, so a handler can hand the token back.
    pub token: Token,
    /// The name that outlives the slot, for whoever keeps state per
    /// connection.
    pub id: ConnId,
    /// Protocol state for this peer.
    pub state: Conn,
    /// What this socket is currently registered for.
    ///
    /// Tracked so the worker only calls into the kernel when the answer
    /// changes. Leaving WRITABLE registered with an empty output buffer
    /// makes epoll report readiness on every turn, which is a busy loop
    /// that looks like load.
    pub interest: Interest,
}

struct Slot {
    /// How many times this slot has been handed out. Odd while occupied,
    generation: u32,
    conn: Option<Connection>,
}

/// The connections one worker owns.
pub struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
    live: usize,
}

impl Registry {
    /// An empty registry with room for `capacity` connections before it
    pub fn with_capacity(capacity: usize) -> Self {
        Registry {
            slots: Vec::with_capacity(capacity),
            free: Vec::new(),
            live: 0,
        }
    }

    /// How many connections are open.
    pub fn len(&self) -> usize {
        self.live
    }

    /// Whether no connections are open.
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Take a connection in and return the token to register it under.
    pub fn insert(&mut self, socket: TcpStream) -> Token {
        let index = match self.free.pop() {
            Some(i) => i,
            None => {
                self.slots.push(Slot {
                    generation: 0,
                    conn: None,
                });
                (self.slots.len() - 1) as u32
            }
        };
        let slot = &mut self.slots[index as usize];
        slot.generation = slot.generation.wrapping_add(1);
        let token = Token::new(index, slot.generation);
        slot.conn = Some(Connection {
            socket,
            token,
            id: ConnId::next(),
            state: Conn::new(),
            interest: Interest::READABLE,
        });
        self.live += 1;
        token
    }

    /// Find a connection, or nothing if the token is stale.
    pub fn get_mut(&mut self, token: Token) -> Option<&mut Connection> {
        let slot = self.slots.get_mut(token.index() as usize)?;
        if slot.generation != token.generation() {
            return None;
        }
        slot.conn.as_mut()
    }

    /// Drop a connection and free its slot.
    pub fn remove(&mut self, token: Token) -> Option<Connection> {
        let slot = self.slots.get_mut(token.index() as usize)?;
        if slot.generation != token.generation() {
            return None;
        }
        let conn = slot.conn.take()?;
        slot.generation = slot.generation.wrapping_add(1);
        self.free.push(token.index());
        self.live -= 1;
        Some(conn)
    }

    /// Every live connection, for shutdown and for accounting.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Connection> {
        self.slots.iter_mut().filter_map(|s| s.conn.as_mut())
    }
}
