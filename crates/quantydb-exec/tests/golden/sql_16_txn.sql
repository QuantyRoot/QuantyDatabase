# explicit transactions, the shared cases of 16_txn.qql. as of, branch and
# gc are qql-only, so the history and branch lines are not mirrored here.

> CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)
ok
> INSERT INTO t (id, n) VALUES (1, 10)
put 1

# a rolled back transaction leaves no trace
> BEGIN
ok
> INSERT INTO t (id, n) VALUES (2, 20)
put 1
> SELECT * FROM t
1|10
2|20
> ROLLBACK
ok
> SELECT * FROM t
1|10

# a committed transaction applies all its statements at once
> BEGIN
ok
> INSERT INTO t (id, n) VALUES (3, 30)
put 1
> UPDATE t SET n = n + 5 WHERE id = 1
set 1
> DELETE FROM t WHERE id = 3
del 1
> INSERT INTO t (id, n) VALUES (4, 40)
put 1
> SELECT * FROM t ORDER BY id
1|15
4|40
> COMMIT
ok
> SELECT * FROM t ORDER BY id
1|15
4|40

# control errors
> COMMIT
error: no transaction is open
> ROLLBACK
error: no transaction is open

# nesting is refused
> BEGIN
ok
> BEGIN
error: a transaction is already open; commit or rollback first

# a failing statement keeps the transaction open and buffers nothing
> INSERT INTO t (id, n) VALUES (1, 99)
error: duplicate key (1) in table 't'
> SELECT * FROM t ORDER BY id
1|15
4|40
> ROLLBACK
ok

# the commit spelled END, and the transaction keyword, both parse
> BEGIN TRANSACTION
ok
> INSERT INTO t (id, n) VALUES (6, 60)
put 1
> END TRANSACTION
ok
> SELECT * FROM t ORDER BY id
1|15
4|40
6|60

# explain works inside a transaction, over the pending schema and data
> BEGIN
ok
> CREATE TABLE u (id INTEGER PRIMARY KEY, t_id INTEGER)
ok
> CREATE INDEX idx_u_t ON u (t_id)
ok
> EXPLAIN SELECT * FROM u WHERE t_id = 1
IndexScan u via t_id = 1
> ROLLBACK
ok

# u never existed outside the rolled back transaction
> SELECT * FROM u
error: no table named 'u'
