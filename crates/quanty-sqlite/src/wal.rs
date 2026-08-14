//! The write-ahead log.
//!
//! In wal mode a writer does not touch the main database file. It appends
//! whole pages to a `-wal` file next to it, and only a checkpoint folds
//! them back in. So the main file on its own is a snapshot of the database
//! as of the last checkpoint, and every change since then lives here. A
//! reader that ignores this file does not fail; it quietly returns an older
//! database, which is the worst kind of wrong.
//!
//! The format is a 32 byte header followed by frames, each of which is a 24
//! byte header and one page of data. Reading it correctly is three rules:
//!
//! Frames belong to the log only if their salts match the header's. A
//! checkpoint bumps the salts and then writes new frames from the start of
//! the file, so a `-wal` file routinely contains stale frames from an
//! earlier generation, sitting after the new ones and looking perfectly
//! well formed.
//!
//! Frames are only valid up to the first checksum that does not match. The
//! checksum is cumulative: each frame's covers the previous frame's result,
//! its own header and its page, so a torn write anywhere invalidates
//! everything after it rather than just itself. That is what makes the log
//! safe to read after a crash without a recovery pass.
//!
//! Only frames up to the last commit frame count. A commit frame is one
//! whose header carries the database size, and frames after the last one
//! belong to a transaction that was still open. Those rows were never
//! committed, and returning them would be inventing data.

use std::collections::HashMap;

use crate::error::{Result, SqliteError};
use crate::source::Source;

pub const WAL_HEADER_LEN: usize = 32;
pub const FRAME_HEADER_LEN: usize = 24;

const MAGIC_LITTLE: u32 = 0x377f_0682;
const MAGIC_BIG: u32 = 0x377f_0683;

/// A `-wal` file, read up to its last commit.
pub struct Wal<S: Source> {
    source: S,
    page_size: u32,
    /// Page number to the byte offset of that page's newest committed
    /// frame data. Later frames overwrite earlier ones, which is how a page
    /// written three times in a row ends up at its third version.
    pages: HashMap<u32, u64>,
    /// Database size in pages as of the last commit. A transaction can
    /// shrink the database, so this is not simply the largest page seen.
    page_count: u32,
    committed_frames: u32,
    total_frames: u32,
}

impl<S: Source> Wal<S> {
    /// Read and validate a `-wal` file.
    ///
    /// An empty file is not an error: sqlite leaves a zero length `-wal`
    /// behind after a checkpoint, and so does a database that has been in
    /// wal mode but never written to.
    pub fn open(source: S) -> Result<Wal<S>> {
        let len = source.len()?;
        if len < WAL_HEADER_LEN as u64 {
            return Ok(Wal {
                source,
                page_size: 0,
                pages: HashMap::new(),
                page_count: 0,
                committed_frames: 0,
                total_frames: 0,
            });
        }

        let mut header = [0u8; WAL_HEADER_LEN];
        source.read_at(0, &mut header)?;

        let magic = be32(&header, 0);
        let big_endian = match magic {
            MAGIC_LITTLE => false,
            MAGIC_BIG => true,
            other => {
                return Err(SqliteError::malformed(
                    None,
                    format!("the wal file starts with {other:#010x}, not a wal magic number"),
                ))
            }
        };

        // the checksum comes before any of the fields are believed. it
        // covers the header's first 24 bytes starting from zero, and a
        // header that fails it cannot be trusted to state its own page
        // size, let alone the salts every frame is judged against.
        let running = checksum(big_endian, (0, 0), &header[..24]);
        let expected = (be32(&header, 24), be32(&header, 28));
        if running != expected {
            return Err(SqliteError::malformed(
                None,
                "the wal header checksum does not match its contents",
            ));
        }

        let format = be32(&header, 4);
        if format != 3_007_000 {
            return Err(SqliteError::unsupported(format!(
                "wal format version {format} is not the one this reader knows (3007000)"
            )));
        }

        let page_size = be32(&header, 8);
        if page_size < 512 || !page_size.is_power_of_two() {
            return Err(SqliteError::malformed(
                None,
                format!("the wal header gives a page size of {page_size}"),
            ));
        }

        let salt = (be32(&header, 16), be32(&header, 20));

        let frame_len = FRAME_HEADER_LEN as u64 + page_size as u64;
        let total_frames =
            u32::try_from((len - WAL_HEADER_LEN as u64) / frame_len).unwrap_or(u32::MAX);

        let mut pages: HashMap<u32, u64> = HashMap::new();
        let mut running = running;
        let mut committed_frames = 0u32;
        let mut page_count = 0u32;
        // frames seen since the last commit: they only count once a commit
        // frame arrives, so they are held here rather than published
        let mut pending: Vec<(u32, u64)> = Vec::new();
        let mut frame_header = [0u8; FRAME_HEADER_LEN];
        let mut page = vec![0u8; page_size as usize];

        for index in 0..total_frames {
            let at = WAL_HEADER_LEN as u64 + index as u64 * frame_len;
            source.read_at(at, &mut frame_header)?;
            let number = be32(&frame_header, 0);
            let after_commit = be32(&frame_header, 4);
            let frame_salt = (be32(&frame_header, 8), be32(&frame_header, 12));
            if frame_salt != salt {
                // a frame from an earlier generation of the log
                break;
            }

            source.read_at(at + FRAME_HEADER_LEN as u64, &mut page)?;
            let after_header = checksum(big_endian, running, &frame_header[..8]);
            let computed = checksum(big_endian, after_header, &page);
            if computed != (be32(&frame_header, 16), be32(&frame_header, 20)) {
                // a torn or interrupted write: nothing from here on is
                // trustworthy, including frames that look intact
                break;
            }
            running = computed;

            if number == 0 {
                return Err(SqliteError::malformed(
                    None,
                    format!("wal frame {index} claims to hold page 0"),
                ));
            }
            pending.push((number, at + FRAME_HEADER_LEN as u64));

            if after_commit != 0 {
                for (number, offset) in pending.drain(..) {
                    pages.insert(number, offset);
                }
                committed_frames = index + 1;
                page_count = after_commit;
            }
        }

        // pages written past the committed size belong to a transaction
        // that grew the database and then did not commit
        pages.retain(|number, _| *number <= page_count);

        Ok(Wal {
            source,
            page_size,
            pages,
            page_count,
            committed_frames,
            total_frames,
        })
    }

    /// Whether the log contributes anything at all.
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// Page size the log was written with. Zero for an empty log.
    pub fn page_size(&self) -> u32 {
        self.page_size
    }

    /// Database size in pages as of the last commit, zero for an empty log.
    pub fn page_count(&self) -> u32 {
        self.page_count
    }

    /// Frames the log holds, and how many of them survived validation and
    /// ended at a commit.
    pub fn frame_counts(&self) -> (u32, u32) {
        (self.total_frames, self.committed_frames)
    }

    /// Read `buf.len()` bytes of page `number` starting `offset` bytes into
    /// it, if the log holds a committed version of that page.
    pub fn read_page_part(&self, number: u32, offset: u32, buf: &mut [u8]) -> Result<bool> {
        let Some(at) = self.pages.get(&number) else {
            return Ok(false);
        };
        let end = offset as u64 + buf.len() as u64;
        if end > self.page_size as u64 {
            return Err(SqliteError::malformed(
                number,
                format!(
                    "a read of {} bytes at {offset} runs past the {} byte wal page",
                    buf.len(),
                    self.page_size
                ),
            ));
        }
        self.source.read_at(at + offset as u64, buf)?;
        Ok(true)
    }
}

/// SQLite's wal checksum: two running 32 bit words over the data read as
/// pairs of 32 bit integers, in the byte order the header's magic selects.
///
/// The data length is always a multiple of eight in the places this is
/// used, and anything left over is ignored rather than padded, which
/// matches what sqlite does.
fn checksum(big_endian: bool, start: (u32, u32), data: &[u8]) -> (u32, u32) {
    let (mut s0, mut s1) = start;
    let mut chunks = data.chunks_exact(8);
    for chunk in &mut chunks {
        let (x, y) = if big_endian {
            (
                u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
                u32::from_be_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]),
            )
        } else {
            (
                u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
                u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]),
            )
        };
        s0 = s0.wrapping_add(x).wrapping_add(s1);
        s1 = s1.wrapping_add(y).wrapping_add(s0);
    }
    (s0, s1)
}

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// A database file with its log read on top of it.
///
/// This is a `Source`, not a special case inside the reader: a page that
/// the log holds a committed version of is served from there, everything
/// else from the main file, and nothing above this line has to know that a
/// log exists at all. The reader that walks b-trees is the same code
/// either way.
///
/// Reads are split at page boundaries before being served, so a caller
/// that asks for a range spanning two pages gets each half from wherever
/// that page currently lives.
pub struct WalSource<S: Source, W: Source> {
    main: S,
    log: Option<Wal<W>>,
    page_size: u32,
    /// Database size in pages, which the log's last commit can change: a
    /// transaction may grow the database, and may shrink it.
    page_count: u32,
}

impl<S: Source, W: Source> WalSource<S, W> {
    /// Put `log` on top of `main`. Pass `None` when there is no log file,
    /// which is the state sqlite leaves behind after a checkpoint.
    pub fn new(main: S, log: Option<W>) -> Result<WalSource<S, W>> {
        let mut header = [0u8; crate::header::HEADER_LEN];
        if main.len()? < header.len() as u64 {
            return Err(SqliteError::not_sqlite(
                "the file is shorter than a database header",
            ));
        }
        main.read_at(0, &mut header)?;
        let page_size = crate::header::Header::parse(&header)?.page_size;

        let log = match log {
            Some(source) => {
                let wal = Wal::open(source)?;
                // this check comes before the empty case on purpose. a log
                // whose page size disagrees with the database is one we
                // have misread, and misreading it makes it look empty:
                // frames land at the wrong offsets and fail their salt
                // check. treating that as "no log" would drop real commits
                // and then report the database as complete, which is the
                // exact outcome this whole path exists to prevent. a page
                // size of zero is the one honest empty case, a file with no
                // header in it at all.
                if wal.page_size() != 0 && wal.page_size() != page_size {
                    return Err(SqliteError::malformed(
                        None,
                        format!(
                            "the log is written in {} byte pages and the database in {page_size}",
                            wal.page_size()
                        ),
                    ));
                }
                if wal.is_empty() {
                    None
                } else {
                    Some(wal)
                }
            }
            None => None,
        };

        let page_count = match &log {
            Some(wal) => wal.page_count(),
            None => u32::try_from(main.len()? / page_size as u64).unwrap_or(u32::MAX),
        };

        Ok(WalSource {
            main,
            log,
            page_size,
            page_count,
        })
    }

    /// Whether any page is actually being served from the log.
    pub fn has_log(&self) -> bool {
        self.log.is_some()
    }
}

impl<S: Source, W: Source> Source for WalSource<S, W> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let Some(log) = &self.log else {
            return self.main.read_at(offset, buf);
        };

        let page_size = self.page_size as u64;
        let mut done = 0usize;
        while done < buf.len() {
            let at = offset + done as u64;
            let page = (at / page_size) + 1;
            let within = (at % page_size) as u32;
            let take = ((page_size - within as u64) as usize).min(buf.len() - done);
            let slice = &mut buf[done..done + take];

            // a page number past u32 cannot be in the log, so such a read
            // goes to the main file and fails there if it is out of range
            let served = match u32::try_from(page) {
                Ok(number) => log.read_page_part(number, within, slice)?,
                Err(_) => false,
            };
            if !served {
                self.main.read_at(at, slice)?;
            }
            done += take;
        }
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        match &self.log {
            // the log's last commit is what says how large the database is
            // now; the main file may be shorter, and may be longer
            Some(_) => Ok(self.page_count as u64 * self.page_size as u64),
            None => self.main.len(),
        }
    }

    /// This source reads the log, so a wal mode database read through it is
    /// complete by construction.
    fn accounts_for_wal(&self) -> bool {
        true
    }
}
