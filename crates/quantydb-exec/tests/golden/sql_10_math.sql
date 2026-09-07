# expression arithmetic, checked and documented; mirror of 10_math.qql

> CREATE TABLE m (id INTEGER PRIMARY KEY, v INTEGER NOT NULL DEFAULT 0)
ok

> INSERT INTO m (id, v) VALUES (1, 10), (2, -3)
put 2

> SELECT * FROM m WHERE v * 2 = 20
1|10

> SELECT * FROM m WHERE v + v = -6
2|-3

> SELECT * FROM m WHERE -v = 3
2|-3

> SELECT * FROM m WHERE v - 1 < 0
2|-3

> SELECT * FROM m WHERE v / 3 = 3
1|10

> SELECT * FROM m WHERE v % 3 = 1
1|10

> SELECT * FROM m WHERE (v + 2) * 3 = 36
1|10

> UPDATE m SET v = 9223372036854775807 WHERE id = 1
set 1

> UPDATE m SET v = v + 1 WHERE id = 1
error: integer overflow

> SELECT * FROM m WHERE id = 1
1|9223372036854775807

> UPDATE m SET v = -v - 1 WHERE id = 1
set 1

> SELECT * FROM m WHERE id = 1
1|-9223372036854775808

> UPDATE m SET v = -v WHERE id = 1
error: integer overflow

> SELECT * FROM m WHERE v / 0 = 1
error: division by zero

> SELECT * FROM m WHERE v % 0 = 1
error: division by zero

> SELECT * FROM m WHERE NOT (id = 1) AND NOT NOT (id = 2)
2|-3

> SELECT * FROM m WHERE true
1|-9223372036854775808
2|-3

> SELECT * FROM m WHERE false

> SELECT * FROM m WHERE null
