//! What the SQL front end does with SQL it does not support.
//!
//! The promise this file exists to keep is narrow and important: a query
//! this dialect cannot run comes back as an error that names the construct,
//! rather than as a result. The dangerous failure is not a rejection, it is
//! a query that looks accepted and answers a different question than the
//! one asked, and the way that creeps in is a parser that skips a clause it
//! does not recognise.
//!
//! So each case below asserts two things: that the input is refused, and
//! that the message contains the words a reader would search for. The
//! second half is what keeps the errors useful as the dialect grows, since
//! a generic "parse error" passes the first half perfectly well.
//!
//! When one of these constructs is implemented, its case moves out of this
//! file and into the golden suite. A test failing here because something
//! now works is the good kind of failure.

use quanty_ql::parse_sql;

/// `(input, the words the message must contain)`
const REFUSED: &[(&str, &str)] = &[
    // functions and aggregates
    ("select count(*) from users", "functions and aggregates"),
    ("select sum(score) from users", "functions and aggregates"),
    ("select upper(name) from users", "functions and aggregates"),
    (
        "select id, row_number() over () from users",
        "functions and aggregates",
    ),
    // grouping
    ("select id from users group by id", "group by"),
    ("select id from users having id > 1", "group by"),
    ("select distinct name from users", "select distinct"),
    // set operations
    (
        "select id from users union select id from users",
        "compound selects",
    ),
    (
        "select id from users union all select id from users",
        "compound selects",
    ),
    (
        "select id from users intersect select id from users",
        "compound selects",
    ),
    (
        "select id from users except select id from users",
        "compound selects",
    ),
    // subqueries, in each of the places they turn up
    (
        "with recent as (select id from users) select id from recent",
        "common table expressions",
    ),
    (
        "select id from (select id from users)",
        "subqueries and parentheses in from",
    ),
    (
        "select id from users where id in (select id from users)",
        "in (...)",
    ),
    (
        "select id from users where exists (select 1 from users)",
        "exists",
    ),
    (
        "select (select id from users) from users",
        "expressions are not supported yet",
    ),
    // expressions we do not have
    (
        "select case when id = 1 then 'a' else 'b' end from users",
        "case expressions",
    ),
    (
        "select id from users where name like 'a%'",
        "like and pattern matching",
    ),
    ("select id from users where id between 1 and 5", "between"),
    // joins beyond what the planner does
    (
        "select id from users cross join orders on 1 = 1",
        "cross and natural joins",
    ),
    (
        "select id from users right join orders on 1 = 1",
        "right and full outer joins",
    ),
    (
        "select id from users full outer join orders on 1 = 1",
        "right and full outer joins",
    ),
    (
        "select id from users natural join orders",
        "cross and natural joins",
    ),
    // naming
    ("select name as n from users", "column aliases"),
    ("select u.id from users u", "table aliases"),
    ("select id from main.users", "database-qualified names"),
    // statements
    ("insert into users values (1, 'a')", "explicit column list"),
    (
        "insert into users (id) values (1) on conflict do nothing",
        "on conflict",
    ),
    ("alter table users add column x int", "alter table"),
    // clauses
    ("select id from users limit 1 offset 2", "offset"),
    (
        "select id from users order by id nulls last",
        "nulls first / nulls last",
    ),
];

#[test]
fn unsupported_sql_is_refused_by_name() {
    for (input, expected) in REFUSED {
        match parse_sql(input) {
            Ok(ast) => {
                panic!("this dialect does not support it, but it parsed:\n  {input}\n  as {ast:?}")
            }
            Err(e) => {
                let message = e.to_string();
                assert!(
                    message.contains(expected),
                    "the message should name the construct\n  input:    {input}\n  \
                     expected: something containing {expected:?}\n  got:      {message}"
                );
            }
        }
    }
}

#[test]
fn every_refusal_says_where_it_is() {
    // a message without a position is a message the reader has to hunt
    // with, and these inputs are longer than they look once generated
    for (input, _) in REFUSED {
        let message = parse_sql(input).unwrap_err().to_string();
        assert!(
            message.contains("at byte") || message.contains("byte 0"),
            "no position in: {message}"
        );
    }
}

#[test]
fn the_supported_subset_still_parses() {
    // the other half of the promise: this file must not be able to pass by
    // the front end refusing everything
    for input in [
        "select * from users",
        "select id, name from users",
        "select id from users where id = 1",
        "select id from users where id > 1 and name = 'a'",
        "select id from users order by id desc limit 10",
        "select id from users join orders on users.id = orders.user_id",
        "select id from users left join orders on users.id = orders.user_id",
        "insert into users (id, name) values (1, 'a')",
        "update users set name = 'b' where id = 1",
        "delete from users where id = 1",
        "create index ix_name on users (name)",
        "begin",
        "commit",
        "rollback",
    ] {
        parse_sql(input).unwrap_or_else(|e| panic!("this should parse: {input}\n  {e}"));
    }
}
