//! A [`Connection`] served by a helper program over its standard input and output, for engines
//! that are too large to link into the app (DuckDB). See `docs/duckdb-research.md`, section 7.
//!
//! One helper process holds one engine instance. Sessions on the same database file share the
//! process, because an engine such as DuckDB allows only one process to open a file read-write.

pub mod proto;

use std::collections::HashMap;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use tokio::sync::mpsc;

use self::proto::{
    Call, Hello, PROTOCOL_VERSION, Reply, Request, RequestBody, RequestId, Response, ResponseBody, SessionId,
    SessionInfo,
};
use super::{BatchSink, Connection, Explained};
use crate::config::ConnectionConfig;
use crate::dialect::Dialect;
use crate::engines::HelperEngine;
use crate::model::{SchemaObject, StatementResult, TableStructure};
use crate::plan::ExplainStatements;
use crate::server::ServerInfo;

/// Replies queued per request before the helper's reader waits for the caller, so a slow export
/// sink slows the helper instead of filling memory.
const REPLY_QUEUE: usize = 4;

type Waiters = HashMap<RequestId, mpsc::Sender<ResponseBody>>;

/// The reader thread's view of a helper: who waits for which reply, and why the helper stopped.
#[derive(Default)]
struct Routes {
    waiters: Waiters,
    exited: Option<String>,
}

/// A running helper process.
struct Helper {
    engine: &'static HelperEngine,
    hello: Hello,
    /// Frames for the writer thread. Dropping the sender closes the helper's input, which ends it.
    outbox: std::sync::mpsc::Sender<Vec<u8>>,
    routes: Arc<Mutex<Routes>>,
    next_request: AtomicU64,
    next_session: AtomicU64,
}

impl Helper {
    fn spawn(engine: &'static HelperEngine, program: &Path) -> Result<Self> {
        let mut child = Command::new(program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("starting {}", program.display()))?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let routes = Arc::new(Mutex::new(Routes::default()));
        let (outbox, frames) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::Builder::new().name(format!("{}-writer", engine.name)).spawn(move || write_loop(stdin, frames))?;
        let reader_routes = routes.clone();
        std::thread::Builder::new()
            .name(format!("{}-reader", engine.name))
            .spawn(move || read_loop(child, stdout, reader_routes, engine.label))?;
        Ok(Self {
            engine,
            hello: Hello { protocol: 0, engine: String::new(), engine_version: String::new() },
            outbox,
            routes,
            next_request: AtomicU64::new(1),
            next_session: AtomicU64::new(1),
        })
    }

    /// Starts the helper and checks that it speaks this app's protocol.
    async fn start(engine: &'static HelperEngine) -> Result<Arc<Self>> {
        let program = crate::engines::locate(engine)?;
        let mut helper = Self::spawn(engine, &program)?;
        let Reply::Hello(hello) = helper.request(RequestBody::Hello).await? else {
            bail!("{} answered the handshake with something else", program.display());
        };
        if hello.protocol != PROTOCOL_VERSION || hello.engine != engine.name {
            bail!(
                "{} speaks {} protocol {}, but this app needs {} protocol {PROTOCOL_VERSION}",
                program.display(),
                hello.engine,
                hello.protocol,
                engine.name
            );
        }
        helper.hello = hello;
        Ok(Arc::new(helper))
    }

    fn exited(&self) -> Option<String> {
        self.routes.lock().unwrap().exited.clone()
    }

    /// Sends a request and returns the receiver for its replies.
    fn send(&self, body: RequestBody, queue: usize) -> Result<(RequestId, mpsc::Receiver<ResponseBody>)> {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(queue);
        {
            let mut routes = self.routes.lock().unwrap();
            if let Some(why) = &routes.exited {
                bail!("{why}");
            }
            routes.waiters.insert(id, tx);
        }
        let mut frame = Vec::new();
        proto::encode(&Request { id, body }, &mut frame)?;
        if self.outbox.send(frame).is_err() {
            self.routes.lock().unwrap().waiters.remove(&id);
            bail!("the {} helper's input is closed", self.engine.label);
        }
        Ok((id, rx))
    }

    async fn request(&self, body: RequestBody) -> Result<Reply> {
        let (_, mut rx) = self.send(body, 1)?;
        loop {
            match rx.recv().await {
                Some(ResponseBody::Done(reply)) => return reply.map_err(|e| anyhow!(e)),
                Some(ResponseBody::Batch { .. }) => continue,
                None => bail!("{}", self.exited().unwrap_or_else(|| "the helper dropped the request".into())),
            }
        }
    }

    /// Sends a request whose reply nobody waits for.
    fn notify(&self, body: RequestBody) {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let mut frame = Vec::new();
        if proto::encode(&Request { id, body }, &mut frame).is_ok() {
            let _ = self.outbox.send(frame);
        }
    }
}

fn write_loop(stdin: ChildStdin, frames: std::sync::mpsc::Receiver<Vec<u8>>) {
    let mut stdin = BufWriter::new(stdin);
    while let Ok(frame) = frames.recv() {
        if proto::write_frame(&mut stdin, &frame).is_err() {
            return;
        }
    }
}

fn read_loop(mut child: Child, stdout: std::process::ChildStdout, routes: Arc<Mutex<Routes>>, label: &'static str) {
    let mut stdout = BufReader::with_capacity(1 << 16, stdout);
    let mut buf = Vec::new();
    let failure = loop {
        match proto::read_frame(&mut stdout, &mut buf) {
            Ok(true) => {}
            Ok(false) => break None,
            Err(e) => break Some(format!("{e:#}")),
        }
        let response: Response = match proto::decode(&buf) {
            Ok(r) => r,
            Err(e) => break Some(format!("unreadable reply: {e:#}")),
        };
        let done = matches!(response.body, ResponseBody::Done(_));
        let waiter = {
            let mut routes = routes.lock().unwrap();
            if done { routes.waiters.remove(&response.id) } else { routes.waiters.get(&response.id).cloned() }
        };
        if let Some(waiter) = waiter {
            // A caller that stopped listening is not an error.
            let _ = waiter.blocking_send(response.body);
        }
    };
    let status = match child.wait() {
        Ok(s) => describe_exit(s),
        Err(e) => e.to_string(),
    };
    let why = match failure {
        Some(f) => format!("The {label} helper stopped ({status}): {f}"),
        None => format!("The {label} helper stopped ({status})"),
    };
    let waiters = {
        let mut routes = routes.lock().unwrap();
        routes.exited = Some(why.clone());
        std::mem::take(&mut routes.waiters)
    };
    for (_, waiter) in waiters {
        let _ = waiter.blocking_send(ResponseBody::Done(Err(why.clone())));
    }
}

fn describe_exit(status: std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("killed by signal {signal}");
        }
    }
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => status.to_string(),
    }
}

/// Helpers that hold a database file, by canonical path and access mode. Every session on that
/// file, the Test button included, goes through the same process.
static SHARED: LazyLock<Mutex<HashMap<SharedFile, Weak<Helper>>>> = LazyLock::new(Default::default);

/// A database file's canonical path and whether it is open read-only.
type SharedFile = (PathBuf, bool);

async fn helper_for(engine: &'static HelperEngine, shared_file: Option<SharedFile>) -> Result<Arc<Helper>> {
    let Some(key) = shared_file else { return Helper::start(engine).await };
    let running = SHARED.lock().unwrap().get(&key).and_then(Weak::upgrade).filter(|h| h.exited().is_none());
    if let Some(helper) = running {
        return Ok(helper);
    }
    let helper = Helper::start(engine).await?;
    let mut shared = SHARED.lock().unwrap();
    // Another caller may have started one meanwhile; keep the first so the file has one owner.
    if let Some(existing) = shared.get(&key).and_then(Weak::upgrade).filter(|h| h.exited().is_none()) {
        return Ok(existing);
    }
    shared.retain(|_, h| h.strong_count() > 0);
    shared.insert(key, Arc::downgrade(&helper));
    Ok(helper)
}

/// The helper and the session in it that a [`RemoteConnection`] currently uses.
struct Bound {
    helper: Arc<Helper>,
    session: SessionId,
}

pub struct RemoteConnection {
    engine: &'static HelperEngine,
    config: ConnectionConfig,
    bound: tokio::sync::Mutex<Bound>,
    info: SessionInfo,
}

impl RemoteConnection {
    pub async fn open(engine: &'static HelperEngine, config: &ConnectionConfig) -> Result<Self> {
        let (bound, info) = Self::bind(engine, config).await?;
        Ok(Self { engine, config: config.clone(), bound: tokio::sync::Mutex::new(bound), info })
    }

    async fn bind(engine: &'static HelperEngine, config: &ConnectionConfig) -> Result<(Bound, SessionInfo)> {
        let helper = helper_for(engine, (engine.shared_file)(config)).await?;
        let session = helper.next_session.fetch_add(1, Ordering::Relaxed);
        match helper.request(RequestBody::Open { session, config: config.clone() }).await? {
            Reply::Opened(info) => Ok((Bound { helper, session }, info)),
            other => bail!("unexpected reply to open: {other:?}"),
        }
    }

    /// The helper and session to send to. A helper that stopped is replaced by a new one, with
    /// a new session, so the connection recovers on the next request after a crash.
    async fn target(&self) -> Result<(Arc<Helper>, SessionId)> {
        let mut bound = self.bound.lock().await;
        if bound.helper.exited().is_some() {
            *bound = Self::bind(self.engine, &self.config).await?.0;
        }
        Ok((bound.helper.clone(), bound.session))
    }

    async fn call(&self, call: Call) -> Result<Reply> {
        let (helper, session) = self.target().await?;
        helper.request(RequestBody::Call { session, call }).await
    }

    /// The engine's version as its helper reported it.
    pub async fn engine_version(&self) -> String {
        self.bound.lock().await.helper.hello.engine_version.clone()
    }
}

impl Drop for RemoteConnection {
    fn drop(&mut self) {
        let bound = self.bound.get_mut();
        bound.helper.notify(RequestBody::Close { session: bound.session });
    }
}

fn unexpected(method: &str, reply: Reply) -> anyhow::Error {
    anyhow!("unexpected reply to {method}: {reply:?}")
}

#[async_trait]
impl Connection for RemoteConnection {
    fn dialect(&self) -> Dialect {
        self.info.dialect
    }

    fn server_version(&self) -> String {
        self.info.server_version.clone()
    }

    fn server_info(&self) -> ServerInfo {
        self.info.server_info
    }

    fn database(&self) -> String {
        self.info.database.clone()
    }

    fn default_schema(&self) -> String {
        self.info.default_schema.clone()
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        match self.call(Call::ListDatabases).await? {
            Reply::Names(n) => Ok(n),
            other => Err(unexpected("list_databases", other)),
        }
    }

    async fn list_schemas(&self) -> Result<Vec<String>> {
        match self.call(Call::ListSchemas).await? {
            Reply::Names(n) => Ok(n),
            other => Err(unexpected("list_schemas", other)),
        }
    }

    async fn list_objects(&self) -> Result<Vec<SchemaObject>> {
        match self.call(Call::ListObjects).await? {
            Reply::Objects(o) => Ok(o),
            other => Err(unexpected("list_objects", other)),
        }
    }

    async fn table_structure(&self, schema: &str, table: &str) -> Result<TableStructure> {
        match self.call(Call::TableStructure { schema: schema.into(), table: table.into() }).await? {
            Reply::Structure(s) => Ok(s),
            other => Err(unexpected("table_structure", other)),
        }
    }

    async fn object_definition(&self, object: &SchemaObject) -> Result<String> {
        match self.call(Call::ObjectDefinition(object.clone())).await? {
            Reply::Text(t) => Ok(t),
            other => Err(unexpected("object_definition", other)),
        }
    }

    async fn query(&self, sql: &str, max_rows: Option<usize>) -> Result<StatementResult> {
        match self.call(Call::Query { sql: sql.into(), max_rows }).await? {
            Reply::Statement(r) => Ok(r),
            other => Err(unexpected("query", other)),
        }
    }

    async fn query_batches(&self, sql: &str, batch: usize, sink: &mut BatchSink<'_>) -> Result<u64> {
        let (helper, session) = self.target().await?;
        let call = Call::QueryBatches { sql: sql.into(), batch };
        let (_, mut rx) = helper.send(RequestBody::Call { session, call }, REPLY_QUEUE)?;
        let mut sink_error = None;
        loop {
            match rx.recv().await {
                Some(ResponseBody::Batch { columns, rows }) => {
                    if sink_error.is_none()
                        && let Err(e) = sink(&columns, rows)
                    {
                        sink_error = Some(e);
                        helper.notify(RequestBody::Cancel { session });
                    }
                }
                Some(ResponseBody::Done(reply)) => {
                    if let Some(e) = sink_error {
                        return Err(e);
                    }
                    return match reply.map_err(|e| anyhow!(e))? {
                        Reply::Count(n) => Ok(n),
                        other => Err(unexpected("query_batches", other)),
                    };
                }
                None => bail!("{}", helper.exited().unwrap_or_else(|| "the helper dropped the request".into())),
            }
        }
    }

    async fn execute_in_transaction(&self, statements: &[String]) -> Result<u64> {
        match self.call(Call::ExecuteInTransaction(statements.to_vec())).await? {
            Reply::Count(n) => Ok(n),
            other => Err(unexpected("execute_in_transaction", other)),
        }
    }

    async fn explain(&self, req: &ExplainStatements) -> Result<Explained> {
        match self.call(Call::Explain(req.clone())).await? {
            Reply::Explained(e) => Ok(e),
            other => Err(unexpected("explain", other)),
        }
    }

    async fn cancel(&self) -> Result<()> {
        let (helper, session) = {
            let bound = self.bound.lock().await;
            (bound.helper.clone(), bound.session)
        };
        helper.request(RequestBody::Cancel { session }).await.map(|_| ())
    }

    async fn ping(&self) -> Result<()> {
        self.call(Call::Ping).await.map(|_| ())
    }
}
