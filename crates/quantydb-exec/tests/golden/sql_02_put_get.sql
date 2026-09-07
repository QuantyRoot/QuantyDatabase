# inserts, defaults, nulls, projections; the sql mirror of 02_put_get.qql

> CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score INTEGER NOT NULL DEFAULT 0, bio TEXT)
ok

> INSERT INTO users (id, name) VALUES (1, 'elchi')
put 1

> INSERT INTO users (id, name, score, bio) VALUES (2, 'mira', 7, 'hi')
put 1

> INSERT INTO users (id, name) VALUES (3, 'nox')
put 1

> INSERT INTO users (id, name, score) VALUES (4, 'rex', 2)
put 1

> SELECT * FROM users
1|elchi|0|null
2|mira|7|hi
3|nox|0|null
4|rex|2|null

> SELECT name FROM users
elchi
mira
nox
rex

> SELECT score, id FROM users
0|1
7|2
0|3
2|4

> SELECT name, name FROM users
elchi|elchi
mira|mira
nox|nox
rex|rex

> INSERT INTO users (id, name) VALUES (1, 'dupe')
error: duplicate key (1) in table 'users'

> INSERT INTO users (id, name) VALUES (5, 'ok'), (5, 'dupe')
error: duplicate key (5) in table 'users'

# the failed statement above was atomic, id 5 never landed
> SELECT * FROM users WHERE id = 5

> INSERT INTO users (id) VALUES (6)
error: column 'name' is missing and has no default

> INSERT INTO users (id, name, nope) VALUES (7, 'x', 1)
error: table 'users' has no column 'nope'

> INSERT INTO users (id, name, name) VALUES (8, 'x', 'y')
error: column 'name' appears twice in this row

> INSERT INTO users (id, name) VALUES (NULL, 'x')
error: column 'id': null in a column that is not @null

> INSERT INTO users (id, name) VALUES (9, NULL)
error: column 'name': null in a column that is not @null

> INSERT INTO users (id, name, bio) VALUES (10, 'n', NULL)
put 1

> SELECT * FROM users WHERE id = 10
10|n|0|null

> INSERT INTO users (id, name) VALUES (2 + 9, 'computed')
put 1

> SELECT id FROM users WHERE name = 'computed'
11

> INSERT INTO users (id, name) VALUES (name, 'x')
error: unknown column 'name'
