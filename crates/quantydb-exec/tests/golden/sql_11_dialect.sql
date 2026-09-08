# the sql surface itself: quoting, case rules, operators, literals.
# semantics decisions live in docs/SQL.md and ADR-014.

# keywords match in any case; identifiers keep the case they were written in
> CrEaTe TaBlE Album (AlbumId INTEGER PRIMARY KEY, Title NVARCHAR(160) NOT NULL)
ok

> INSERT INTO Album (AlbumId, Title) VALUES (1, 'Kind of Blue'), (2, 'Aja')
put 2

> SeLeCt Title FrOm Album WhErE AlbumId = 2
Aja

# name matching is exact-case; album is not Album
> SELECT * FROM album
error: no table named 'album'

# three quoting styles, all bypassing reserved words
> CREATE TABLE q (id INTEGER PRIMARY KEY, "order" INTEGER NOT NULL DEFAULT 0, [limit] TEXT, `set` BOOLEAN)
ok

> INSERT INTO q (id, "order", [limit], `set`) VALUES (1, 5, 'x', true), (2, 9, NULL, false)
put 2

> SELECT "order", [limit] FROM q WHERE `set` = true
5|x

# unquoted reserved words do not sneak through as names
> SELECT order FROM q
error: parse error at byte 7: 'order' is a reserved word here; quote it ("order") to use it as a name

# strings escape quotes by doubling them; backslashes are just bytes
> INSERT INTO q (id, [limit]) VALUES (3, 'it''s a \n')
put 1

> SELECT [limit] FROM q WHERE id = 3
it's a \n

# = and == are the same operator, != and <> are the same operator
> SELECT id FROM q WHERE "order" == 5
1

> SELECT id FROM q WHERE "order" <> 5 AND "order" != 4
2
3

# is null / is not null; and is as the general null-safe comparison
> SELECT id FROM q WHERE [limit] IS NULL
2

> SELECT id FROM q WHERE [limit] IS NOT NULL
1
3

> SELECT id FROM q WHERE [limit] IS 'x'
1

# a comparison spelled with the null literal is refused, not guessed at
> SELECT id FROM q WHERE [limit] = NULL
error: parse error at byte 31: comparing with null never matches in sql; write is null or is not null

> SELECT id FROM q WHERE [limit] < NULL
error: parse error at byte 31: comparing with null never matches in sql; write is null or is not null

# || concatenates; + on text concatenates too (engine rule, not sqlite's)
> SELECT id FROM q WHERE [limit] || 'y' = 'xy'
1

> SELECT id FROM q WHERE [limit] + 'y' = 'xy'
1

# comments in both styles
> SELECT id FROM q WHERE id = 3 -- trailing comment
3

> /* leading comment */ SELECT id FROM q WHERE id = 3
3

# number shapes: .5 and 5. are floats, 0x1f is an int
> CREATE TABLE nums (id INTEGER PRIMARY KEY, f REAL)
ok

> INSERT INTO nums (id, f) VALUES (1, .5), (2, 5.), (0x10, 1.25)
put 3

> SELECT * FROM nums WHERE f = 0.5
1|0.5

> SELECT * FROM nums WHERE id = 16
16|1.25

# blob literals accept both hex cases and render canonically
> CREATE TABLE bl (id INTEGER PRIMARY KEY, x BLOB NOT NULL)
ok

> INSERT INTO bl (id, x) VALUES (1, x'C0FFEE'), (2, X'00')
put 2

> SELECT * FROM bl WHERE x = x'c0ffee'
1|x"c0ffee"

# a trailing semicolon is fine
> SELECT id FROM bl WHERE id = 1;
1

# without rowid and strict are properties this engine has anyway
> CREATE TABLE wr (a INTEGER PRIMARY KEY, b TEXT) WITHOUT ROWID, STRICT
ok

> INSERT INTO wr (a, b) VALUES (1, 'x')
put 1

> SELECT * FROM wr
1|x

# foreign keys parse and are not enforced, sqlite's default behavior;
# the referenced table does not even exist here
> CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER NOT NULL REFERENCES parent (id) ON DELETE CASCADE)
ok

> INSERT INTO child (id, pid) VALUES (1, 999)
put 1

> SELECT * FROM child
1|999

# sqlite affinity mapping: numeric and decimal are float, the date family
# is text, varchar lengths parse and mean nothing
> CREATE TABLE inv (id INTEGER PRIMARY KEY, total NUMERIC(10,2) NOT NULL, at DATETIME NOT NULL, who CHARACTER VARYING(70))
ok

> INSERT INTO inv (id, total, at) VALUES (1, 0.99, '2009-01-01 00:00:00')
put 1

> SELECT * FROM inv
1|0.99|2009-01-01 00:00:00|null

# int literals widen into the numeric-as-float column
> INSERT INTO inv (id, total, at) VALUES (2, 5, '2009-01-02 00:00:00')
put 1

> SELECT total FROM inv WHERE id = 2
5.0

# explain query plan is the long spelling of explain
> EXPLAIN QUERY PLAN SELECT * FROM inv WHERE id = 1
KeyLookup inv (id = 1)
