//! Serves a tableplus-rs [`Connection`] from a helper program: requests arrive on standard
//! input, replies leave on standard output, both framed as in
//! [`tableplus_core::driver::remote::proto`]. Each request runs as its own task, so a cancel
//! reaches a session while that session's query runs.

use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Write};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tableplus_core::ConnectionConfig;
use tableplus_core::driver::DynConnection;
use tableplus_core::driver::remote::proto::{
    self, Call, Hello, PROTOCOL_VERSION, Reply, Request, RequestBody, RequestId, Response, ResponseBody, SessionId,
    SessionInfo,
};

/// Encoded replies waiting for standard output. A full queue makes a streaming query wait for
/// the app to read, so a large export never piles up in memory.
const OUTBOX: usize = 8;

/// An engine the helper serves.
#[async_trait::async_trait]
pub trait Engine: Send + Sync + 'static {
    /// The engine's name in the handshake, matching the app's engine list.
    fn name(&self) -> &'static str;
    /// The engine library's own version.
    fn version(&self) -> String;
    /// Opens one session.
    async fn open(&self, config: ConnectionConfig) -> Result<DynConnection>;
}

#[derive(Clone)]
struct Outbox {
    frames: SyncSender<Vec<u8>>,
}

impl Outbox {
    fn send(&self, id: RequestId, body: ResponseBody) {
        let mut frame = Vec::new();
        let body = match proto::encode(&Response { id, body }, &mut frame) {
            Ok(()) => None,
            Err(e) => Some(ResponseBody::Done(Err(format!("could not encode the reply: {e:#}")))),
        };
        if let Some(body) = body
            && proto::encode(&Response { id, body }, &mut frame).is_err()
        {
            return;
        }
        // Waiting for room here is the backpressure; the app gone means nobody reads anyway.
        let _ = tokio::task::block_in_place(|| self.frames.send(frame));
    }

    fn done(&self, id: RequestId, reply: Result<Reply>) {
        self.send(id, ResponseBody::Done(reply.map_err(|e| format!("{e:#}"))));
    }
}

type Sessions = Arc<Mutex<HashMap<SessionId, DynConnection>>>;

/// Serves `engine` until standard input closes.
pub fn serve(engine: impl Engine) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let (frames, queued) = sync_channel::<Vec<u8>>(OUTBOX);
    let writer = std::thread::Builder::new().name("stdout".into()).spawn(move || write_loop(queued))?;
    let outbox = Outbox { frames };
    let engine = Arc::new(engine);
    let sessions: Sessions = Arc::default();

    let mut stdin = BufReader::with_capacity(1 << 16, std::io::stdin().lock());
    let mut buf = Vec::new();
    while proto::read_frame(&mut stdin, &mut buf)? {
        let request: Request = proto::decode(&buf)?;
        let (outbox, engine, sessions) = (outbox.clone(), engine.clone(), sessions.clone());
        runtime.spawn(async move { handle(request, &*engine, &sessions, &outbox).await });
    }

    // The app closed the pipe: stop running statements, then close every session so the engine
    // can write its files back before the process exits.
    let open: Vec<DynConnection> = sessions.lock().unwrap().drain().map(|(_, c)| c).collect();
    runtime.block_on(async {
        for conn in &open {
            let _ = conn.cancel().await;
        }
    });
    drop(open);
    runtime.shutdown_timeout(Duration::from_secs(5));
    drop(outbox);
    let _ = writer.join();
    Ok(())
}

fn write_loop(queued: Receiver<Vec<u8>>) {
    let mut stdout = BufWriter::with_capacity(1 << 16, std::io::stdout().lock());
    while let Ok(first) = queued.recv() {
        // Flush only when nothing else is queued, so a stream of batches leaves in large writes.
        for frame in std::iter::once(first).chain(queued.try_iter()) {
            if stdout.write_all(&frame).is_err() {
                return;
            }
        }
        if stdout.flush().is_err() {
            return;
        }
    }
}

fn session(sessions: &Sessions, id: SessionId) -> Result<DynConnection> {
    sessions.lock().unwrap().get(&id).cloned().ok_or_else(|| anyhow::anyhow!("session {id} is not open"))
}

async fn handle(request: Request, engine: &dyn Engine, sessions: &Sessions, outbox: &Outbox) {
    let id = request.id;
    let reply = match request.body {
        RequestBody::Hello => Ok(Reply::Hello(Hello {
            protocol: PROTOCOL_VERSION,
            engine: engine.name().into(),
            engine_version: engine.version(),
        })),
        RequestBody::Open { session, config } => match engine.open(config).await {
            Ok(conn) => {
                let info = SessionInfo {
                    dialect: conn.dialect(),
                    server_version: conn.server_version(),
                    server_info: conn.server_info(),
                    database: conn.database(),
                    default_schema: conn.default_schema(),
                };
                sessions.lock().unwrap().insert(session, conn);
                Ok(Reply::Opened(info))
            }
            Err(e) => Err(e),
        },
        RequestBody::Close { session } => {
            let closed = sessions.lock().unwrap().remove(&session);
            drop(closed);
            Ok(Reply::Unit)
        }
        RequestBody::Cancel { session: s } => match session(sessions, s) {
            Ok(conn) => conn.cancel().await.map(|()| Reply::Unit),
            Err(e) => Err(e),
        },
        RequestBody::Call { session: s, call } => match session(sessions, s) {
            Ok(conn) => run(&conn, call, id, outbox).await,
            Err(e) => Err(e),
        },
    };
    outbox.done(id, reply);
}

async fn run(conn: &DynConnection, call: Call, id: RequestId, outbox: &Outbox) -> Result<Reply> {
    Ok(match call {
        Call::ListDatabases => Reply::Names(conn.list_databases().await?),
        Call::ListSchemas => Reply::Names(conn.list_schemas().await?),
        Call::ListObjects => Reply::Objects(conn.list_objects().await?),
        Call::TableStructure { schema, table } => Reply::Structure(conn.table_structure(&schema, &table).await?),
        Call::ObjectDefinition(object) => Reply::Text(conn.object_definition(&object).await?),
        Call::Query { sql, max_rows } => Reply::Statement(conn.query(&sql, max_rows).await?),
        Call::QueryBatches { sql, batch } => {
            let mut sink = |columns: &[tableplus_core::ColumnMeta], rows: Vec<tableplus_core::Row>| {
                outbox.send(id, ResponseBody::Batch { columns: columns.to_vec(), rows });
                Ok(())
            };
            Reply::Count(conn.query_batches(&sql, batch, &mut sink).await?)
        }
        Call::ExecuteInTransaction(statements) => Reply::Count(conn.execute_in_transaction(&statements).await?),
        Call::Explain(req) => Reply::Explained(conn.explain(&req).await?),
        Call::Ping => {
            conn.ping().await?;
            Reply::Unit
        }
    })
}
