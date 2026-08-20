//! One statement in, the messages that answer it out.

use quanty_core::{Storage, Value};
use quanty_exec::{ExecError, Output, Session};
use quanty_proto::{batch_rows, ClientMessage, ErrorCode, ServerMessage};
use quanty_ql::ast::Statement;
use quanty_ql::ParseError;

/// What the executor may do with a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Writes nothing, so it may run while another connection holds a
    /// transaction. There is no commit to share, so it is never batched.
    Reads,
    /// Can share a commit with its neighbours in the queue.
    Batchable,
    /// Opens a transaction, so this connection holds the queue afterwards.
    Begins,
    /// Closes one.
    Ends,
    /// Manages its own commit and cannot run inside a transaction.
    Alone,
}

/// A parsed request, ready to run.
pub(crate) struct Parsed {
    pub(crate) statement: Statement,
    pub(crate) kind: Kind,
}

/// Parse a request, or the messages that say why it could not be.
///
/// Parsing happens here rather than inside `Session::execute` because the
/// executor has to know what a statement is before deciding whether it can
/// share a commit with the one behind it.
pub(crate) fn parse(request: &ClientMessage) -> Result<Option<Parsed>, Vec<ServerMessage>> {
    let parsed = match request {
        // No authentication is required yet, so a token is accepted
        // without being looked at. ADR-026 decides where tokens will live;
        // until it is built, saying `Ready` is what this server means.
        ClientMessage::Auth(_) => return Ok(None),
        ClientMessage::Query(source) => quanty_ql::parse(source),
        ClientMessage::QuerySql(source) => quanty_ql::parse_sql(source),
        // The connection handles `Close` itself and never submits one.
        ClientMessage::Close => return Ok(None),
    };
    match parsed {
        Ok(statement) => {
            let kind = classify(&statement);
            Ok(Some(Parsed { statement, kind }))
        }
        Err(e) => Err(failed(ErrorCode::Parse, parse_message(&e))),
    }
}

fn parse_message(e: &ParseError) -> String {
    ExecError::from(e.clone()).to_string()
}

fn classify(statement: &Statement) -> Kind {
    match statement {
        // Conservative on purpose: `log` and `show branches` write nothing
        // either, but they refuse to run inside a transaction, so they stay
        // where the engine already puts them.
        Statement::Get(_) | Statement::ShowTables | Statement::Explain(_) => Kind::Reads,
        Statement::Begin => Kind::Begins,
        Statement::Commit | Statement::Rollback => Kind::Ends,
        // These manage commits at the database level and refuse to run
        // inside a transaction, so batching them would break them.
        Statement::Branch { .. }
        | Statement::Switch { .. }
        | Statement::Merge { .. }
        | Statement::DropBranch { .. }
        | Statement::ShowBranches
        | Statement::Log
        | Statement::Gc { .. } => Kind::Alone,
        _ => Kind::Batchable,
    }
}

/// Run one parsed statement against a session that is ready for it.
pub(crate) fn answer<S: Storage>(session: &mut Session<S>, parsed: &Parsed) -> Vec<ServerMessage> {
    render(session.execute_ast(&parsed.statement))
}

/// The reply to a request that never reaches the engine.
pub(crate) fn ready() -> Vec<ServerMessage> {
    vec![ServerMessage::Ready]
}

/// An error the executor produced rather than the engine.
pub(crate) fn failed(code: ErrorCode, detail: impl Into<String>) -> Vec<ServerMessage> {
    vec![ServerMessage::error(code, detail.into())]
}

fn render(result: Result<Output, ExecError>) -> Vec<ServerMessage> {
    let output = match result {
        Ok(output) => output,
        Err(e) => return failed(code_for(&e), e.to_string()),
    };
    match output {
        Output::Ok => vec![ServerMessage::Ok],
        Output::Count { verb, n } => vec![ServerMessage::Count {
            verb: verb.to_string(),
            n,
        }],
        Output::Lines(lines) => vec![ServerMessage::Lines(lines)],
        Output::Rows { columns, rows } => rows_or_error(columns, rows),
    }
}

/// A result set, or the error that describes why it could not be framed.
fn rows_or_error(columns: Vec<String>, rows: Vec<Vec<Value>>) -> Vec<ServerMessage> {
    let mut out = vec![ServerMessage::RowsBegin { columns }];
    match batch_rows(rows) {
        Ok(batches) => out.extend(batches),
        Err(e) => {
            out.push(ServerMessage::error(ErrorCode::Execution, e.to_string()));
            return out;
        }
    }
    out.push(ServerMessage::RowsEnd);
    out
}

fn code_for(e: &ExecError) -> ErrorCode {
    match e {
        ExecError::Parse(_) => ErrorCode::Parse,
        _ => ErrorCode::Execution,
    }
}
