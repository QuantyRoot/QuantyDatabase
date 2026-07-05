# parse errors point at the problem; plan errors name the missing thing
# the sql mirror of 09_errors.qql

> bogus stuff
error: parse error at byte 0: expected a statement (select, insert, update, delete, create, drop, explain), found 'bogus'

> SELECT
error: parse error at byte 6: the select list takes plain column names or * for now; expressions are not supported yet

> SELECT * FROM users
error: no table named 'users'

> CREATE TABLE t (id INTEGER PRIMARY KEY)
ok

> SELECT * FROM t WHERE nope = 1
error: table 't' has no column 'nope'

> SELECT nope FROM t
error: table 't' has no column 'nope'

> SELECT * FROM t ORDER BY nope
error: cannot order by unknown column 'nope'

> UPDATE t SET nope = 2 WHERE id = 1
error: table 't' has no column 'nope'

> DELETE FROM t WHERE nope = 1
error: table 't' has no column 'nope'

> SELECT * FROM t WHERE
error: parse error at byte 21: expected a value or column, found the end of the statement

> SELECT * FROM t WHERE id =
error: parse error at byte 26: expected a value or column, found the end of the statement

> CREATE TABLE t2 (id INTT PRIMARY KEY)
error: parse error at byte 20: unknown column type 'intt' (use integer, real, text, blob or boolean)

> CREATE TABLE t3 (id INTEGER NOPE)
error: parse error at byte 28: expected ')' after the column list, found 'NOPE'

> SELECT * FROM t LIMIT -1
error: parse error at byte 22: limit takes a plain non-negative integer for now

> INSERT INTO t () VALUES ()
error: parse error at byte 15: expected a column name, found ')'

> INSERT INTO t (id) VALUES (1) trailing
error: parse error at byte 30: one statement per call; found more input after the statement

> SELECT * FROM t WHERE id = 99999999999999999999
error: parse error at byte 27: integer too large for int

> SELECT * FROM t WHERE id = 1e999
error: parse error at byte 27: float literal is out of range

> INSERT INTO t (id) VALUES (1)
put 1

> SELECT * FROM t WHERE id
error: the condition is a int, not a bool

> UPDATE t SET
error: parse error at byte 12: expected a column name, found the end of the statement
