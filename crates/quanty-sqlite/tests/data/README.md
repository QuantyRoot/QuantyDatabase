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

## chinook.oracle

One line per table, `<name> <row count> <sha256>`, plus a total. The digest
covers every row of that table in rowid order, and it was produced by the
real SQLite library rather than by this crate, so a reader that is wrong in
the same way as its expectations is not a failure mode that exists here.

Each row is rendered as the rowid, then the columns as they are physically
stored, fields separated by byte 0x1f and the row terminated by 0x0a:

- rowid: `r:` then the decimal value
- SQL NULL: `null`
- integer: `i:` then the decimal value
- real: `f:` then 16 lowercase hex digits of the big endian IEEE 754 bits,
  because no float formatting rule then has to agree across two languages
- text: `t:` then the raw UTF-8 bytes
- blob: `b:` then lowercase hex

A column declared `integer primary key` is an alias for the rowid, and
SQLite stores NULL in its place in the record. The oracle renders NULL there
too and carries the rowid separately, so this compares the physical layout.
Turning that NULL back into the value a user would see is a decision about
what a row means, and it belongs to the importer.

To regenerate:

```python
import hashlib, sqlite3, struct
DB = "chinook.sqlite"
con = sqlite3.connect(DB)

def alias_column(table):
    cols = con.execute(f'pragma table_info("{table}")').fetchall()
    pks = [c for c in cols if c[5] != 0]
    if len(pks) != 1:
        return None
    return pks[0][1] if (pks[0][2] or "").strip().upper() == "INTEGER" else None

def render(v):
    if v is None: return b"null"
    if isinstance(v, int): return b"i:" + str(v).encode()
    if isinstance(v, float): return b"f:" + struct.pack(">d", v).hex().encode()
    if isinstance(v, str): return b"t:" + v.encode("utf-8")
    if isinstance(v, bytes): return b"b:" + v.hex().encode()
    raise TypeError(type(v))

tables = sorted(t for (t,) in con.execute(
    "select name from sqlite_master where type='table'").fetchall())
lines, total = [], 0
for table in tables:
    cols = [c[1] for c in con.execute(f'pragma table_info("{table}")').fetchall()]
    alias = alias_column(table)
    quoted = ", ".join(f'"{c}"' for c in cols)
    rows = con.execute(f'select rowid, {quoted} from "{table}" order by rowid').fetchall()
    digest = hashlib.sha256()
    for row in rows:
        values = list(row[1:])
        if alias is not None:
            values[cols.index(alias)] = None
        parts = [b"r:" + str(row[0]).encode()] + [render(v) for v in values]
        digest.update(b"\x1f".join(parts) + b"\n")
    lines.append(f"{table} {len(rows)} {digest.hexdigest()}")
    total += len(rows)
lines.append(f"# total rows {total}")
open("chinook.oracle", "w").write("\n".join(lines) + "\n")
```

## rowid_alias.sqlite

Seven tables, one per way of declaring a single column integer primary key,
each holding one row whose key is 7. Whether that column is an alias for the
rowid decides where the 7 is: an alias means the record stores NULL and the
cell's rowid holds 7, no alias means the record holds 7 and the rowid is 1.
So the file itself answers the question, and the parser's rule can be
checked against it rather than against a reading of the documentation.

| table               | declaration                       | alias |
|---------------------|-----------------------------------|-------|
| a_col_pk            | `x integer primary key`           | yes   |
| b_col_pk_desc       | `x integer primary key desc`      | no    |
| c_tbl_pk            | `x integer, primary key (x)`      | yes   |
| d_tbl_pk_desc       | `x integer, primary key (x desc)` | yes   |
| e_int_not_integer   | `x int primary key`               | no    |
| f_autoinc           | `x integer primary key autoincrement` | yes |
| g_mixed_case        | `x InTeGeR primary key`           | yes   |

The two `desc` rows disagreeing is the point of the fixture. Written as a
column constraint, `desc` suppresses the alias; written as a table
constraint it does not. SQLite documents that as a quirk kept for backwards
compatibility, and a reader that gets it backwards reads the primary key of
a whole table wrongly without anything failing.

Generated with any sqlite3:

```python
import sqlite3
con = sqlite3.connect("rowid_alias.sqlite")
con.execute("pragma page_size=512")
for name, ddl in {
    "a_col_pk": "create table a_col_pk (x integer primary key, y text)",
    "b_col_pk_desc": "create table b_col_pk_desc (x integer primary key desc, y text)",
    "c_tbl_pk": "create table c_tbl_pk (x integer, y text, primary key (x))",
    "d_tbl_pk_desc": "create table d_tbl_pk_desc (x integer, y text, primary key (x desc))",
    "e_int_not_integer": "create table e_int_not_integer (x int primary key, y text)",
    "f_autoinc": "create table f_autoinc (x integer primary key autoincrement, y text)",
    "g_mixed_case": "create table g_mixed_case (x InTeGeR primary key, y text)",
}.items():
    con.execute(ddl)
    con.execute(f"insert into {name} values (7, 'hi')")
con.commit()
```

## shapes.sqlite

Five tables, each one a shape that breaks a naive importer. None of them is
exotic; every one of these turns up in databases people actually have.

`generated (id, a, v virtual, s stored, z)`: a virtual generated column
between real ones. The record holds four values for five declared columns,
because a virtual column is computed on demand and never written. Zipping
the declaration against the record puts `s`'s value into `v`, `z`'s into
`s`, and leaves `z` empty, and every one of those looks like a plausible
row. A generated column with neither `stored` nor `virtual` written out is
virtual, which the file confirms rather than the documentation.

`grown (id, a, b default 'fallback', c)`: two rows written before two
columns were added with `alter table add column`. SQLite does not rewrite
existing rows, so those records hold two values while the table has four
columns, and the missing ones take their default from the declaration.
SQLite does rewrite the stored create statement, so the defaults are there
to be read.

`affinity (id, r real, n numeric, i integer, u)`: the same values, 1.0, 2.5,
3.0 and 4.0, in columns of four different affinities, plus one text value in
the untyped column. In `r` the whole numbers are stored as integers and are
floats on the way out. In `n` the conversion is permanent and the column
really does hold both integers and reals. In `u` nothing was converted at
all. One set of values, three different answers, all decided by the
declared type.

`nopk (a, b)`: no primary key. The rows are still identified by their
rowids, which is what has to become the key when nothing else can.

`nullable_pk (k text primary key, v)`: two rows whose primary key is NULL.
A primary key that is not an alias for the rowid may hold NULL in SQLite, a
backwards compatibility quirk it still honours, and our key columns may
not. Without this fixture that case is only ever found by a user.

Generated with any sqlite3:

```python
import sqlite3
con = sqlite3.connect("shapes.sqlite")
con.execute("pragma page_size = 512")
con.execute("pragma journal_mode = delete")
con.execute("""create table generated (
    id integer primary key, a integer,
    v integer as (a * 2) virtual, s integer as (a * 3) stored, z text)""")
for a in (1, 2, 3):
    con.execute("insert into generated (id, a, z) values (?, ?, ?)", (a, a * 10, f"z{a}"))
con.execute("create table grown (id integer primary key, a integer)")
for i in (1, 2):
    con.execute("insert into grown values (?, ?)", (i, i * 100))
con.execute("alter table grown add column b text default 'fallback'")
con.execute("alter table grown add column c integer")
con.execute("insert into grown values (3, 300, 'echt', 42)")
con.execute("create table affinity (id integer primary key, r real, n numeric, i integer, u)")
for k, v in enumerate([1.0, 2.5, 3.0], start=1):
    con.execute("insert into affinity values (?,?,?,?,?)", (k, v, v, int(v), v))
con.execute("insert into affinity values (4, 4.0, 4.0, 4, 'text statt zahl')")
con.execute("create table nopk (a text, b integer)")
con.execute("insert into nopk values ('p', 1)")
con.execute("insert into nopk values ('q', 2)")
con.execute("create table nullable_pk (k text primary key, v text)")
con.execute("insert into nullable_pk values ('a', 'eins')")
con.execute("insert into nullable_pk values (NULL, 'nix')")
con.execute("insert into nullable_pk values (NULL, 'auch nix')")
con.commit()
con.execute("vacuum")
```

## without_rowid.sqlite and without_rowid.oracle

Six tables, all `without rowid`, which means each one is stored as an index
b-tree keyed by its primary key rather than as a table b-tree. The record of
such a table holds the key columns first in key order, then the rest in
declared order, and nothing in the bytes says so. That permutation is the
reason a reader has to parse the create statement before it can read one of
these at all, and this fixture is what proves it undoes it.

Page size 512, so `kv` needs interior index pages and the walk is a real
in-order traversal rather than one leaf.

- `kv (k text primary key, v integer not null)`: 500 rows, key `key-%04d`
  for 0 to 499 and value three times the number.
- `two (a text, b integer, c text, d text, primary key (c, a))`: 5 rows of
  `aN, N, cN, dN`. Declared a, b, c, d and keyed (c, a), so the record holds
  c, a, b, d, and a reader that ignores the permutation returns the columns
  shuffled while looking perfectly healthy.
- `grown_wr`: two rows written, then `alter table add column extra text
  default 'spaeter'`, then two more. The first two have records that end
  early, so the column reads as missing rather than as its default; turning
  that into the default is the importer's job.
- `uni`: keys outside ASCII, ordered by their UTF-8 bytes.
- `desc_key (k integer primary key desc)`: the tree is in descending order,
  and a scan reports the order that is there rather than one it prefers.
- `int_pk (k integer primary key)`: in a rowid table this column would alias
  the rowid and be stored as NULL. In a without rowid table there is no
  rowid to alias, so the value really is in the record.

The oracle has the same shape as chinook.oracle, `<table> <rows> <sha256>`,
with rows rendered in *declared* order and each value rendered as described
above. A column that a record ends before is rendered as the six bytes
`missing`. Rows are hashed in the order the tree holds them, which is the
`order by` given in the generator below.

To regenerate both files:

```python
import hashlib, os, sqlite3, struct
OUT = "without_rowid.sqlite"
if os.path.exists(OUT):
    os.remove(OUT)
con = sqlite3.connect(OUT)
con.execute("pragma page_size = 512")
con.execute("pragma journal_mode = delete")
con.execute("create table kv (k text primary key, v integer not null) without rowid")
for i in range(500):
    con.execute("insert into kv values (?, ?)", ("key-%04d" % i, i * 3))
con.execute("""create table two (
    a text, b integer, c text, d text, primary key (c, a)) without rowid""")
for i in range(5):
    con.execute("insert into two values (?, ?, ?, ?)", ("a%d" % i, i, "c%d" % i, "d%d" % i))
con.execute("create table grown_wr (k text primary key, v text) without rowid")
con.execute("insert into grown_wr values ('k0', 'v0')")
con.execute("insert into grown_wr values ('k1', 'v1')")
con.execute("alter table grown_wr add column extra text default 'spaeter'")
con.execute("insert into grown_wr values ('k8', 'v8', 'direkt')")
con.execute("insert into grown_wr values ('k9', 'v9', 'direkt')")
short_rows = {"k0", "k1"}
con.execute("create table uni (k text primary key, v text) without rowid")
for k, v in [("z", "drei"), ("\u00e4pfel", "eins"), ("\u65e5\u672c", "zwei")]:
    con.execute("insert into uni values (?, ?)", (k, v))
con.execute("create table desc_key (k integer primary key desc, v text) without rowid")
for i in range(6):
    con.execute("insert into desc_key values (?, ?)", (i, "v%d" % i))
con.execute("create table int_pk (k integer primary key, v text) without rowid")
for i in [1, 2, 100]:
    con.execute("insert into int_pk values (?, ?)", (i, "v%d" % i))
con.commit()
con.close()

con = sqlite3.connect(OUT)
def render(v):
    if v is None: return b"null"
    if isinstance(v, int): return b"i:" + str(v).encode()
    if isinstance(v, float): return b"f:" + struct.pack(">d", v).hex().encode()
    if isinstance(v, str): return b"t:" + v.encode("utf-8")
    if isinstance(v, bytes): return b"b:" + v.hex().encode()
    raise TypeError(type(v))
plan = {"kv": ("k", set()), "two": ("c, a", set()), "grown_wr": ("k", short_rows),
        "uni": ("k", set()), "desc_key": ("k desc", set()), "int_pk": ("k", set())}
lines, total = [], 0
for table, (order, short) in sorted(plan.items()):
    cols = [c[1] for c in con.execute('pragma table_info("%s")' % table).fetchall()]
    quoted = ", ".join('"%s"' % c for c in cols)
    rows = con.execute('select %s from "%s" order by %s' % (quoted, table, order)).fetchall()
    digest = hashlib.sha256()
    for row in rows:
        parts = [b"missing" if (name == "extra" and row[0] in short) else render(value)
                 for name, value in zip(cols, row)]
        digest.update(b"\x1f".join(parts) + b"\n")
    lines.append("%s %d %s" % (table, len(rows), digest.hexdigest()))
    total += len(rows)
lines.append("# total rows %d" % total)
open("without_rowid.oracle", "w").write("\n".join(lines) + "\n")
```
