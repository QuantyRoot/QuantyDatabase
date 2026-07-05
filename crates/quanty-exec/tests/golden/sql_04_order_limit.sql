# sorting and limiting; nulls sort first ascending; mirror of 04_order_limit.qql

> CREATE TABLE s (id INTEGER PRIMARY KEY, score INTEGER, name TEXT NOT NULL)
ok

> INSERT INTO s (id, score, name) VALUES (1, 50, 'c'), (2, 10, 'a'), (3, NULL, 'b'), (4, 50, 'd')
put 4

> SELECT * FROM s ORDER BY score
3|null|b
2|10|a
1|50|c
4|50|d

> SELECT * FROM s ORDER BY score DESC
1|50|c
4|50|d
2|10|a
3|null|b

> SELECT * FROM s ORDER BY score ASC
3|null|b
2|10|a
1|50|c
4|50|d

> SELECT * FROM s ORDER BY name DESC
4|50|d
1|50|c
3|null|b
2|10|a

> SELECT * FROM s ORDER BY score LIMIT 2
3|null|b
2|10|a

> SELECT * FROM s LIMIT 0

> SELECT * FROM s LIMIT 2
1|50|c
2|10|a

> SELECT * FROM s LIMIT 100
1|50|c
2|10|a
3|null|b
4|50|d

> SELECT name FROM s ORDER BY score DESC LIMIT 3
c
d
a

> SELECT * FROM s ORDER BY zzz
error: cannot order by unknown column 'zzz'
