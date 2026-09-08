# type behavior across the whole surface; the sql mirror of 08_types.qql

> CREATE TABLE t (id INTEGER PRIMARY KEY, f REAL, b BOOLEAN, x BLOB, s TEXT)
ok

> INSERT INTO t (id, f, b, x, s) VALUES (1, 1.5, true, x'c0ffee', 'grüezi')
put 1

> SELECT * FROM t WHERE id = 1
1|1.5|true|x"c0ffee"|grüezi

> INSERT INTO t (id, f) VALUES (2, 3)
put 1

# int literals widen into float columns and stay floats
> SELECT * FROM t WHERE id = 2
2|3.0|null|null|null

> INSERT INTO t (id, f) VALUES (3, 2.5e2)
put 1

> SELECT f FROM t WHERE id = 3
250.0

> INSERT INTO t (id, b) VALUES (4, false)
put 1

> INSERT INTO t (id, s) VALUES (5, 'quote"d')
put 1

> SELECT s FROM t WHERE id = 5
quote"d

> INSERT INTO t (id, f) VALUES (6, -0.5)
put 1

> SELECT * FROM t WHERE f < 0
6|-0.5|null|null|null

> INSERT INTO t (id, s) VALUES (-9223372036854775807, 'min')
put 1

> SELECT id FROM t WHERE s = 'min'
-9223372036854775807

> SELECT * FROM t WHERE b = true
1|1.5|true|x"c0ffee"|grüezi

> SELECT * FROM t WHERE b = false
4|null|false|null|null

> SELECT * FROM t WHERE f = 250
3|250.0|null|null|null

> INSERT INTO t (id, f) VALUES (7, 1 / 2)
put 1

# 1 / 2 is int division before the widening, docs call this out
> SELECT f FROM t WHERE id = 7
0.0

> INSERT INTO t (id, f) VALUES (8, 1.0 / 2)
put 1

> SELECT f FROM t WHERE id = 8
0.5

> INSERT INTO t (id, b) VALUES (9, 1)
error: column 'b': a int value does not fit into a column of type bool

> INSERT INTO t (id, x) VALUES (10, 'nope')
error: column 'x': a text value does not fit into a column of type bytes

> INSERT INTO t (id) VALUES (1.5)
error: column 'id': a float value does not fit into a column of type int

> CREATE TABLE fo (id INTEGER PRIMARY KEY, f REAL NOT NULL)
ok

> INSERT INTO fo (id, f) VALUES (1, 0.5), (2, -1.5), (3, 10.0)
put 3

> SELECT * FROM fo ORDER BY f
2|-1.5
1|0.5
3|10.0

> SELECT * FROM fo WHERE f = 10
3|10.0

> CREATE TABLE bo (id INTEGER PRIMARY KEY, x BLOB NOT NULL)
ok

> CREATE INDEX idx_bo_x ON bo (x)
ok

> INSERT INTO bo (id, x) VALUES (1, x'00'), (2, x'ff'), (3, x'00ff')
put 3

> SELECT * FROM bo WHERE x = x'00ff'
3|x"00ff"

> SELECT * FROM bo ORDER BY x
1|x"00"
3|x"00ff"
2|x"ff"

> CREATE TABLE tc (id INTEGER PRIMARY KEY, s TEXT NOT NULL)
ok

> INSERT INTO tc (id, s) VALUES (1, 'ab')
put 1

> UPDATE tc SET s = s || 'cd'
set 1

> SELECT * FROM tc
1|abcd
