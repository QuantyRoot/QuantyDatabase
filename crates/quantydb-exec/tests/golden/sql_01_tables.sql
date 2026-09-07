# table lifecycle and DDL validation; the sql mirror of 01_tables.qql

> show tables

> CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score INTEGER NOT NULL DEFAULT 0)
ok

> CREATE INDEX idx_users_name ON users (name)
ok

> show tables
users

> CREATE TABLE users (id INTEGER PRIMARY KEY)
error: table 'users' already exists

> CREATE TABLE empty_pk (a INTEGER)
error: parse error at byte 33: table 'empty_pk' needs a primary key

> CREATE TABLE two_pk (a INTEGER PRIMARY KEY, b INTEGER PRIMARY KEY)
error: parse error at byte 65: the table already has a primary key; use one table-level primary key (a, b) for a composite key

> CREATE TABLE dup (a INTEGER PRIMARY KEY, a TEXT)
error: column 'a' is defined twice in table 'dup'

> CREATE TABLE baddef (a INTEGER PRIMARY KEY, b INTEGER NOT NULL DEFAULT 'x')
error: default for column 'b': a text value does not fit into a column of type int

> CREATE TABLE t2 (a INTEGER PRIMARY KEY)
ok

> show tables
t2
users

> DROP TABLE t2
ok

> show tables
users

> DROP TABLE t2
error: no table named 't2'

> CREATE TABLE composite (a INTEGER NOT NULL, b TEXT NOT NULL, v INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (a, b))
ok

> INSERT INTO composite (a, b) VALUES (1, 'x'), (1, 'y')
put 2

> INSERT INTO composite (a, b) VALUES (1, 'x')
error: duplicate key (1, "x") in table 'composite'

> SELECT * FROM composite
1|x|0
1|y|0

> DROP TABLE users
ok

> show tables
composite
