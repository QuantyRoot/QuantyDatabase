//! One statement in, the messages that answer it out.

use quanty_core::{Storage, Value};
use quanty_exec::{ExecError, Output, Session};
use quanty_proto::{batch_rows, ClientMessage, ErrorCode, ServerMessage};

/// Run one request against a session that is ready for it.
pub(crate) fn answer<S: Storage>(
    session: &mut Session<S>,
    request: &ClientMessage,
) -> Vec<ServerMessage> {
    match request {
        // No authentication is required yet, so a token is accepted
        // without being looked at. ADR-026 decides where tokens will live;
        // until it is built, saying `Ready` is what this server means.
        ClientMessage::Auth(_) => vec![ServerMessage::Ready],
        ClientMessage::Query(source) => render(session.execute(source)),
        ClientMessage::QuerySql(source) => render(session.execute_sql(source)),
        // The connection handles `Close` itself and never submits one.
        ClientMessage::Close => Vec::new(),
    }
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
