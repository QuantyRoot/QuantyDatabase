# filter semantics, including the null rules; the sql mirror of 03_where.qql

> CREATE TABLE n (id INTEGER PRIMARY KEY, v INTEGER, t TEXT NOT NULL DEFAULT 'x')
ok

> INSERT INTO n (id, v) VALUES (1, 10), (2, 20), (3, NULL)
put 3

> INSERT INTO n (id, v, t) VALUES (4, 30, 'y')
put 1

> SELECT * FROM n WHERE v > 15
2|20|x
4|30|y

> SELECT * FROM n WHERE v >= 20 AND t = 'x'
2|20|x

> SELECT * FROM n WHERE v < 15 OR v > 25
1|10|x
4|30|y

# null = 10 is false, so not(...) is true: null rows pass a negated equality
> SELECT * FROM n WHERE NOT v = 10
2|20|x
3|null|x
4|30|y

> SELECT * FROM n WHERE v IS NULL
3|null|x

> SELECT * FROM n WHERE v IS NOT NULL
1|10|x
2|20|x
4|30|y

# null arithmetic yields null, a null condition is false
> SELECT * FROM n WHERE v + 5 > 20
2|20|x
4|30|y

> SELECT * FROM n WHERE id % 2 = 0
2|20|x
4|30|y

> SELECT * FROM n WHERE (v < 15 OR v > 25) AND t = 'y'
4|30|y

> SELECT * FROM n WHERE t = 'y' OR t = 'x' AND v = 10
1|10|x
4|30|y

> SELECT * FROM n WHERE v > 'a'
error: cannot compare a int to a text

> SELECT * FROM n WHERE t
error: the condition is a text, not a bool

> SELECT * FROM n WHERE v + 1
error: the condition is a int, not a bool

> SELECT * FROM n WHERE zzz = 1
error: table 'n' has no column 'zzz'
