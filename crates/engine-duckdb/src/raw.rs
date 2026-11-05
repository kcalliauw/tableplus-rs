//! Safe wrappers over the parts of DuckDB's C API the driver uses. The `duckdb` crate keeps its
//! connection and statement handles private, and the driver needs them for statement counts,
//! statement types and interrupts, so it talks to `libduckdb-sys` directly.

use std::ffi::{CStr, CString, c_char};
use std::marker::PhantomData;
use std::ptr;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use libduckdb_sys as ffi;

/// Copies a string DuckDB allocated with `duckdb_malloc` and frees it.
///
/// # Safety
///
/// `p` is null or a NUL-terminated string the caller owns and must free with `duckdb_free`.
unsafe fn owned_string(p: *mut c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    unsafe { ffi::duckdb_free(p.cast()) };
    Some(s)
}

/// Copies a string DuckDB keeps ownership of.
///
/// # Safety
///
/// `p` is null or a NUL-terminated string that stays valid during the call.
unsafe fn borrowed_string(p: *const c_char) -> Option<String> {
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

pub fn library_version() -> String {
    // SAFETY: DuckDB returns a static string.
    unsafe { borrowed_string(ffi::duckdb_library_version()) }.unwrap_or_default()
}

fn c_string(s: &str) -> Result<CString> {
    CString::new(s).map_err(|_| anyhow!("the text contains a NUL character"))
}

/// One open database: a file, or memory.
pub struct Database {
    db: ffi::duckdb_database,
}

// SAFETY: a DuckDB database handle may be shared between threads; connections are what each
// thread uses.
unsafe impl Send for Database {}
unsafe impl Sync for Database {}

impl Database {
    /// Opens `path` (`None` for memory) with the given settings.
    pub fn open(path: Option<&str>, settings: &[(&str, &str)]) -> Result<Arc<Self>> {
        let path = path.map(c_string).transpose()?;
        // SAFETY: every handle is created and destroyed here, and the strings outlive the calls.
        unsafe {
            let mut config: ffi::duckdb_config = ptr::null_mut();
            if ffi::duckdb_create_config(&mut config) != ffi::DuckDBSuccess {
                bail!("could not create a DuckDB configuration");
            }
            for (key, value) in settings {
                let (k, v) = (c_string(key)?, c_string(value)?);
                if ffi::duckdb_set_config(config, k.as_ptr(), v.as_ptr()) != ffi::DuckDBSuccess {
                    ffi::duckdb_destroy_config(&mut config);
                    bail!("DuckDB refused the setting {key} = {value}");
                }
            }
            let mut db: ffi::duckdb_database = ptr::null_mut();
            let mut error: *mut c_char = ptr::null_mut();
            let path_ptr = path.as_ref().map_or(ptr::null(), |p| p.as_ptr());
            let state = ffi::duckdb_open_ext(path_ptr, &mut db, config, &mut error);
            ffi::duckdb_destroy_config(&mut config);
            let message = owned_string(error);
            if state != ffi::DuckDBSuccess {
                bail!("{}", message.unwrap_or_else(|| "DuckDB could not open the database".into()));
            }
            Ok(Arc::new(Self { db }))
        }
    }

    pub fn connect(self: &Arc<Self>) -> Result<Conn> {
        let mut con: ffi::duckdb_connection = ptr::null_mut();
        // SAFETY: `db` is open while `self` lives, and the connection keeps `self` alive.
        if unsafe { ffi::duckdb_connect(self.db, &mut con) } != ffi::DuckDBSuccess {
            bail!("DuckDB could not open a connection");
        }
        Ok(Conn { con, _db: self.clone() })
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        // SAFETY: every connection holds an `Arc<Database>`, so none is left.
        unsafe { ffi::duckdb_close(&mut self.db) };
    }
}

/// One connection, used by one thread at a time.
pub struct Conn {
    con: ffi::duckdb_connection,
    _db: Arc<Database>,
}

// SAFETY: a connection may move between threads; it is not `Sync`, so one thread uses it at a
// time.
unsafe impl Send for Conn {}

impl Drop for Conn {
    fn drop(&mut self) {
        // SAFETY: statements and results borrow `self`, so none is alive.
        unsafe { ffi::duckdb_disconnect(&mut self.con) };
    }
}

/// Interrupts a connection's running statement from another thread.
#[derive(Clone, Copy)]
pub struct Interrupt(ffi::duckdb_connection);

// SAFETY: `duckdb_interrupt` only sets a flag on the connection and may be called from any thread.
unsafe impl Send for Interrupt {}
unsafe impl Sync for Interrupt {}

impl Interrupt {
    /// # Safety
    ///
    /// The connection must still be open.
    pub unsafe fn interrupt(self) {
        unsafe { ffi::duckdb_interrupt(self.0) };
    }
}

impl Conn {
    pub fn interrupt_handle(&self) -> Interrupt {
        Interrupt(self.con)
    }

    /// Splits `sql` into statements without running any.
    pub fn extract(&self, sql: &str) -> Result<Extracted<'_>> {
        let text = c_string(sql)?;
        let mut extracted: ffi::duckdb_extracted_statements = ptr::null_mut();
        // SAFETY: `extracted` is destroyed by `Extracted`'s drop, even when the count is 0.
        let count = unsafe { ffi::duckdb_extract_statements(self.con, text.as_ptr(), &mut extracted) };
        let extracted = Extracted { con: self.con, extracted, count, _conn: PhantomData };
        if count == 0 {
            // SAFETY: `extracted` is live.
            let error = unsafe { borrowed_string(ffi::duckdb_extract_statements_error(extracted.extracted)) };
            if let Some(e) = error {
                bail!("{e}");
            }
        }
        Ok(extracted)
    }

    pub fn prepare(&self, sql: &str) -> Result<Prepared<'_>> {
        let text = c_string(sql)?;
        let mut stmt: ffi::duckdb_prepared_statement = ptr::null_mut();
        // SAFETY: `Prepared::checked` destroys `stmt` on every path.
        let state = unsafe { ffi::duckdb_prepare(self.con, text.as_ptr(), &mut stmt) };
        Prepared::checked(stmt, state)
    }

    /// Runs `sql`, which may hold several statements, and discards any rows.
    pub fn execute(&self, sql: &str) -> Result<()> {
        let text = c_string(sql)?;
        // SAFETY: the result is destroyed on every path.
        unsafe {
            let mut result: ffi::duckdb_result = std::mem::zeroed();
            let state = ffi::duckdb_query(self.con, text.as_ptr(), &mut result);
            let error = (state != ffi::DuckDBSuccess).then(|| result_error(&mut result));
            ffi::duckdb_destroy_result(&mut result);
            match error {
                Some(e) => bail!("{e}"),
                None => Ok(()),
            }
        }
    }
}

/// The statements of one SQL text, parsed but not run.
pub struct Extracted<'c> {
    con: ffi::duckdb_connection,
    extracted: ffi::duckdb_extracted_statements,
    count: u64,
    _conn: PhantomData<&'c Conn>,
}

impl Extracted<'_> {
    pub fn len(&self) -> usize {
        self.count as usize
    }

    pub fn prepare(&self, index: usize) -> Result<Prepared<'_>> {
        let mut stmt: ffi::duckdb_prepared_statement = ptr::null_mut();
        // SAFETY: `index` is checked against the count, and `Prepared::checked` destroys `stmt`.
        if index >= self.len() {
            bail!("there is no statement {index}");
        }
        let state =
            unsafe { ffi::duckdb_prepare_extracted_statement(self.con, self.extracted, index as u64, &mut stmt) };
        Prepared::checked(stmt, state)
    }
}

impl Drop for Extracted<'_> {
    fn drop(&mut self) {
        // SAFETY: created by `duckdb_extract_statements` and destroyed only here.
        unsafe { ffi::duckdb_destroy_extracted(&mut self.extracted) };
    }
}

/// What kind of statement DuckDB parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementType {
    /// SELECT, and also SHOW, DESCRIBE, SUMMARIZE, PIVOT and some PRAGMAs.
    Select,
    Explain,
    Other,
}

pub struct Prepared<'c> {
    stmt: ffi::duckdb_prepared_statement,
    _conn: PhantomData<&'c Conn>,
}

impl Prepared<'_> {
    fn checked<'a>(mut stmt: ffi::duckdb_prepared_statement, state: ffi::duckdb_state) -> Result<Prepared<'a>> {
        if state == ffi::DuckDBSuccess {
            return Ok(Prepared { stmt, _conn: PhantomData });
        }
        // SAFETY: the statement must be destroyed even when preparing failed.
        let error = unsafe { borrowed_string(ffi::duckdb_prepare_error(stmt)) };
        unsafe { ffi::duckdb_destroy_prepare(&mut stmt) };
        bail!("{}", error.unwrap_or_else(|| "DuckDB could not prepare the statement".into()))
    }

    pub fn statement_type(&self) -> StatementType {
        // SAFETY: `stmt` is a live prepared statement.
        match unsafe { ffi::duckdb_prepared_statement_type(self.stmt) } {
            ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_SELECT => StatementType::Select,
            ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_EXPLAIN => StatementType::Explain,
            _ => StatementType::Other,
        }
    }

    /// The result's columns as DuckDB knows them before running.
    pub fn columns(&self) -> Vec<(String, LogicalType)> {
        // SAFETY: indexes stay below the column count; names are freed by `owned_string`.
        unsafe {
            let n = ffi::duckdb_prepared_statement_column_count(self.stmt);
            (0..n)
                .map(|i| {
                    let name = owned_string(ffi::duckdb_prepared_statement_column_name(self.stmt, i).cast_mut());
                    let ty = LogicalType(ffi::duckdb_prepared_statement_column_logical_type(self.stmt, i));
                    (name.unwrap_or_default(), ty)
                })
                .collect()
        }
    }

    /// Runs the statement, producing rows while they are read instead of all at once.
    pub fn stream(&mut self) -> Result<QueryResult<'_>> {
        // SAFETY: `QueryResult` destroys the result on every path. This is the C API's only
        // streaming entry point; it is marked deprecated without a replacement yet.
        unsafe {
            let mut result: ffi::duckdb_result = std::mem::zeroed();
            #[allow(deprecated)]
            let state = ffi::duckdb_execute_prepared_streaming(self.stmt, &mut result);
            QueryResult::checked(result, state)
        }
    }

    /// Runs the statement to completion, holding every row in memory.
    pub fn materialize(&mut self) -> Result<QueryResult<'_>> {
        // SAFETY: as in `stream`.
        unsafe {
            let mut result: ffi::duckdb_result = std::mem::zeroed();
            let state = ffi::duckdb_execute_prepared(self.stmt, &mut result);
            QueryResult::checked(result, state)
        }
    }
}

impl Drop for Prepared<'_> {
    fn drop(&mut self) {
        // SAFETY: results borrow the statement, so none is alive.
        unsafe { ffi::duckdb_destroy_prepare(&mut self.stmt) };
    }
}

fn result_error(result: &mut ffi::duckdb_result) -> String {
    // SAFETY: `result` is a result DuckDB filled in.
    unsafe { borrowed_string(ffi::duckdb_result_error(result)) }.unwrap_or_else(|| "DuckDB reported an error".into())
}

/// What a statement produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultKind {
    Rows,
    Changed(u64),
    Nothing,
}

pub struct QueryResult<'s> {
    result: ffi::duckdb_result,
    _stmt: PhantomData<&'s mut ()>,
}

impl QueryResult<'_> {
    unsafe fn checked<'a>(mut result: ffi::duckdb_result, state: ffi::duckdb_state) -> Result<QueryResult<'a>> {
        if state == ffi::DuckDBSuccess {
            return Ok(QueryResult { result, _stmt: PhantomData });
        }
        let error = result_error(&mut result);
        unsafe { ffi::duckdb_destroy_result(&mut result) };
        bail!("{error}")
    }

    pub fn kind(&mut self) -> ResultKind {
        // SAFETY: `result` is live.
        unsafe {
            match ffi::duckdb_result_return_type(self.result) {
                ffi::duckdb_result_type_DUCKDB_RESULT_TYPE_CHANGED_ROWS => {
                    ResultKind::Changed(ffi::duckdb_rows_changed(&mut self.result))
                }
                ffi::duckdb_result_type_DUCKDB_RESULT_TYPE_NOTHING => ResultKind::Nothing,
                _ => ResultKind::Rows,
            }
        }
    }

    pub fn columns(&mut self) -> Vec<(String, LogicalType)> {
        // SAFETY: indexes stay below the column count; DuckDB owns the names.
        unsafe {
            let n = ffi::duckdb_column_count(&mut self.result);
            (0..n)
                .map(|i| {
                    let name = borrowed_string(ffi::duckdb_column_name(&mut self.result, i)).unwrap_or_default();
                    (name, LogicalType(ffi::duckdb_column_logical_type(&mut self.result, i)))
                })
                .collect()
        }
    }

    /// The next chunk of rows, `None` at the end. An interrupted or failed statement is an error.
    pub fn next_chunk(&mut self) -> Result<Option<Chunk<'_>>> {
        // SAFETY: the chunk is destroyed by `Chunk`'s drop and borrows this result.
        let chunk = unsafe { ffi::duckdb_fetch_chunk(self.result) };
        if !chunk.is_null() {
            return Ok(Some(Chunk { chunk, _result: PhantomData }));
        }
        // SAFETY: `result` is live; a null message means the result simply ended.
        match unsafe { borrowed_string(ffi::duckdb_result_error(&mut self.result)) } {
            Some(e) => bail!("{e}"),
            None => Ok(None),
        }
    }

    /// Cell access for materialized results. Never mixed with [`Self::next_chunk`] on one result.
    pub fn cells(&mut self) -> Cells<'_> {
        // SAFETY: `result` is a materialized result.
        #[allow(deprecated)]
        let rows = unsafe { ffi::duckdb_row_count(&mut self.result) };
        Cells { result: &mut self.result, rows }
    }
}

impl Drop for QueryResult<'_> {
    fn drop(&mut self) {
        // SAFETY: chunks borrow the result, so none is alive.
        unsafe { ffi::duckdb_destroy_result(&mut self.result) };
    }
}

/// Per-cell access to a materialized result, through the C API's older value functions. Used
/// only for results that can't be cast to text by wrapping the statement, such as
/// `INSERT … RETURNING`.
pub struct Cells<'r> {
    result: &'r mut ffi::duckdb_result,
    rows: u64,
}

impl Cells<'_> {
    pub fn rows(&self) -> u64 {
        self.rows
    }

    pub fn is_null(&mut self, col: u64, row: u64) -> bool {
        // SAFETY: callers stay inside the result's bounds.
        #[allow(deprecated)]
        unsafe {
            ffi::duckdb_value_is_null(self.result, col, row)
        }
    }

    /// DuckDB's text for the cell, or `None` for types the old functions can't render.
    pub fn text(&mut self, col: u64, row: u64) -> Option<String> {
        // SAFETY: as above; the string is freed by `owned_string`.
        #[allow(deprecated)]
        unsafe {
            owned_string(ffi::duckdb_value_varchar(self.result, col, row))
        }
    }

    pub fn blob(&mut self, col: u64, row: u64) -> Vec<u8> {
        // SAFETY: as above; DuckDB allocates `data`, which is copied and freed here.
        #[allow(deprecated)]
        unsafe {
            let blob = ffi::duckdb_value_blob(self.result, col, row);
            if blob.data.is_null() {
                return Vec::new();
            }
            let bytes = std::slice::from_raw_parts(blob.data.cast::<u8>(), blob.size as usize).to_vec();
            ffi::duckdb_free(blob.data);
            bytes
        }
    }
}

pub struct Chunk<'r> {
    chunk: ffi::duckdb_data_chunk,
    _result: PhantomData<&'r ()>,
}

impl Chunk<'_> {
    pub fn len(&self) -> usize {
        // SAFETY: `chunk` is live.
        unsafe { ffi::duckdb_data_chunk_get_size(self.chunk) as usize }
    }

    pub fn column(&self, index: usize) -> Column<'_> {
        // SAFETY: the vector lives as long as the chunk.
        unsafe {
            let vector = ffi::duckdb_data_chunk_get_vector(self.chunk, index as u64);
            Column {
                data: ffi::duckdb_vector_get_data(vector).cast_const(),
                validity: ffi::duckdb_vector_get_validity(vector),
                _chunk: PhantomData,
            }
        }
    }
}

impl Drop for Chunk<'_> {
    fn drop(&mut self) {
        // SAFETY: columns borrow the chunk, so none is alive.
        unsafe { ffi::duckdb_destroy_data_chunk(&mut self.chunk) };
    }
}

/// One column of a chunk, in DuckDB's in-memory layout.
pub struct Column<'c> {
    data: *const std::ffi::c_void,
    validity: *mut u64,
    _chunk: PhantomData<&'c ()>,
}

/// Fixed-width types a column can be read as directly.
pub trait Primitive: Copy {}
impl Primitive for bool {}
impl Primitive for i8 {}
impl Primitive for i16 {}
impl Primitive for i32 {}
impl Primitive for i64 {}
impl Primitive for u8 {}
impl Primitive for u16 {}
impl Primitive for u32 {}
impl Primitive for u64 {}
impl Primitive for f32 {}
impl Primitive for f64 {}

impl Column<'_> {
    pub fn is_valid(&self, row: usize) -> bool {
        // SAFETY: a null mask means every row is valid; otherwise `row` is below the chunk size.
        self.validity.is_null() || unsafe { ffi::duckdb_validity_row_is_valid(self.validity, row as u64) }
    }

    /// # Safety
    ///
    /// `T` must be the column's physical type, and `row` below the chunk's length.
    pub unsafe fn get<T: Primitive>(&self, row: usize) -> T {
        unsafe { *self.data.cast::<T>().add(row) }
    }

    /// The bytes of a VARCHAR or BLOB cell.
    ///
    /// # Safety
    ///
    /// The column must be VARCHAR or BLOB, and `row` below the chunk's length.
    pub unsafe fn bytes(&self, row: usize) -> &[u8] {
        unsafe {
            let s = self.data.cast::<ffi::duckdb_string_t>().add(row).cast_mut();
            let len = ffi::duckdb_string_t_length(*s) as usize;
            let data = ffi::duckdb_string_t_data(s);
            std::slice::from_raw_parts(data.cast::<u8>(), len)
        }
    }
}

/// A column's DuckDB type.
pub struct LogicalType(ffi::duckdb_logical_type);

impl Drop for LogicalType {
    fn drop(&mut self) {
        // SAFETY: owned by this wrapper.
        unsafe { ffi::duckdb_destroy_logical_type(&mut self.0) };
    }
}

impl LogicalType {
    pub fn id(&self) -> ffi::DUCKDB_TYPE {
        // SAFETY: the type is live.
        unsafe { ffi::duckdb_get_type_id(self.0) }
    }

    /// The type as DuckDB spells it, such as `DECIMAL(12,2)`, `VARCHAR[]` or `JSON`.
    pub fn name(&self) -> String {
        // SAFETY: the type is live, child types are wrapped so they are destroyed, and names are
        // freed by `owned_string`.
        unsafe {
            if let Some(alias) = owned_string(ffi::duckdb_logical_type_get_alias(self.0)) {
                return alias;
            }
            let child = |t| LogicalType(t);
            match self.id() {
                ffi::DUCKDB_TYPE_DUCKDB_TYPE_DECIMAL => {
                    format!("DECIMAL({},{})", ffi::duckdb_decimal_width(self.0), ffi::duckdb_decimal_scale(self.0))
                }
                ffi::DUCKDB_TYPE_DUCKDB_TYPE_LIST => {
                    format!("{}[]", child(ffi::duckdb_list_type_child_type(self.0)).name())
                }
                ffi::DUCKDB_TYPE_DUCKDB_TYPE_ARRAY => format!(
                    "{}[{}]",
                    child(ffi::duckdb_array_type_child_type(self.0)).name(),
                    ffi::duckdb_array_type_array_size(self.0)
                ),
                ffi::DUCKDB_TYPE_DUCKDB_TYPE_MAP => format!(
                    "MAP({}, {})",
                    child(ffi::duckdb_map_type_key_type(self.0)).name(),
                    child(ffi::duckdb_map_type_value_type(self.0)).name()
                ),
                ffi::DUCKDB_TYPE_DUCKDB_TYPE_STRUCT => {
                    let fields: Vec<String> = (0..ffi::duckdb_struct_type_child_count(self.0))
                        .map(|i| {
                            let name = owned_string(ffi::duckdb_struct_type_child_name(self.0, i)).unwrap_or_default();
                            format!("{name} {}", child(ffi::duckdb_struct_type_child_type(self.0, i)).name())
                        })
                        .collect();
                    format!("STRUCT({})", fields.join(", "))
                }
                ffi::DUCKDB_TYPE_DUCKDB_TYPE_UNION => {
                    let members: Vec<String> = (0..ffi::duckdb_union_type_member_count(self.0))
                        .map(|i| {
                            let name = owned_string(ffi::duckdb_union_type_member_name(self.0, i)).unwrap_or_default();
                            format!("{name} {}", child(ffi::duckdb_union_type_member_type(self.0, i)).name())
                        })
                        .collect();
                    format!("UNION({})", members.join(", "))
                }
                id => base_name(id).to_string(),
            }
        }
    }
}

fn base_name(id: ffi::DUCKDB_TYPE) -> &'static str {
    match id {
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_BOOLEAN => "BOOLEAN",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_TINYINT => "TINYINT",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_SMALLINT => "SMALLINT",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_INTEGER => "INTEGER",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGINT => "BIGINT",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_UTINYINT => "UTINYINT",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_USMALLINT => "USMALLINT",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_UINTEGER => "UINTEGER",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_UBIGINT => "UBIGINT",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_FLOAT => "FLOAT",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_DOUBLE => "DOUBLE",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP => "TIMESTAMP",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_DATE => "DATE",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIME => "TIME",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_INTERVAL => "INTERVAL",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_HUGEINT => "HUGEINT",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_UHUGEINT => "UHUGEINT",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_VARCHAR => "VARCHAR",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_BLOB => "BLOB",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_S => "TIMESTAMP_S",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_MS => "TIMESTAMP_MS",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_NS => "TIMESTAMP_NS",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_ENUM => "ENUM",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_UUID => "UUID",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIT => "BIT",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIME_TZ => "TIME WITH TIME ZONE",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_TZ => "TIMESTAMP WITH TIME ZONE",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGNUM => "BIGNUM",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_SQLNULL => "NULL",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIME_NS => "TIME_NS",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_GEOMETRY => "GEOMETRY",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_VARIANT => "VARIANT",
        _ => "UNKNOWN",
    }
}
