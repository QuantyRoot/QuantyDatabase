# the planner must pick key lookups and index scans, and explain must say so
# mirror of 07_explain.qql

> CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score INTEGER NOT NULL DEFAULT 0)
ok

> CREATE INDEX idx_users_name ON users (name)
ok

> EXPLAIN SELECT * FROM users
SeqScan users

> EXPLAIN SELECT * FROM users WHERE score > 100 ORDER BY score DESC LIMIT 10
Limit 10
  Sort score desc
    Filter (score > 100)
      SeqScan users

> EXPLAIN SELECT * FROM users WHERE id = 7
KeyLookup users (id = 7)

> EXPLAIN SELECT * FROM users WHERE id = 7 AND score > 1
Filter (score > 1)
  KeyLookup users (id = 7)

> EXPLAIN SELECT * FROM users WHERE name = 'elchi'
IndexScan users via name = "elchi"

> EXPLAIN SELECT * FROM users WHERE name = 'elchi' AND score > 3
Filter (score > 3)
  IndexScan users via name = "elchi"

> EXPLAIN SELECT * FROM users WHERE score > 3
Filter (score > 3)
  SeqScan users

> EXPLAIN SELECT * FROM users WHERE name = 'x' AND score > 1 AND id > 0
Filter ((score > 1) and (id > 0))
  IndexScan users via name = "x"

> EXPLAIN SELECT * FROM users WHERE id = 1 AND id = 2
Filter (id = 2)
  KeyLookup users (id = 1)

> EXPLAIN UPDATE users SET score = score + 5 WHERE id = 1
Update users
  KeyLookup users (id = 1)

> EXPLAIN DELETE FROM users WHERE name = 'x'
Delete users
  IndexScan users via name = "x"

> EXPLAIN DELETE FROM users
Delete users
  SeqScan users

> EXPLAIN INSERT INTO users (id, name) VALUES (1, 'a'), (2, 'b')
Insert users (2 rows)

> EXPLAIN show tables
error: explain wants get, put, set or del

> EXPLAIN EXPLAIN SELECT * FROM users
error: explain wants get, put, set or del

> CREATE TABLE pairs (a INTEGER NOT NULL, b INTEGER NOT NULL, v INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (a, b))
ok

> EXPLAIN QUERY PLAN SELECT * FROM pairs WHERE a = 1 AND b = 2
KeyLookup pairs (a = 1, b = 2)

> EXPLAIN SELECT * FROM pairs WHERE a = 1
Filter (a = 1)
  SeqScan pairs

> EXPLAIN SELECT * FROM pairs WHERE b = 2 AND a = 1 AND v > 0
Filter (v > 0)
  KeyLookup pairs (a = 1, b = 2)

> EXPLAIN SELECT * FROM users WHERE name IS NULL
IndexScan users via name = null
