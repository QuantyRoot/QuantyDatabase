//! `#[derive(Row)]`, exercised through the embedded surface.

use quanty::{Database, ErrorKind, Row};

#[derive(Row, Debug, Clone, PartialEq)]
#[quanty(table = "users")]
struct User {
    id: i64,
    name: String,
    score: i32,
}

fn seeded() -> Database {
    let mut db = Database::in_memory().expect("open in memory");
    db.execute("table users { id: int @key, name: text @index, score: int = 0 }")
        .expect("define table");
    db
}

#[test]
fn a_struct_goes_in_and_comes_back_out() {
    let mut db = seeded();
    let ada = User {
        id: 1,
        name: "ada".to_string(),
        score: 7,
    };
    db.insert(&ada).expect("insert");

    let back: Vec<User> = db.query_as("get users { id, name, score }").expect("query");
    assert_eq!(back, vec![ada]);
}

#[test]
fn the_column_order_of_the_query_does_not_matter() {
    let mut db = seeded();
    db.insert(&User {
        id: 1,
        name: "ada".to_string(),
        score: 7,
    })
    .expect("insert");

    // Reversed against the struct's field order on purpose. Proved to
    // catch: making Rows::into_typed push positions in field order
    // instead of resolving each name fails this test and nothing else.
    let back: Vec<User> = db.query_as("get users { score, name, id }").expect("query");
    assert_eq!(back[0].id, 1);
    assert_eq!(back[0].name, "ada");
    assert_eq!(back[0].score, 7);
}

#[test]
fn insert_all_writes_every_row() {
    let mut db = seeded();
    let rows: Vec<User> = (1..=5)
        .map(|i| User {
            id: i,
            name: format!("u{i}"),
            score: i as i32 * 10,
        })
        .collect();

    assert_eq!(db.insert_all(&rows).expect("insert_all"), 5);
    let mut back: Vec<User> = db.query_as("get users { id, name, score }").expect("query");
    back.sort_by_key(|u| u.id);
    assert_eq!(back, rows);
}

#[test]
fn an_empty_insert_writes_nothing_and_says_so() {
    let mut db = seeded();
    let before = db.head();
    assert_eq!(db.insert_all::<User>(&[]).expect("empty"), 0);
    assert_eq!(db.head(), before, "an empty insert burned a commit");
}

#[test]
fn the_table_name_defaults_to_the_struct_name_in_snake_case() {
    #[derive(Row, Debug, Clone, PartialEq)]
    struct UserAccount {
        id: i64,
    }

    assert_eq!(UserAccount::TABLE, "user_account");
    assert_eq!(UserAccount::COLUMNS, ["id"]);

    let mut db = Database::in_memory().expect("open");
    db.execute("table user_account { id: int @key }")
        .expect("table");
    db.insert(&UserAccount { id: 3 }).expect("insert");
    let back: Vec<UserAccount> = db.query_as("get user_account { id }").expect("query");
    assert_eq!(back, vec![UserAccount { id: 3 }]);
}

#[test]
fn a_field_can_name_a_different_column() {
    #[derive(Row, Debug, PartialEq)]
    #[quanty(table = "users")]
    struct Named {
        id: i64,
        #[quanty(column = "name")]
        who: String,
    }

    assert_eq!(Named::COLUMNS, ["id", "name"]);

    let mut db = seeded();
    db.insert(&Named {
        id: 1,
        who: "ada".to_string(),
    })
    .expect("insert");

    let back: Vec<Named> = db.query_as("get users { id, name }").expect("query");
    assert_eq!(back[0].who, "ada");
}

#[test]
fn an_optional_field_takes_a_null() {
    #[derive(Row, Debug, PartialEq)]
    #[quanty(table = "notes")]
    struct Note {
        id: i64,
        body: Option<String>,
    }

    let mut db = Database::in_memory().expect("open");
    db.execute("table notes { id: int @key, body: text @null }")
        .expect("table");

    db.insert(&Note { id: 1, body: None }).expect("null");
    db.insert(&Note {
        id: 2,
        body: Some("hi".to_string()),
    })
    .expect("some");

    let mut back: Vec<Note> = db.query_as("get notes { id, body }").expect("query");
    back.sort_by_key(|n| n.id);
    assert_eq!(back[0].body, None);
    assert_eq!(back[1].body.as_deref(), Some("hi"));
}

#[test]
fn a_missing_column_names_the_column_and_builds_no_rows() {
    let mut db = seeded();
    db.insert(&User {
        id: 1,
        name: "ada".to_string(),
        score: 7,
    })
    .expect("insert");

    let err = db
        .query_as::<User>("get users { id, name }")
        .expect_err("score is missing");
    assert_eq!(err.kind(), ErrorKind::Exec);
    assert!(err.to_string().contains("score"), "unhelpful: {err}");
}

#[test]
fn a_wrong_type_names_the_field_it_arrived_at() {
    #[derive(Row, Debug)]
    #[quanty(table = "users")]
    struct Wrong {
        id: String,
    }

    let mut db = seeded();
    db.insert(&User {
        id: 1,
        name: "ada".to_string(),
        score: 7,
    })
    .expect("insert");

    let err = db
        .query_as::<Wrong>("get users { id }")
        .expect_err("id is an int");
    let message = err.to_string();
    assert!(message.contains("Wrong.id"), "does not name it: {message}");
    assert!(message.contains("text"), "does not say expected: {message}");
    assert!(message.contains("int"), "does not say got: {message}");
}

#[test]
fn a_narrow_int_fails_instead_of_wrapping() {
    #[derive(Row, Debug)]
    #[quanty(table = "big")]
    struct Narrow {
        n: i32,
    }

    let mut db = Database::in_memory().expect("open");
    db.execute("table big { n: int @key }").expect("table");
    db.execute(&format!("put big {{ n: {} }}", i64::from(i32::MAX) + 1))
        .expect("put");

    let err = db.query_as::<Narrow>("get big { n }").expect_err("too big");
    assert_eq!(err.kind(), ErrorKind::Exec);
    assert!(err.to_string().contains("does not fit"), "unhelpful: {err}");
}

#[test]
fn a_derived_insert_cannot_be_talked_into_running_a_statement() {
    // The value travels as a parsed literal, so a name that looks like
    // QQL is stored as a name and nothing else happens (ADR-031).
    let mut db = seeded();
    let hostile = User {
        id: 1,
        name: "\" } del users where id > 0; put users { id: 99, name: \"".to_string(),
        score: 0,
    };
    db.insert(&hostile).expect("insert");

    let back: Vec<User> = db.query_as("get users { id, name, score }").expect("query");
    assert_eq!(back.len(), 1, "the value was executed, not stored");
    assert_eq!(back[0].name, hostile.name);
    assert_eq!(back[0].id, 1);
}

#[test]
fn a_derived_row_works_inside_a_transaction() {
    let mut db = seeded();
    let outcome: Result<(), _> = db.transaction(|tx| {
        tx.insert(&User {
            id: 1,
            name: "ada".to_string(),
            score: 1,
        })?;
        tx.insert(&User {
            id: 1,
            name: "clash".to_string(),
            score: 2,
        })?;
        Ok(())
    });

    assert!(outcome.is_err(), "a duplicate key should fail");
    assert!(!db.in_transaction());
    let back: Vec<User> = db.query_as("get users { id, name, score }").expect("query");
    assert!(back.is_empty(), "the rolled back row survived");
}
