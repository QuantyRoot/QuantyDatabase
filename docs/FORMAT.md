# QuantyDB File Format, version 1

Normative description of the on-disk format as implemented in quanty-core.
If the code and this document disagree, one of them has a bug and the fix
must touch both.

Byte order is little endian everywhere. All offsets are absolute unless
stated otherwise.

## File layout

A database is a single file made of fixed-size pages. The page size is
chosen at creation time (power of two, 512 to 65536, default 4096) and never
changes for the lifetime of the file.

```
page 0   meta slot A
page 1   meta slot B
page 2+  data pages
```

Page id N lives at byte offset `N * page_size`. Page id 0 doubles as the
nil value for root pointers, which is unambiguous because page 0 is always
a meta page.

## Page header

Every page, meta pages included, starts with the same 16 byte header:

```
offset  size  field
0       4     crc32c checksum over bytes [4 .. page_size]
4       1     page type
5       1     flags (reserved, 0 in version 1)
6       2     entry count / used bytes, meaning owned by the page type
8       8     lsn: txid of the commit that sealed this page
```

Page types in version 1:

```
0  meta
1  btree branch
2  btree leaf
3  overflow
4  free list         (reserved, phase 3)
5  blob              (reserved, phase 6)
6  commit record
```

The checksum is written when a page is sealed at commit time and verified on
every read from storage. A page whose checksum does not verify is reported
as corruption, never returned to a caller.

## Meta page body

Immediately after the 16 byte header:

```
offset  size  field
16      8     magic, the ASCII bytes "QUANTYDB"
24      4     format version, 1
28      4     page size in bytes
32      8     txid of this commit
40      8     data root page id       (0 = none)
48      8     catalog root page id    (0 = none)
56      8     free list root page id  (0 = none)
64      8     page count: total pages in the file, metas included
72      8     commit wall clock time, unix milliseconds (informational)
80      8     newest commit record page (0 = none)
```

The rest of the page is zero. The checksum covers the whole page, zeros
included. A meta page is valid when its checksum verifies, the magic and
version match, the page size is legal and matches the slot the meta was read
from, page count is at least 2, and every non-nil root points inside
`[2, page_count)`.

## B-tree nodes

Keys are raw bytes, compared bytewise; the key encoding in quanty-core
guarantees byte order equals logical order for typed tuples. The longest
allowed key is `page_size / 8`. Values up to `page_size / 4` live inline,
larger ones move to an overflow chain.

Leaf body, after the header (the header count field holds the entry count,
cells are stored back to back in key order):

```
per cell:
  2     key length (u16)
  1     value flag: 0 = inline, 1 = overflow
  klen  key bytes
  if inline:   4 value length (u32), then the value bytes
  if overflow: 8 head page id, 8 total value length
```

Branch body (count field = number of separator entries):

```
8       leftmost child page id (keys below the first separator)
per cell:
  2     key length (u16)
  1     flag, must be 0 in version 1
  klen  separator key: the lowest key reachable through this child
  8     child page id
```

Keys in a node are strictly ascending; readers treat violations as
corruption.

## Overflow chains

A value too big for its leaf is split across a chain of overflow pages:

```
8       next page in the chain (0 = last)
2       bytes used in this page (u16)
n       value bytes
```

Each page carries up to `page_size - 26` value bytes. The leaf cell stores
the head page and the total length; readers verify the reassembled length
and reject chains that end early, run long or loop.

## Commit records

Every commit writes one commit record page; the meta points at the newest
one, forming a chain back through history. This is what snapshots of old
commits and, later, branches hang off.

```
16      8     commit id (equals the txid that sealed this page)
24      8     parent commit id (0 = the empty initial state)
32      8     data root page at this commit
40      8     catalog root page at this commit
48      8     page of the parent's commit record (0 = none)
56      8     wall clock, unix milliseconds (informational)
```

## Commit protocol

Creation writes an identical txid 0 meta into both slots and syncs.

A commit with transaction id T does, in order:

1. seal every page written by the transaction (stamp lsn = T, compute
   checksum) and write it to its page offset
2. fsync
3. encode the meta for T and write it to slot `T % 2`
4. fsync

Step 4 is the commit point. Two invariants make this safe without a WAL:

- **Alternating slots.** T and T-1 always live in different slots, so a torn
  meta write can only damage the slot being written, never the previous
  commit's meta.
- **Copy-on-write.** Transaction T only ever writes to page ids that no meta
  with txid <= T-1 references (fresh allocations past the old page count;
  reused free pages follow the same rule once the free list exists). A crash
  during step 1 or 2 therefore cannot damage any committed state.

## Recovery

On open, read both meta slots and every legal page size candidate (the size
field inside a corrupted meta cannot be trusted, and there are only eight
legal sizes, so all of them are tried). Collect every meta that validates,
pick the one with the highest txid. The pages beyond it are garbage from an
interrupted commit and are simply unreachable.

If at least one candidate carries the magic but none validates, the file is
reported as corrupted. If no candidate carries the magic, the file is not a
Quanty database.

## Compatibility rules

- Readers must reject any format version they do not know.
- Flags and reserved page types must be zero / unused in version 1; readers
  treat violations as corruption.
- Additive changes (new page types, new meta fields inside the zero region)
  bump the version. Nothing is reinterpreted in place, ever.

## Logical layer (phase 2)

Everything below is plain keys and values in the two trees; nothing here
adds page types or touches the physical layer.

Keys use the tuple encoding from `quanty-core::encoding`: type-tagged,
order preserving, self delimiting. `(a, b)` denotes such a tuple.

### Catalog tree

```
("seq")            next object id, u64 LE (tables and indexes share it)
("table", name)    serialized table definition
```

Table definitions serialize as: version byte (1), table id u64, name
(u16 length + UTF-8), column count u16, then per column: name, type tag
(0 int, 1 float, 2 text, 3 bytes, 4 bool), flag byte (bit 0 key, bit 1
nullable), index id u64 (0 = none), default marker byte (0 = none,
1 = tuple-encoded value with u32 length prefix). All integers little
endian.

### Data tree

```
(table_id, pk...)               row; value = tuple encoding of all columns
                                in declaration order
(index_id, value, pk...)        secondary index entry; value is empty
```

Object ids are encoded as int elements, so every table and every index
owns one contiguous key range and scans are prefix ranges. Appending the
primary key to index entries keeps duplicate column values apart without
any extra bookkeeping.

Schema and rows commit through the same meta, so a table definition and
its data can never diverge, and time travel reads the schema of the
commit it looks at.
