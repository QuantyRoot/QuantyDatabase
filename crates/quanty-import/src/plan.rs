//! Deciding what a SQLite database becomes, before anything is written.
//!
//! This is the first of the import's two passes (ADR-019). It reads the
//! whole source, decides every column's type, picks every table's key, and
//! writes nothing. What comes out is a plan and a report: the plan is what
//! the second pass executes, the report is what the developer reads.
//!
//! The point of separating them is that a developer should learn about all
//! the problems at once, a minute in, rather than about the first one after
//! ten minutes of writing. It also means the target schema is settled
//! before a single row is inserted, so nothing has to be reshaped halfway.
//!
//! Nothing here refuses a database it could import. Where a choice has to
//! be made, it is made, and it goes in the report; `--strict` turns the
//! choices back into refusals for whoever prefers that.

use quanty_core::Value;
use quanty_ql::ast::TypeName;
use quanty_sqlite::{ColumnSurvey, Reader, SchemaObject, Source, StorageClass, TableSurvey};

use crate::default;
use crate::name::Names;

/// Beyond this an integer is no longer exactly representable as a float, so
/// widening a mixed integer and real column would change the value.
const LARGEST_EXACT_INTEGER_IN_A_FLOAT: u64 = 1 << 53;

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Refuse anything that would need a judgement call instead of making
    /// one. Every note that says a column was widened becomes a problem.
    pub strict: bool,
}

/// Where a target column's value comes from in a source row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueSource {
    /// The declared column at this position, read through the row layout.
    Declared(usize),
    /// The row's rowid, for a key column we added ourselves.
    Rowid,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnPlan {
    pub source_name: String,
    pub name: String,
    pub ty: TypeName,
    pub nullable: bool,
    pub key: bool,
    pub indexed: bool,
    pub default: Option<Value>,
    pub source: ValueSource,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TablePlan {
    pub source_name: String,
    pub name: String,
    pub root_page: u32,
    pub rows: u64,
    pub columns: Vec<ColumnPlan>,
}

impl TablePlan {
    pub fn key_columns(&self) -> impl Iterator<Item = &ColumnPlan> {
        self.columns.iter().filter(|c| c.key)
    }
}

/// Something the import decided, which the developer should know about but
/// which does not stop anything.
#[derive(Debug, Clone, PartialEq)]
pub enum Note {
    Renamed {
        what: String,
        from: String,
        to: String,
    },
    Widened {
        column: String,
        classes: Vec<StorageClass>,
        to: TypeName,
    },
    /// A column with no values at all, whose type came from its declaration.
    TypeFromDeclarationOnly {
        column: String,
        declared: Option<String>,
        ty: TypeName,
    },
    /// The table had no key we could use, so the rowid became one.
    AddedRowidKey {
        table: String,
        column: String,
        reason: String,
    },
    /// A default we could not read, so the column is nullable instead.
    DefaultNotUnderstood {
        column: String,
        text: String,
    },
    Skipped {
        what: String,
        reason: String,
    },
    /// A rule the source enforced that we have no equivalent for.
    ///
    /// This is not a `--strict` matter. That switch refuses judgement
    /// calls, where a different answer was available; there is no other
    /// answer here, because our language has nowhere to put a foreign key.
    /// Refusing would leave the developer with no import and no foreign key
    /// either, so the import happens and the note says what is no longer
    /// being enforced.
    ConstraintNotCarried {
        table: String,
        what: String,
        consequence: String,
    },
}

/// Something that stops the import.
#[derive(Debug, Clone, PartialEq)]
pub enum Problem {
    /// Mixing integers and reals means widening to float, and this integer
    /// would not survive that.
    IntegerTooLargeForFloat {
        column: String,
        largest: u64,
    },
    Unsupported {
        what: String,
        reason: String,
    },
    /// Raised only under `--strict`, where a judgement call is refused.
    WouldWiden {
        column: String,
        classes: Vec<StorageClass>,
        to: TypeName,
    },
}

#[derive(Debug, Clone, Default)]
pub struct ImportPlan {
    pub tables: Vec<TablePlan>,
    pub notes: Vec<Note>,
    pub problems: Vec<Problem>,
}

impl ImportPlan {
    pub fn is_runnable(&self) -> bool {
        self.problems.is_empty()
    }

    pub fn rows(&self) -> u64 {
        self.tables.iter().map(|t| t.rows).sum()
    }

    /// What the developer reads after running the command.
    pub fn report(&self) -> String {
        let mut out = String::new();
        for table in &self.tables {
            out.push_str(&format!(
                "{} -> {} ({} rows)\n",
                table.source_name, table.name, table.rows
            ));
            for column in &table.columns {
                let mut flags = Vec::new();
                if column.key {
                    flags.push("@key".to_string());
                }
                if column.indexed {
                    flags.push("@index".to_string());
                }
                if column.nullable {
                    flags.push("@null".to_string());
                }
                if let Some(value) = &column.default {
                    flags.push(format!("= {}", render(value)));
                }
                out.push_str(&format!(
                    "  {:24} {:6} {}\n",
                    column.name,
                    type_name(column.ty),
                    flags.join(" ")
                ));
            }
        }
        if !self.notes.is_empty() {
            out.push_str("\nnotes:\n");
            for note in &self.notes {
                out.push_str(&format!("  - {}\n", describe_note(note)));
            }
        }
        if !self.problems.is_empty() {
            out.push_str("\nproblems, nothing was written:\n");
            for problem in &self.problems {
                out.push_str(&format!("  - {}\n", describe_problem(problem)));
            }
        }
        out
    }
}

pub fn type_name(ty: TypeName) -> &'static str {
    match ty {
        TypeName::Int => "int",
        TypeName::Float => "float",
        TypeName::Text => "text",
        TypeName::Bytes => "bytes",
        TypeName::Bool => "bool",
    }
}

fn render(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => format!("{f:?}"),
        Value::Text(t) => format!("{t:?}"),
        Value::Bytes(b) => format!(
            "x\"{}\"",
            b.iter().map(|x| format!("{x:02x}")).collect::<String>()
        ),
    }
}

fn describe_note(note: &Note) -> String {
    match note {
        Note::Renamed { what, from, to } => {
            format!("{what} {from:?} is not a name our language allows, imported as {to}")
        }

        Note::Widened {
            column,
            classes,
            to,
        } => format!(
            "{column} holds {}, imported as {}; comparisons follow the new type",
            classes
                .iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
                .join(" and "),
            type_name(*to)
        ),
        Note::TypeFromDeclarationOnly {
            column,
            declared,
            ty,
        } => format!(
            "{column} holds no values at all, typed {} from its declaration {}",
            type_name(*ty),
            declared.as_deref().unwrap_or("(none)")
        ),
        Note::AddedRowidKey {
            table,
            column,
            reason,
        } => format!("{table} has {reason}, so its rowid was added as the column {column}"),
        Note::DefaultNotUnderstood { column, text } => format!(
            "{column} defaults to {text:?}, which is an expression rather than a value, so the \
             column is nullable instead"
        ),
        Note::Skipped { what, reason } => format!("{what} was skipped: {reason}"),
        Note::ConstraintNotCarried {
            table,
            what,
            consequence,
        } => format!("{table} {what}, which we cannot enforce: {consequence}"),
    }
}

fn describe_problem(problem: &Problem) -> String {
    match problem {
        Problem::IntegerTooLargeForFloat { column, largest } => format!(
            "{column} holds both integers and reals, which would widen it to float, but {largest} \
             is too large to survive that exactly"
        ),
        Problem::Unsupported { what, reason } => format!("{what}: {reason}"),
        Problem::WouldWiden {
            column,
            classes,
            to,
        } => format!(
            "{column} holds {} and would be imported as {}, refused because of --strict",
            classes
                .iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
                .join(" and "),
            type_name(*to)
        ),
    }
}

/// Read the whole source database and decide what it becomes.
pub fn plan<S: Source>(reader: &Reader<S>, options: &Options) -> quanty_sqlite::Result<ImportPlan> {
    let schema = reader.schema()?;
    let mut plan = ImportPlan::default();
    let mut table_names = Names::new();

    // indexes are not objects to import but attributes of the columns they
    // cover, so they are collected once here rather than looked up per table
    let indexes = collect_indexes(&schema, &mut plan);

    for object in schema.objects() {
        if object.is_internal() {
            plan.notes.push(Note::Skipped {
                what: object.name.clone(),
                reason: "it is sqlite's own bookkeeping, not your data".to_string(),
            });
            continue;
        }
        match object.kind {
            quanty_sqlite::ObjectKind::Table => plan_table(
                reader,
                object,
                options,
                &indexes,
                &mut table_names,
                &mut plan,
            )?,
            // an index is already accounted for above
            quanty_sqlite::ObjectKind::Index => {}
            kind => plan.notes.push(Note::Skipped {
                what: format!("the {} {}", kind.as_str(), object.name),
                reason: "it holds no rows of its own".to_string(),
            }),
        }
    }

    Ok(plan)
}

fn plan_table<S: Source>(
    reader: &Reader<S>,
    object: &SchemaObject,
    options: &Options,
    indexes: &[(String, String)],
    table_names: &mut Names,
    plan: &mut ImportPlan,
) -> quanty_sqlite::Result<()> {
    let def = match object.table_def() {
        Ok(def) => def,
        Err(e) => {
            plan.problems.push(Problem::Unsupported {
                what: object.name.clone(),
                reason: e.to_string(),
            });
            return Ok(());
        }
    };
    note_constraints(&def, &object.name, plan);

    let survey = reader.survey_table(object)?;
    let (table_name, renamed) = table_names.assign(&object.name);
    if renamed {
        plan.notes.push(Note::Renamed {
            what: "the table".to_string(),
            from: object.name.clone(),
            to: table_name.clone(),
        });
    }

    // which declared columns can carry the key, and which have to be added
    let key_choice = choose_key(&def, &survey);

    // falling back to the rowid is only an option where there is one. a
    // without rowid table that cannot use its own primary key has nothing
    // left to be keyed by, and saying that here is better than discovering
    // it row by row in the writing pass.
    if def.without_rowid {
        if let KeyChoice::AddRowid { reason } = &key_choice {
            plan.problems.push(Problem::Unsupported {
                what: object.name.clone(),
                reason: format!(
                    "it is a without rowid table with {reason}, so there is no key left to \
                     give it"
                ),
            });
            return Ok(());
        }
    }

    let mut column_names = Names::new();
    let mut columns = Vec::new();

    if let KeyChoice::AddRowid { reason } = &key_choice {
        let (name, _) = column_names.assign("rowid");
        plan.notes.push(Note::AddedRowidKey {
            table: table_name.clone(),
            column: name.clone(),
            reason: reason.clone(),
        });
        columns.push(ColumnPlan {
            source_name: "rowid".to_string(),
            name,
            ty: TypeName::Int,
            nullable: false,
            key: true,
            indexed: false,
            default: None,
            source: ValueSource::Rowid,
        });
    }

    let indexed: Vec<&str> = indexes
        .iter()
        .filter(|(table, _)| table.eq_ignore_ascii_case(&object.name))
        .map(|(_, column)| column.as_str())
        .collect();

    for (index, column) in def.columns.iter().enumerate() {
        let survey_column = &survey.columns[index];
        if survey_column.is_virtual {
            plan.notes.push(Note::Skipped {
                what: format!("{}.{}", table_name, column.name),
                reason: "it is a virtual generated column, so the file holds nothing for it"
                    .to_string(),
            });
            continue;
        }

        let (name, renamed) = column_names.assign(&column.name);
        if renamed {
            plan.notes.push(Note::Renamed {
                what: format!("the column in {}", object.name),
                from: column.name.clone(),
                to: name.clone(),
            });
        }
        let qualified = format!("{table_name}.{name}");

        let ty = decide_type(survey_column, &qualified, options, plan);
        let is_key = matches!(&key_choice, KeyChoice::Columns(names)
            if names.iter().any(|k| k.eq_ignore_ascii_case(&column.name)));

        // a default that we can read fills the rows that predate the column
        let mut default = None;
        if let Some(text) = &column.default {
            match default::parse(text) {
                Some(Value::Null) => {}
                Some(value) if value_fits(&value, ty) => default = Some(value),
                Some(_) => plan.notes.push(Note::DefaultNotUnderstood {
                    column: qualified.clone(),
                    text: text.clone(),
                }),
                None => plan.notes.push(Note::DefaultNotUnderstood {
                    column: qualified.clone(),
                    text: text.clone(),
                }),
            }
        }

        // missing values are covered by a default if there is one
        let nullable = if is_key {
            false
        } else {
            survey_column.nulls > 0 || (survey_column.missing > 0 && default.is_none())
        };

        // an index is a b-tree too, so a column with a value longer than a
        // key may be stored but not indexed. the index is dropped rather
        // than the table, and the note says so, because a missing index
        // costs speed and a missing table costs data.
        let mut indexed = !is_key && indexed.iter().any(|c| c.eq_ignore_ascii_case(&column.name));
        if indexed && survey_column.longest_value > key_limit() {
            plan.notes.push(Note::Skipped {
                what: format!("the index on {}.{}", object.name, column.name),
                reason: format!(
                    "it holds a value of {} bytes and an index key stops at {}",
                    survey_column.longest_value,
                    key_limit()
                ),
            });
            indexed = false;
        }

        columns.push(ColumnPlan {
            source_name: column.name.clone(),
            name: name.clone(),
            ty,
            nullable,
            key: is_key,
            indexed,
            default,
            source: ValueSource::Declared(index),
        });
    }

    plan.tables.push(TablePlan {
        source_name: object.name.clone(),
        name: table_name,
        root_page: object.root_page.unwrap_or(0),
        rows: survey.rows,
        columns,
    });
    Ok(())
}

/// Say out loud what the source enforced and we will not.
fn note_constraints(def: &quanty_sqlite::TableDef, table: &str, plan: &mut ImportPlan) {
    use quanty_sqlite::Constraint;
    for constraint in &def.unsupported_constraints {
        let (what, consequence) = match constraint {
            Constraint::Unique { columns } => (
                format!("requires {} to be unique", columns.join(" and ")),
                "duplicates can be written after the import".to_string(),
            ),
            Constraint::Check { expression } => (
                format!("checks {expression}"),
                "rows that would fail it can be written after the import".to_string(),
            ),
            Constraint::ForeignKey {
                columns,
                references,
            } => (
                format!(
                    "has a foreign key on {} referencing {}",
                    columns.join(" and "),
                    if references.is_empty() {
                        "another table"
                    } else {
                        references
                    }
                ),
                "nothing will stop a row pointing at something that is not there".to_string(),
            ),
            Constraint::Collation { column, name } => (
                format!("compares {column} using the {name} collation"),
                "comparisons and ordering on it will differ from the source".to_string(),
            ),
        };
        plan.notes.push(Note::ConstraintNotCarried {
            table: table.to_string(),
            what,
            consequence,
        });
    }
}

enum KeyChoice {
    /// These declared columns become the key, in this order.
    Columns(Vec<String>),
    AddRowid {
        reason: String,
    },
}

/// Pick a key, and never fail to pick one.
/// The longest key our b-tree accepts, which depends on the page size a
/// database was created with.
///
/// This is not a detail the importer can leave to the writer. A key that is
/// one byte too long fails on the row that holds it, halfway through a
/// table, with a database already part written, which is exactly the class
/// of surprise the planning pass exists to move to the front.
fn key_limit() -> usize {
    quanty_core::max_key_len(quanty_core::PagerOptions::default().page_size)
}

fn choose_key(def: &quanty_sqlite::TableDef, survey: &TableSurvey) -> KeyChoice {
    if let Some(alias) = def.rowid_alias() {
        return KeyChoice::Columns(vec![alias.name.clone()]);
    }
    if survey.primary_key.is_empty() {
        return KeyChoice::AddRowid {
            reason: "no primary key".to_string(),
        };
    }
    // a primary key that is not a rowid alias may hold null in sqlite, and
    // our key columns may not
    for key in &survey.primary_key {
        let column = survey
            .columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(key));
        match column {
            Some(column) if column.has_nulls() => {
                return KeyChoice::AddRowid {
                    reason: format!("a primary key whose column {key} holds null"),
                }
            }
            Some(column) if column.is_virtual => {
                return KeyChoice::AddRowid {
                    reason: format!("a primary key over the virtual column {key}"),
                }
            }
            // a key column holding a value longer than the b-tree allows
            // cannot be a key here, and the rowid can, so the table still
            // imports and the report says what changed
            Some(column) if column.longest_value > key_limit() => {
                return KeyChoice::AddRowid {
                    reason: format!(
                        "a primary key over {key}, which holds a value of {} bytes and keys \
                         stop at {}",
                        column.longest_value,
                        key_limit()
                    ),
                }
            }
            Some(_) => {}
            None => {
                return KeyChoice::AddRowid {
                    reason: format!("a primary key over {key}, which is not a stored column"),
                }
            }
        }
    }
    KeyChoice::Columns(survey.primary_key.clone())
}

/// The rule from ADR-019, in one place.
fn decide_type(
    column: &ColumnSurvey,
    qualified: &str,
    options: &Options,
    plan: &mut ImportPlan,
) -> TypeName {
    let classes = column.classes();
    match classes.as_slice() {
        // nothing stored: the declaration is all we have
        [] => {
            let ty = from_affinity(column);
            plan.notes.push(Note::TypeFromDeclarationOnly {
                column: qualified.to_string(),
                declared: column.declared_type.clone(),
                ty,
            });
            ty
        }
        [StorageClass::Integer] => TypeName::Int,
        [StorageClass::Real] => TypeName::Float,
        [StorageClass::Text] => TypeName::Text,
        [StorageClass::Blob] => TypeName::Bytes,
        // integers and reals together are the common mixture, and float
        // holds both, as long as every integer survives the trip
        [StorageClass::Integer, StorageClass::Real] => {
            if column.largest_integer > LARGEST_EXACT_INTEGER_IN_A_FLOAT {
                plan.problems.push(Problem::IntegerTooLargeForFloat {
                    column: qualified.to_string(),
                    largest: column.largest_integer,
                });
            }
            widen(qualified, &classes, TypeName::Float, options, plan);
            TypeName::Float
        }
        // anything with a blob in it keeps its bytes
        mixed if mixed.contains(&StorageClass::Blob) => {
            widen(qualified, &classes, TypeName::Bytes, options, plan);
            TypeName::Bytes
        }
        mixed => {
            let _ = mixed;
            widen(qualified, &classes, TypeName::Text, options, plan);
            TypeName::Text
        }
    }
}

fn widen(
    qualified: &str,
    classes: &[StorageClass],
    to: TypeName,
    options: &Options,
    plan: &mut ImportPlan,
) {
    plan.notes.push(Note::Widened {
        column: qualified.to_string(),
        classes: classes.to_vec(),
        to,
    });
    if options.strict {
        plan.problems.push(Problem::WouldWiden {
            column: qualified.to_string(),
            classes: classes.to_vec(),
            to,
        });
    }
}

fn from_affinity(column: &ColumnSurvey) -> TypeName {
    use quanty_sqlite::Affinity;
    match column.affinity {
        Affinity::Integer => TypeName::Int,
        Affinity::Real => TypeName::Float,
        Affinity::Text => TypeName::Text,
        Affinity::Blob => TypeName::Bytes,
        // numeric means integer or real, and with nothing stored to say
        // which, the narrower one is the honest guess
        Affinity::Numeric => TypeName::Int,
    }
}

fn value_fits(value: &Value, ty: TypeName) -> bool {
    matches!(
        (value, ty),
        (Value::Int(_), TypeName::Int)
            | (Value::Int(_), TypeName::Float)
            | (Value::Float(_), TypeName::Float)
            | (Value::Text(_), TypeName::Text)
            | (Value::Bytes(_), TypeName::Bytes)
            | (Value::Bool(_), TypeName::Bool)
    )
}

/// The indexes we can express, as (table, column) pairs.
///
/// We have `@index` on a single column and nothing else, so a composite or
/// expression index cannot be carried over. Those are noted rather than
/// silently dropped: an index that vanishes turns a fast query slow, which
/// is the kind of surprise that costs an afternoon to find.
///
/// An index sqlite created for a primary key or a unique constraint has no
/// statement of its own and is not noted, because the key it enforces comes
/// across as a key.
fn collect_indexes(schema: &quanty_sqlite::Schema, plan: &mut ImportPlan) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for index in schema.indexes() {
        let Some(sql) = &index.sql else {
            continue;
        };
        match single_column_of(sql) {
            Some(column) => out.push((index.table_name.clone(), column)),
            None => plan.notes.push(Note::Skipped {
                what: format!("the index {}", index.name),
                reason: "it covers more than one column or an expression, which we cannot \
                         express yet"
                    .to_string(),
            }),
        }
    }
    out
}

/// The single column an index covers, if it covers exactly one.
fn single_column_of(sql: &str) -> Option<String> {
    let open = sql.find('(')?;
    let close = sql.rfind(')')?;
    if close <= open {
        return None;
    }
    let inside = &sql[open + 1..close];
    if inside.contains(',') || inside.contains('(') {
        return None;
    }
    let column = inside
        .trim()
        .trim_matches(|c| c == '"' || c == '[' || c == ']' || c == '`')
        .trim();
    // `create index i on t (c collate nocase)` and `(c desc)` are still one
    // column, but the extra words are not part of its name
    let name = column.split_whitespace().next()?;
    (!name.is_empty()).then(|| name.to_string())
}
