//! Every number in the protocol that bounds an allocation.

/// Largest body this implementation will read or write, 16 MiB.
pub const MAX_BODY: usize = 16 * 1024 * 1024;

/// Most values one row may carry, and so most columns a result may have.
pub const MAX_VALUES_PER_ROW: usize = 4096;

/// Most rows one `RowBatch` may carry.
pub const MAX_ROWS_PER_BATCH: usize = 65536;

/// Most strings one `Lines` message may carry.
pub const MAX_LINES: usize = 65536;

/// Most elements to reserve up front from a count read off the wire.
pub const PREALLOC_ELEMS: usize = 256;

/// Smallest number of bytes a value can encode to: the tag of a `Null`.
pub const MIN_VALUE_LEN: usize = 1;

/// Smallest number of bytes a row can encode to: its value count, empty.
pub const MIN_ROW_LEN: usize = 4;

/// Smallest number of bytes a string can encode to: its length, empty.
pub const MIN_TEXT_LEN: usize = 4;
