//! quanty-core: the storage engine underneath QuantyDB.
//!
//! This crate is the trust anchor of the whole project. It knows nothing
//! about SQL, QQL, servers or blobs; it provides a crash-safe, checksummed,
//! copy-on-write page store that everything else is built on.
//!
//! Current layer map (phase 0 of docs/ROADMAP.md):
//!
//! - [`storage`]: byte-level backends (file, memory) behind one trait
//! - [`page`]: page layout, header, checksums
//! - [`meta`]: the dual meta pages and crash recovery
//! - [`pager`]: allocation, the write batch and the commit protocol
//! - [`encoding`]: order-preserving key encoding (tuples of typed values)
//! - node / btree: the copy-on-write B-tree with overflow chains
//! - commit / db: commit records, transactions, snapshots of any commit

mod btree;
mod cache;
mod commit;
mod db;
pub mod encoding;
mod error;
mod meta;
mod node;
pub mod page;
mod pager;
mod storage;

pub use btree::{max_key_len, ReadPages, Scan};
pub use commit::CommitInfo;
pub use db::{Db, Snapshot, WriteTx};
pub use encoding::{decode_key, encode_key, Value};
pub use error::{Error, Result};
pub use meta::{Meta, FORMAT_VERSION, MAGIC};
pub use page::{
    PageId, PageType, DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE, MIN_PAGE_SIZE, NIL_PAGE, PAGE_HEADER_LEN,
};
pub use pager::{Pager, PagerOptions, WriteBatch};
pub use storage::{FileStorage, MemStorage, Storage};
