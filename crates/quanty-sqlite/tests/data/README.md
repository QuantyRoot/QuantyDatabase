# Test data

`chinook.sqlite` is the Chinook sample database, byte for byte as published
in release v1.4.5 of https://github.com/lerocha/chinook-database, file
`Chinook_Sqlite.sqlite`. It is MIT licensed, copyright Luis Rocha; the
license text sits next to it in LICENSE.chinook.txt.

It is checked in rather than downloaded because a test that needs the
network is not a test. One megabyte in git is a fair price for having the
acceptance corpus present in every checkout and every CI run.

Why this file specifically. It is a real database written by a real SQLite
(3.36.0), not something we generated to match our own reader, and it covers
the parts of the format that are easy to get wrong:

- 11 tables and 11 indexes, so `sqlite_master` has both kinds in it
- 15607 rows, enough that a wrong page traversal shows up as a wrong count
- interior and leaf pages, since the larger tables do not fit one page
- a composite primary key on PlaylistTrack, which is a table with a
  separate index rather than a rowid alias
- nullable columns with real NULLs in them
- declared types the SQL standard does not have (NVARCHAR, NUMERIC,
  DATETIME), which is where type mapping has to make a decision
- text values outside ASCII (accented names in several tables)
- a non-empty freelist (199 free pages), so a reader that walks the file
  linearly instead of following the b-trees gets caught
- page size 1024 rather than the 4096 default, so a hard coded page size
  gets caught

The header, for reference when reading test expectations: page size 1024,
1042 pages, reserved space 0, text encoding UTF-8, schema format 4, first
freelist trunk page 8.

## records.sqlite

Chinook has no overflow pages at all, so it cannot exercise the rule that
decides how much of a payload stays on its page. This second fixture exists
for that rule and for the serial types, and it was written by a real SQLite
(via Python's sqlite3 module) so that the bytes are somebody else's opinion
of the format, not ours.

Page size 512, which puts the boundaries at: 477 payload bytes local on a
table leaf, 102 on an index page, 39 when a payload spills to the minimum,
and 508 payload bytes per overflow page. 202 of its 237 pages are overflow
pages.

The content is defined by rules rather than by a dump, so the tests
recompute the expected values instead of trusting a stored copy of them:

- `spill(n integer primary key, v text not null)`: for each n in 0, 1, 57,
  58, 100, 400, then every value from 468 to 486, then 600, 1000, 5000 and
  50000, one row whose text is n characters long. The text of length n is
  the lowercase alphabet repeated and rotated by n, so character i is
  `'a' + ((i + n) % 26)`. The run from 468 to 486 straddles the 477 byte
  boundary from both sides, one byte at a time.
- `blobs(n integer primary key, b blob not null)`: same idea for blobs, with
  byte i of the blob of length n being `(i * 7 + n) % 256`, for n in 0, 1,
  300, 474, 475, 476, 508, 509, 1016, 1017 and 20000. The values around 508
  and 1016 land on exact multiples of the overflow page capacity, where an
  off by one produces an empty trailing page.
- `kinds(id integer primary key, v)`: one row per serial type, including
  both integer widths at their extremes, the two types that encode the
  values 0 and 1 in the header alone, empty text, empty blob and NULL.
- `unicode(id integer primary key, v text)`: text that is not ASCII,
  including one value long enough to spill mid character.
- `idx_spill(n integer primary key, v text)` with an index on `v`: text
  lengths from 90 to 115 plus 400 and 3000, which straddles the 102 byte
  index boundary, because index pages keep much less local than table pages
  and use a different formula.

To regenerate it, run this against any sqlite3, then move the file here:

```python
import os, sqlite3
OUT = "records.sqlite"
if os.path.exists(OUT):
    os.remove(OUT)
con = sqlite3.connect(OUT)
con.execute("pragma page_size = 512")
con.execute("pragma journal_mode = delete")
text_of = lambda n: "".join(chr(ord("a") + ((i + n) % 26)) for i in range(n))
blob_of = lambda n: bytes(((i * 7 + n) % 256) for i in range(n))
con.execute("create table spill (n integer primary key, v text not null)")
for n in [0, 1, 57, 58, 100, 400] + list(range(468, 487)) + [600, 1000, 5000, 50000]:
    con.execute("insert into spill values (?, ?)", (n, text_of(n)))
con.execute("create table blobs (n integer primary key, b blob not null)")
for n in [0, 1, 300, 474, 475, 476, 508, 509, 1016, 1017, 20000]:
    con.execute("insert into blobs values (?, ?)", (n, blob_of(n)))
con.execute("create table kinds (id integer primary key, v)")
for id_, v in [(1, None), (2, -128), (3, 127), (4, -32768), (5, 32767),
               (6, -8388608), (7, 8388607), (8, -2147483648), (9, 2147483647),
               (10, -140737488355328), (11, 140737488355327),
               (12, -9223372036854775808), (13, 9223372036854775807),
               (14, 0.5), (15, -2.25), (16, 1.7976931348623157e308),
               (17, 0), (18, 1), (19, ""), (20, b""),
               (21, "grus, Zurich"), (22, "japanisch: konnichiwa"),
               (23, b"\x00\x01\xfe\xff")]:
    con.execute("insert into kinds values (?, ?)", (id_, v))
con.execute("create table unicode (id integer primary key, v text)")
for id_, v in [(1, "\u00fcber"), (2, "\u65e5\u672c\u8a9e"),
               (3, "\U0001f600 emoji"), (4, "a" + "\u00e9" * 300)]:
    con.execute("insert into unicode values (?, ?)", (id_, v))
con.execute("create table idx_spill (n integer primary key, v text)")
for n in list(range(90, 116)) + [400, 3000]:
    con.execute("insert into idx_spill values (?, ?)", (n, text_of(n)))
con.execute("create index idx_spill_v on idx_spill (v)")
con.commit()
con.execute("vacuum")
con.close()
```
