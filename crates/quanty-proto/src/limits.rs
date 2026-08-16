//! Every number in the protocol that bounds an allocation.
//!
//! Collected in one file on purpose. These constants are the difference
//! between a decoder that is merely correct and one that cannot be used as
//! a weapon, and a reader checking that property should not have to find
//! them scattered across five modules. They are part of the format, not
//! implementation detail: see docs/PROTOCOL.md, where the same numbers are
//! normative, because two implementations that disagree about them
//! disagree about which frames are legal.

/// Largest body this implementation will read or write, 16 MiB.
///
/// The reason a decoder may allocate from a length field at all. The number
/// arrives from the network, so it is either capped before it reaches an
/// allocator or it is a way to ask for arbitrary memory.
pub const MAX_BODY: usize = 16 * 1024 * 1024;

/// Most values one row may carry, and so most columns a result may have.
///
/// A table wider than this is not something this engine supports, so the
/// limit costs nothing real and removes a multiplier.
pub const MAX_VALUES_PER_ROW: usize = 4096;

/// Most rows one `RowBatch` may carry.
///
/// Not a limit on result size: a result is a sequence of batches and the
/// sequence is unbounded. This bounds only how much a single frame can
/// commit the receiver to before it has seen any of it.
pub const MAX_ROWS_PER_BATCH: usize = 65536;

/// Most strings one `Lines` message may carry.
pub const MAX_LINES: usize = 65536;

/// Most elements to reserve up front from a count read off the wire.
///
/// The counts above are already capped, so this is not what makes the
/// memory bounded; it is what keeps a message carrying two rows from
/// reserving room for sixty-five thousand. Beyond this the vector grows as
/// elements actually arrive, which costs a few reallocations in the rare
/// large case and nothing in the common small one.
pub const PREALLOC_ELEMS: usize = 256;

/// Smallest number of bytes a value can encode to: the tag of a `Null`.
pub const MIN_VALUE_LEN: usize = 1;

/// Smallest number of bytes a row can encode to: its value count, empty.
pub const MIN_ROW_LEN: usize = 4;

/// Smallest number of bytes a string can encode to: its length, empty.
pub const MIN_TEXT_LEN: usize = 4;
