# secondary indexes: declared, created late, kept in sync; mirror of 06_index.qql

> CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score INTEGER NOT NULL DEFAULT 0)
ok

> CREATE INDEX idx_users_name ON users (name)
ok

> INSERT INTO users (id, name) VALUES (1, 'elchi'), (2, 'mira')
put 2

> INSERT INTO users (id, name, score) VALUES (3, 'elchi', 5)
put 1

> SELECT * FROM users WHERE name = 'elchi'
1|elchi|0
3|elchi|5

> SELECT * FROM users WHERE name = 'elchi' AND score > 0
3|elchi|5

> SELECT * FROM users WHERE name = 'nobody'

> UPDATE users SET name = 'renamed' WHERE id = 1
set 1

> SELECT * FROM users WHERE name = 'elchi'
3|elchi|5

> SELECT * FROM users WHERE name = 'renamed'
1|renamed|0

> DELETE FROM users WHERE name = 'elchi'
del 1

> SELECT * FROM users
1|renamed|0
2|mira|0

> CREATE TABLE late (id INTEGER PRIMARY KEY, tag TEXT NOT NULL)
ok

> INSERT INTO late (id, tag) VALUES (1, 'x'), (2, 'y'), (3, 'x')
put 3

> CREATE INDEX idx_late_tag ON late (tag)
ok

> SELECT * FROM late WHERE tag = 'x'
1|x
3|x

> CREATE INDEX idx_late_tag_again ON late (tag)
error: 'late.tag' is already indexed

> CREATE INDEX idx_late_nope ON late (nope)
error: table 'late' has no column 'nope'

> CREATE INDEX idx_nope ON nope (c)
error: no table named 'nope'

> CREATE TABLE ni (id INTEGER PRIMARY KEY, v INTEGER)
ok

> CREATE INDEX idx_ni_v ON ni (v)
ok

> INSERT INTO ni (id, v) VALUES (1, 5), (3, NULL)
put 2

> INSERT INTO ni (id) VALUES (2)
put 1

> SELECT * FROM ni WHERE v IS NULL
2|null
3|null

> SELECT * FROM ni WHERE v = 5
1|5

> UPDATE ni SET v = NULL WHERE id = 1
set 1

> SELECT * FROM ni WHERE v IS NULL
1|null
2|null
3|null

> SELECT * FROM ni WHERE v = 5
