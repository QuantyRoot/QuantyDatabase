# joins: probe strategies, null padding, qualified names, chained joins
# mirror of 15_joins.qql; same engine, so the rows must match exactly

> CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, city INTEGER)
ok
> CREATE TABLE cities (id INTEGER PRIMARY KEY, name TEXT NOT NULL, code INTEGER, rank INTEGER)
ok
> CREATE INDEX idx_cities_code ON cities (code)
ok
> INSERT INTO users (id, name, city) VALUES (1, 'ada', 10), (2, 'bob', 20), (3, 'cy', 99), (4, 'di', NULL)
put 4
> INSERT INTO cities (id, name, code, rank) VALUES (10, 'oslo', 100, 1), (20, 'bonn', 200, 2), (30, 'riga', 300, 3)
put 3

# inner join spelled plain, and again spelled INNER JOIN: identical
> SELECT * FROM users JOIN cities ON users.city = cities.id
1|ada|10|10|oslo|100|1
2|bob|20|20|bonn|200|2
> SELECT * FROM users INNER JOIN cities ON users.city = cities.id
1|ada|10|10|oslo|100|1
2|bob|20|20|bonn|200|2

# left join, and the LEFT OUTER JOIN spelling: identical
> SELECT * FROM users LEFT JOIN cities ON users.city = cities.id
1|ada|10|10|oslo|100|1
2|bob|20|20|bonn|200|2
3|cy|99|null|null|null|null
4|di|null|null|null|null|null
> SELECT * FROM users LEFT OUTER JOIN cities ON users.city = cities.id
1|ada|10|10|oslo|100|1
2|bob|20|20|bonn|200|2
3|cy|99|null|null|null|null
4|di|null|null|null|null|null

# qualified projection across same-named columns
> SELECT users.name, cities.name FROM users JOIN cities ON users.city = cities.id
ada|oslo
bob|bonn

# unqualified name unique across the scope
> SELECT city, code FROM users JOIN cities ON users.city = cities.id
10|100
20|200

# a second conjunct in on
> SELECT * FROM users JOIN cities ON users.city = cities.id AND cities.rank = 1
1|ada|10|10|oslo|100|1

# where runs after the join
> SELECT * FROM users JOIN cities ON users.city = cities.id WHERE cities.rank > 1
2|bob|20|20|bonn|200|2

# where reaching padded rows: sqlite spells it IS NULL
> SELECT * FROM users LEFT JOIN cities ON users.city = cities.id WHERE cities.id IS NULL
3|cy|99|null|null|null|null
4|di|null|null|null|null|null

# order by a right column
> SELECT * FROM users JOIN cities ON users.city = cities.id ORDER BY cities.name DESC
1|ada|10|10|oslo|100|1
2|bob|20|20|bonn|200|2

# order and limit over a left join
> SELECT * FROM users LEFT JOIN cities ON users.city = cities.id ORDER BY cities.rank ASC LIMIT 2
3|cy|99|null|null|null|null
4|di|null|null|null|null|null

# key probe
> EXPLAIN SELECT * FROM users JOIN cities ON users.city = cities.id
IndexNestedLoopJoin inner on (users.city = cities.id)
  SeqScan users
  KeyProbe cities (id)

# index probe
> EXPLAIN SELECT * FROM users JOIN cities ON users.city = cities.code
IndexNestedLoopJoin inner on (users.city = cities.code)
  SeqScan users
  IndexProbe cities via code

# nested loop
> EXPLAIN SELECT * FROM users JOIN cities ON users.city = cities.rank
NestedLoopJoin inner on (users.city = cities.rank)
  SeqScan users
  SeqScan cities

# left variant, same probe
> EXPLAIN SELECT * FROM users LEFT JOIN cities ON users.city = cities.id
IndexNestedLoopJoin left on (users.city = cities.id)
  SeqScan users
  KeyProbe cities (id)

# residual, filter, sort and limit stacked
> EXPLAIN SELECT * FROM users JOIN cities ON users.city = cities.id AND cities.rank > 0 WHERE users.id > 1 ORDER BY cities.name ASC LIMIT 5
Limit 5
  Sort cities.name asc
    Filter (users.id > 1)
      IndexNestedLoopJoin inner on ((users.city = cities.id) and (cities.rank > 0))
        SeqScan users
        KeyProbe cities (id)

# ambiguous unqualified name
> SELECT name FROM users JOIN cities ON users.city = cities.id
error: column 'name' is ambiguous here; qualify it (users.name or cities.name)

# ambiguous in a filter
> SELECT * FROM users JOIN cities ON users.city = cities.id WHERE id = 10
error: column 'id' is ambiguous here; qualify it (users.id or cities.id)

# qualifier naming no table in scope
> SELECT orders.name FROM users JOIN cities ON users.city = cities.id
error: no table named 'orders' in this statement

# column that exists nowhere
> SELECT zzz FROM users JOIN cities ON users.city = cities.id
error: no table in this statement has a column 'zzz'

# self join needs aliases
> SELECT * FROM users JOIN users ON users.id = users.id
error: table 'users' appears twice in this statement; table aliases are not supported yet

# on-condition naming a table joined later
> SELECT * FROM users JOIN cities ON regions.area = cities.name
error: no table named 'regions' in this statement

# three-table chain
> CREATE TABLE regions (code INTEGER PRIMARY KEY, area TEXT NOT NULL)
ok
> INSERT INTO regions (code, area) VALUES (100, 'north'), (200, 'south')
put 2
> SELECT users.name, regions.area FROM users JOIN cities ON users.city = cities.id JOIN regions ON cities.code = regions.code
ada|north
bob|south
> EXPLAIN SELECT * FROM users JOIN cities ON users.city = cities.id JOIN regions ON cities.code = regions.code
IndexNestedLoopJoin inner on (cities.code = regions.code)
  IndexNestedLoopJoin inner on (users.city = cities.id)
    SeqScan users
    KeyProbe cities (id)
  KeyProbe regions (code)
