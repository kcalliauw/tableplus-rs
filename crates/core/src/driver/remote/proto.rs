//! Messages between the app and a helper program such as `tableplus-duckdb`, and how they are
//! framed on the pipe. One request mirrors one [`Connection`](crate::driver::Connection) method.
//! Every frame is a little-endian `u32` length followed by that many bytes of one encoded
//! message.

use std::io::{self, Read, Write};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::config::ConnectionConfig;
use crate::dialect::Dialect;
use crate::driver::Explained;
use crate::model::{ColumnMeta, Row, SchemaObject, StatementResult, TableStructure};
use crate::plan::ExplainStatements;
use crate::server::ServerInfo;

/// Bumped whenever a message changes shape. The app refuses a helper that answers with another
/// version.
pub const PROTOCOL_VERSION: u32 = 1;

/// A frame larger than this is a corrupt stream, not a message.
const MAX_FRAME: usize = 1 << 30;

/// Picks a session inside one helper process.
pub type SessionId = u64;

/// Matches a reply to the request that asked for it.
pub type RequestId = u64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: RequestId,
    pub body: RequestBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RequestBody {
    /// The first request on a new pipe.
    Hello,
    Open {
        session: SessionId,
        config: ConnectionConfig,
    },
    Close {
        session: SessionId,
    },
    /// Interrupts whatever the session is running. Answered at once, before the interrupted call.
    Cancel {
        session: SessionId,
    },
    Call {
        session: SessionId,
        call: Call,
    },
}

/// The [`Connection`](crate::driver::Connection) methods that do work in the helper.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Call {
    ListDatabases,
    ListSchemas,
    ListObjects,
    TableStructure {
        schema: String,
        table: String,
    },
    ObjectDefinition(SchemaObject),
    Query {
        sql: String,
        max_rows: Option<usize>,
    },
    /// Answered with [`ResponseBody::Batch`] messages, then a [`Reply::Count`].
    QueryBatches {
        sql: String,
        batch: usize,
    },
    ExecuteInTransaction(Vec<String>),
    Explain(ExplainStatements),
    Ping,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: RequestId,
    pub body: ResponseBody,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ResponseBody {
    /// One batch of a [`Call::QueryBatches`] stream; the request's final answer follows.
    Batch { columns: Vec<ColumnMeta>, rows: Vec<Row> },
    /// The request's final answer.
    Done(Result<Reply, String>),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Reply {
    Hello(Hello),
    Opened(SessionInfo),
    Unit,
    Names(Vec<String>),
    Objects(Vec<SchemaObject>),
    Structure(TableStructure),
    Text(String),
    Statement(StatementResult),
    Count(u64),
    Explained(Explained),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    /// For example `duckdb`.
    pub engine: String,
    /// The engine library's own version, for example `v1.5.6`.
    pub engine_version: String,
}

/// What [`Connection`](crate::driver::Connection)'s synchronous methods return, fixed when the
/// session opens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub dialect: Dialect,
    pub server_version: String,
    pub server_info: ServerInfo,
    pub database: String,
    pub default_schema: String,
}

/// Encodes `message` with postcard as one frame: the length, then the bytes. Postcard was
/// measured against JSON on the shop data and a 50-million-row read (`docs/duckdb-research.md`,
/// section 7); JSON was 2 to 3 times slower and can't carry NaN or infinite floats.
pub fn encode<T: Serialize>(message: &T, out: &mut Vec<u8>) -> Result<()> {
    out.clear();
    out.extend_from_slice(&[0; 4]);
    postcard::to_io(message, &mut *out)?;
    let len = u32::try_from(out.len() - 4).context("message too large for one frame")?;
    out[..4].copy_from_slice(&len.to_le_bytes());
    Ok(())
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    Ok(postcard::from_bytes(bytes)?)
}

/// Reads one frame's payload into `buf`. `Ok(false)` at a clean end of the stream.
pub fn read_frame(reader: &mut impl Read, buf: &mut Vec<u8>) -> Result<bool> {
    let mut len = [0u8; 4];
    match reader.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        bail!("pipe frame of {len} bytes is too large");
    }
    buf.resize(len, 0);
    reader.read_exact(buf)?;
    Ok(true)
}

/// Writes an encoded frame from [`encode`] and flushes it.
pub fn write_frame(writer: &mut impl Write, frame: &[u8]) -> io::Result<()> {
    writer.write_all(frame)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ResultSet;
    use crate::value::Value;

    #[test]
    fn frames_round_trip() {
        {
            let rows: Vec<Row> = vec![
                vec![Value::Int(-1), Value::text("é"), Value::Bytes(vec![0, 255].into()), Value::Float(f64::NAN)]
                    .into(),
            ];
            let sent = Response {
                id: 7,
                body: ResponseBody::Done(Ok(Reply::Statement(StatementResult::Rows(ResultSet {
                    columns: vec![ColumnMeta::new("a", "INTEGER")],
                    rows,
                    truncated: true,
                })))),
            };
            let mut frame = Vec::new();
            encode(&sent, &mut frame).unwrap();
            let mut reader = io::Cursor::new(frame);
            let mut buf = Vec::new();
            assert!(read_frame(&mut reader, &mut buf).unwrap());
            let back: Response = decode(&buf).unwrap();
            let ResponseBody::Done(Ok(Reply::Statement(StatementResult::Rows(rs)))) = back.body else { panic!() };
            assert_eq!(back.id, 7);
            assert_eq!(rs.rows[0][2], Value::Bytes(vec![0, 255].into()));
            assert!(rs.truncated);
            assert!(matches!(rs.rows[0][3], Value::Float(f) if f.is_nan()), "NaN crosses the pipe");
            assert!(!read_frame(&mut reader, &mut buf).unwrap(), "clean end of stream");
        }
    }
}
