# updates and deletes; the sql mirror of 05_set_del.qql

> CREATE TABLE u (id INTEGER PRIMARY KEY, score INTEGER NOT NULL DEFAULT 0, name TEXT)
ok

> INSERT INTO u (id, name) VALUES (1, 'a')
put 1

> INSERT INTO u (id, name, score) VALUES (2, 'b', 5)
put 1

> INSERT INTO u (id, score) VALUES (3, 9)
put 1

> UPDATE u SET score = score + 10 WHERE id = 2
set 1

> SELECT * FROM u WHERE id = 2
2|15|b

> UPDATE u SET score = score * 2
set 3

> SELECT * FROM u
1|0|a
2|30|b
3|18|null

> UPDATE u SET score = 0 WHERE score > 100
set 0

> UPDATE u SET id = 99 WHERE id = 1
error: cannot set key column 'id' (comes with row moves in phase 3)

> UPDATE u SET score = 'x' WHERE id = 1
error: column 'score': a text value does not fit into a column of type int

> UPDATE u SET name = NULL WHERE id = 1
set 1

> SELECT * FROM u WHERE id = 1
1|0|null

> UPDATE u SET score = score / 0 WHERE id = 3
error: division by zero

> UPDATE u SET score = score - 5, score = score - 5 WHERE id = 2
set 1

# both assignments read the old row: 30-5 twice lands on 25, not 20
> SELECT * FROM u WHERE id = 2
2|25|b

> DELETE FROM u WHERE id = 2
del 1

> SELECT * FROM u
1|0|null
3|18|null

> DELETE FROM u WHERE score > 5
del 1

> DELETE FROM u
del 1

> SELECT * FROM u

> DELETE FROM u
del 0

> CREATE TABLE sw (id INTEGER PRIMARY KEY, a INTEGER NOT NULL DEFAULT 0, b INTEGER NOT NULL DEFAULT 0)
ok

> INSERT INTO sw (id, a, b) VALUES (1, 1, 2)
put 1

# assignments see the row as it was before the update, so this swaps
> UPDATE sw SET a = b, b = a
set 1

> SELECT * FROM sw
1|2|1
