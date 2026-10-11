//! Writable `turbovec0` virtual table with chunked SQLite-owned persistence.

use std::borrow::Cow;
use std::cell::Cell;
use std::ffi::{CStr, c_char, c_int};
use std::io::{self, Read, Write};
use std::marker::PhantomData;
use std::mem::size_of;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::{Mutex, OnceLock};

use rusqlite::blob::Blob;
use rusqlite::ffi;
use rusqlite::types::{Null, ValueRef};
use rusqlite::vtab::{
    ConflictMode, Context, CreateVTab, Filters, IndexConstraintOp, IndexFlags, IndexInfo, Inserts,
    Module, TransactionVTab, UpdateVTab, Updates, VTab, VTabConfig, VTabConnection, VTabCursor,
    VTabKind, escape_double_quote, parameter,
};
use rusqlite::{Connection, Error, OptionalExtension, Result, Statement, params};
use turbovec::IdMapIndex;

use crate::{TURBOVEC_FORMAT_REVISION, TURBOVEC_FORMAT_VERSION, check_format, parse_vector};

const MODULE_NAME: &CStr = c"turbovec0";
/// Shadow tables a `turbovec0` table may own. `meta` is the 0.2.0 metadata
/// table; an upgraded index keeps it empty, and a new one never creates it.
const SHADOW_SUFFIXES: [&str; 4] = ["meta", "info", "ids", "chunks"];
/// The `_info` layout this build reads and writes. Raise it when a change
/// must make this build refuse an index.
const STORAGE_FORMAT: i64 = 1;
const CHUNK_SIZE: usize = 4 * 1024 * 1024;
const DELTA_MAGIC: &[u8; 4] = b"TVD1";
const DELTA_HEADER_LEN: usize = 12;
const DELTA_CHECKSUM_LEN: usize = 4;
const MIN_DELTA_BASE_BYTES: usize = 1024 * 1024;
const MIN_COMPACTION_DELTA_BYTES: usize = 16 * 1024 * 1024;
/// Planner fallback when the warm row count is unavailable.
const DEFAULT_ESTIMATED_ROWS: i64 = 1_000_000;
/// Planner fallback when a KNN plan's `k` is not known until execution. A top-k
/// plan returns few rows either way, so this stays small rather than borrowing
/// the full-scan estimate.
const DEFAULT_KNN_ROWS: i64 = 10;

const COL_EMBEDDING: c_int = 0;
const COL_SCORE: c_int = 1;
const COL_K: c_int = 2;

const PLAN_FULL_SCAN: c_int = 1;
const PLAN_ROWID: c_int = 2;
const PLAN_KNN: c_int = 3;
const PLAN_KIND_MASK: c_int = 0x0f;
const PLAN_HAS_K: c_int = 0x10;
const PLAN_HAS_LIMIT: c_int = 0x20;
const PLAN_HAS_OFFSET: c_int = 0x40;
const PLAN_HAS_ROWID_FILTER: c_int = 0x80;
const PLAN_ROWID_FILTER_IN: c_int = 0x100;

/// Lowest host SQLite that exposes every API-table slot `turbovec0` reads. The
/// project declares 3.44, but this is the version the module actually cannot
/// proceed without, so it is the version the error names.
const MIN_HOST_VERSION: c_int = 3_038_000;

// sqlite3_api_routines pointer slots from SQLite's stable sqlite3ext.h ABI.
// The IN callbacks were appended in SQLite 3.38; this project supports 3.44+.
const API_LIBVERSION_NUMBER: usize = 67;
const API_VTAB_CONFIG: usize = 177;
const API_VTAB_RHS_VALUE: usize = 257;
const API_VTAB_IN: usize = 259;
const API_VTAB_IN_FIRST: usize = 260;
const API_VTAB_IN_NEXT: usize = 261;

type BestIndexCallback =
    unsafe extern "C" fn(*mut ffi::sqlite3_vtab, *mut ffi::sqlite3_index_info) -> c_int;
type FilterCallback = unsafe extern "C" fn(
    *mut ffi::sqlite3_vtab_cursor,
    c_int,
    *const c_char,
    c_int,
    *mut *mut ffi::sqlite3_value,
) -> c_int;
type IntegrityCallback = unsafe extern "C" fn(
    *mut ffi::sqlite3_vtab,
    *const c_char,
    *const c_char,
    c_int,
    *mut *mut c_char,
) -> c_int;
type VtabConfigCallback = unsafe extern "C" fn(*mut ffi::sqlite3, c_int, ...) -> c_int;
type VtabInCallback = unsafe extern "C" fn(*mut ffi::sqlite3_index_info, c_int, c_int) -> c_int;
type VtabRhsValueCallback = unsafe extern "C" fn(
    *mut ffi::sqlite3_index_info,
    c_int,
    *mut *mut ffi::sqlite3_value,
) -> c_int;
type VtabInIterCallback =
    unsafe extern "C" fn(*mut ffi::sqlite3_value, *mut *mut ffi::sqlite3_value) -> c_int;

#[repr(C)]
struct ModuleV4 {
    module: ffi::sqlite3_module,
    x_integrity: Option<IntegrityCallback>,
}

// `ModuleV4` only places xIntegrity where SQLite reads it while Rusqlite's
// `sqlite3_module` still stops at xShadowName (the iVersion 3 layout): 1 padded
// int plus 23 callback pointers. libsqlite3-sys arrives transitively, so a
// minor bump could grow the struct and silently turn the appended field into
// dead bytes — `PRAGMA integrity_check` would stop checking turbovec0 with no
// diagnostic at all. Fail the build instead.
const _: () = assert!(
    std::mem::offset_of!(ModuleV4, x_integrity) == 24 * size_of::<usize>(),
    "sqlite3_module layout changed; xIntegrity is no longer the 24th slot"
);

static ORIGINAL_BEST_INDEX: OnceLock<usize> = OnceLock::new();
static ORIGINAL_FILTER: OnceLock<usize> = OnceLock::new();
static HOST_VERSION: OnceLock<c_int> = OnceLock::new();
static VTAB_CONFIG: OnceLock<usize> = OnceLock::new();
static VTAB_RHS_VALUE: OnceLock<usize> = OnceLock::new();
static VTAB_IN: OnceLock<usize> = OnceLock::new();
static VTAB_IN_FIRST: OnceLock<usize> = OnceLock::new();
static VTAB_IN_NEXT: OnceLock<usize> = OnceLock::new();

thread_local! {
    static CURRENT_INDEX_INFO: Cell<*mut ffi::sqlite3_index_info> = const { Cell::new(ptr::null_mut()) };
    static CURRENT_FILTER_ARGS: Cell<(*mut *mut ffi::sqlite3_value, c_int)> = const { Cell::new((ptr::null_mut(), 0)) };
}

/// Capture the post-3.38 virtual-table callbacks without compiling every
/// SQLite call against the build machine's newest API table.
pub(crate) unsafe fn initialize_api(api: *mut ffi::sqlite3_api_routines) {
    if api.is_null() {
        return;
    }
    let slots = api.cast::<usize>();
    let version_pointer = unsafe { *slots.add(API_LIBVERSION_NUMBER) };
    if version_pointer == 0 {
        return;
    }
    let version: unsafe extern "C" fn() -> c_int = unsafe { std::mem::transmute(version_pointer) };
    let host_version = unsafe { version() };
    let _ = HOST_VERSION.set(host_version);
    if host_version < MIN_HOST_VERSION {
        return;
    }
    for (slot, destination) in [
        (API_VTAB_CONFIG, &VTAB_CONFIG),
        (API_VTAB_RHS_VALUE, &VTAB_RHS_VALUE),
        (API_VTAB_IN, &VTAB_IN),
        (API_VTAB_IN_FIRST, &VTAB_IN_FIRST),
        (API_VTAB_IN_NEXT, &VTAB_IN_NEXT),
    ] {
        let pointer = unsafe { *slots.add(slot) };
        if pointer != 0 {
            let _ = destination.set(pointer);
        }
    }
}

pub(crate) fn register(connection: &Connection) -> Result<()> {
    let result = unsafe {
        ffi::sqlite3_create_module_v2(
            connection.handle(),
            MODULE_NAME.as_ptr(),
            raw_module(),
            ptr::null_mut(),
            None,
        )
    };
    if result == ffi::SQLITE_OK {
        Ok(())
    } else {
        Err(Error::SqliteFailure(ffi::Error::new(result), None))
    }
}

fn raw_module() -> *const ffi::sqlite3_module {
    static MODULE_POINTER: OnceLock<usize> = OnceLock::new();
    let pointer = *MODULE_POINTER.get_or_init(|| {
        const RUSQLITE_MODULE: Module<TurboVecTable> = Module::update_module_with_tx();
        // Module is repr(transparent) over sqlite3_module. Copying it lets us
        // fill the one callback Rusqlite does not yet expose in its builder.
        let mut module = unsafe {
            *(&RUSQLITE_MODULE as *const Module<TurboVecTable>).cast::<ffi::sqlite3_module>()
        };
        module.iVersion = 4;
        module.xRename = Some(rename);
        module.xSavepoint = Some(savepoint);
        module.xRelease = Some(release);
        module.xRollbackTo = Some(rollback_to);
        module.xShadowName = Some(shadow_name);
        let original_best_index = module.xBestIndex.expect("Rusqlite xBestIndex callback");
        let original_filter = module.xFilter.expect("Rusqlite xFilter callback");
        let _ = ORIGINAL_BEST_INDEX.set(original_best_index as usize);
        let _ = ORIGINAL_FILTER.set(original_filter as usize);
        module.xBestIndex = Some(best_index_with_raw_info);
        module.xFilter = Some(filter_with_raw_args);
        let module = ModuleV4 {
            module,
            x_integrity: Some(integrity),
        };
        Box::into_raw(Box::new(module)) as usize
    });
    pointer as *const ffi::sqlite3_module
}

unsafe extern "C" fn best_index_with_raw_info(
    table: *mut ffi::sqlite3_vtab,
    info: *mut ffi::sqlite3_index_info,
) -> c_int {
    let previous = CURRENT_INDEX_INFO.with(|current| current.replace(info));
    let callback: BestIndexCallback = unsafe {
        std::mem::transmute(
            *ORIGINAL_BEST_INDEX
                .get()
                .expect("original xBestIndex callback"),
        )
    };
    let result = unsafe { callback(table, info) };
    CURRENT_INDEX_INFO.with(|current| current.set(previous));
    result
}

unsafe extern "C" fn filter_with_raw_args(
    cursor: *mut ffi::sqlite3_vtab_cursor,
    plan: c_int,
    index_string: *const c_char,
    argument_count: c_int,
    arguments: *mut *mut ffi::sqlite3_value,
) -> c_int {
    let previous = CURRENT_FILTER_ARGS.with(|current| current.replace((arguments, argument_count)));
    let callback: FilterCallback =
        unsafe { std::mem::transmute(*ORIGINAL_FILTER.get().expect("original xFilter callback")) };
    let result = unsafe { callback(cursor, plan, index_string, argument_count, arguments) };
    CURRENT_FILTER_ARGS.with(|current| current.set(previous));
    result
}

fn current_index_info() -> Result<*mut ffi::sqlite3_index_info> {
    CURRENT_INDEX_INFO.with(|current| {
        let info = current.get();
        (!info.is_null())
            .then_some(info)
            .ok_or_else(|| error("turbovec0 planner callback is unavailable"))
    })
}

fn current_filter_argument(index: usize) -> Result<*mut ffi::sqlite3_value> {
    CURRENT_FILTER_ARGS.with(|current| {
        let (arguments, count) = current.get();
        if arguments.is_null() || index >= usize::try_from(count).unwrap_or(0) {
            return Err(error(format!(
                "turbovec0 filter argument {index} is unavailable"
            )));
        }
        Ok(unsafe { *arguments.add(index) })
    })
}

fn vtab_in(info: *mut ffi::sqlite3_index_info, index: usize, mode: c_int) -> Result<bool> {
    let pointer = *VTAB_IN
        .get()
        .ok_or_else(|| error("turbovec0 rowid IN pushdown requires SQLite 3.38 or newer"))?;
    let callback: VtabInCallback = unsafe { std::mem::transmute(pointer) };
    Ok(unsafe { callback(info, index as c_int, mode) } != 0)
}

/// The top-k a KNN plan will actually produce, when SQLite can tell us at plan
/// time. A hidden `k=n` caps the search; otherwise LIMIT does. Both are often
/// literals or already-bound values, and anything unavailable yields `None`
/// rather than a guess.
fn planned_k(k: Option<usize>, limit: Option<usize>) -> Option<i64> {
    let value = |constraint: Option<usize>| -> Option<i64> {
        rhs_integer(current_index_info().ok()?, constraint?)
    };
    match (value(k), value(limit)) {
        (Some(k), Some(limit)) => Some(k.min(limit)),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
}

/// A constraint's non-negative integer right-hand value, when SQLite knows it
/// during planning. Planner-only, so every failure — an old host, a value not
/// yet known, a non-integer — collapses to `None` instead of failing the
/// statement.
fn rhs_integer(info: *mut ffi::sqlite3_index_info, index: usize) -> Option<i64> {
    let pointer = *VTAB_RHS_VALUE.get()?;
    let callback: VtabRhsValueCallback = unsafe { std::mem::transmute(pointer) };
    let mut value = ptr::null_mut();
    if unsafe { callback(info, index as c_int, &mut value) } != ffi::SQLITE_OK || value.is_null() {
        return None;
    }
    // SAFETY: SQLite owns this value and keeps it valid for the duration of the
    // xBestIndex call this runs inside, which is the only place it is read.
    let value = unsafe {
        if ffi::sqlite3_value_type(value) != ffi::SQLITE_INTEGER {
            return None;
        }
        ffi::sqlite3_value_int64(value)
    };
    (value >= 0).then_some(value)
}

fn append_in_values(list: *mut ffi::sqlite3_value, ids: &mut Vec<u64>) -> Result<()> {
    let first_pointer = *VTAB_IN_FIRST
        .get()
        .ok_or_else(|| error("turbovec0 rowid IN pushdown requires SQLite 3.38 or newer"))?;
    let next_pointer = *VTAB_IN_NEXT
        .get()
        .ok_or_else(|| error("turbovec0 rowid IN pushdown requires SQLite 3.38 or newer"))?;
    let first: VtabInIterCallback = unsafe { std::mem::transmute(first_pointer) };
    let next: VtabInIterCallback = unsafe { std::mem::transmute(next_pointer) };
    let mut value = ptr::null_mut();
    let mut result = unsafe { first(list, &mut value) };
    while result == ffi::SQLITE_OK {
        match unsafe { ffi::sqlite3_value_type(value) } {
            ffi::SQLITE_INTEGER => {
                let rowid = unsafe { ffi::sqlite3_value_int64(value) };
                if let Ok(id) = u64::try_from(rowid) {
                    ids.push(id);
                }
            }
            ffi::SQLITE_NULL => {}
            _ => return Err(error("rowid allowlist values must be SQLite INTEGERs")),
        }
        result = unsafe { next(list, &mut value) };
    }
    if result == ffi::SQLITE_DONE {
        Ok(())
    } else {
        Err(sqlite_error(
            result,
            "cannot read turbovec0 rowid IN values",
        ))
    }
}

unsafe extern "C" fn shadow_name(name: *const c_char) -> c_int {
    if name.is_null() {
        return 0;
    }
    let name = unsafe { CStr::from_ptr(name) }.to_bytes();
    c_int::from(
        SHADOW_SUFFIXES
            .iter()
            .any(|suffix| name.eq_ignore_ascii_case(suffix.as_bytes())),
    )
}

unsafe fn set_vtab_error(table: *mut ffi::sqlite3_vtab, message: &str) -> c_int {
    // Most callbacks reach SQLite through sqlite3VtabImportErrmsg(), which
    // frees and clears zErrMsg for us. sqlite3VtabSavepoint() does not: it
    // returns the code and never looks at the message, so a failing
    // xSavepoint/xRelease/xRollbackTo leaves this allocation owned by nobody.
    // Release whatever is there before overwriting the pointer, otherwise the
    // second failure on one table leaks the first message too.
    unsafe {
        ffi::sqlite3_free((*table).zErrMsg.cast());
        (*table).zErrMsg = ptr::null_mut();
    }
    let length = message.len().saturating_add(1);
    let allocation = unsafe { ffi::sqlite3_malloc64(length as u64) }.cast::<u8>();
    if allocation.is_null() {
        return ffi::SQLITE_NOMEM;
    }
    unsafe {
        ptr::copy_nonoverlapping(message.as_ptr(), allocation, message.len());
        *allocation.add(message.len()) = 0;
        (*table).zErrMsg = allocation.cast();
    }
    ffi::SQLITE_ERROR
}

unsafe fn callback_result(table: *mut ffi::sqlite3_vtab, result: Result<()>) -> c_int {
    match result {
        Ok(()) => ffi::SQLITE_OK,
        Err(cause) => unsafe { set_vtab_error(table, &cause.to_string()) },
    }
}

unsafe extern "C" fn rename(table: *mut ffi::sqlite3_vtab, new_name: *const c_char) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        if new_name.is_null() {
            return unsafe { set_vtab_error(table, "new turbovec0 name is null") };
        }
        let new_name = match unsafe { CStr::from_ptr(new_name) }.to_str() {
            Ok(name) => name,
            Err(cause) => return unsafe { set_vtab_error(table, &cause.to_string()) },
        };
        let table_ref = unsafe { &mut *table.cast::<TurboVecTable>() };
        unsafe { callback_result(table, table_ref.rename(new_name)) }
    }))
    .unwrap_or_else(|_| unsafe { set_vtab_error(table, "panic in turbovec0 xRename") })
}

unsafe extern "C" fn savepoint(table: *mut ffi::sqlite3_vtab, id: c_int) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        let table_ref = unsafe { &mut *table.cast::<TurboVecTable>() };
        let result = table_ref.savepoint(id);
        unsafe { callback_result(table, result) }
    }))
    .unwrap_or(ffi::SQLITE_ERROR)
}

unsafe extern "C" fn release(table: *mut ffi::sqlite3_vtab, id: c_int) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        let table_ref = unsafe { &mut *table.cast::<TurboVecTable>() };
        let result = table_ref.release(id);
        unsafe { callback_result(table, result) }
    }))
    .unwrap_or(ffi::SQLITE_ERROR)
}

unsafe extern "C" fn rollback_to(table: *mut ffi::sqlite3_vtab, id: c_int) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        let table_ref = unsafe { &mut *table.cast::<TurboVecTable>() };
        let result = table_ref.rollback_to(id);
        unsafe { callback_result(table, result) }
    }))
    .unwrap_or(ffi::SQLITE_ERROR)
}

fn error(message: impl Into<String>) -> Error {
    Error::ModuleError(message.into())
}

fn sqlite_error(code: c_int, message: impl Into<String>) -> Error {
    Error::SqliteFailure(ffi::Error::new(code), Some(message.into()))
}

fn quote(identifier: &str) -> String {
    format!("\"{}\"", escape_double_quote(identifier))
}

fn parse_geometry(args: &[&[u8]]) -> Result<(usize, usize)> {
    let mut dimensions = None;
    let mut bit_width = None;
    for arg in args {
        let (name, value) = parameter(arg)?;
        match name {
            "dimensions" => {
                if dimensions.is_some() {
                    return Err(error("dimensions may only be specified once"));
                }
                dimensions = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| error("dimensions must be a positive integer"))?,
                );
            }
            "bit_width" => {
                if bit_width.is_some() {
                    return Err(error("bit_width may only be specified once"));
                }
                bit_width = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| error("bit_width must be 2, 3, or 4"))?,
                );
            }
            _ => return Err(error(format!("unknown turbovec0 argument '{name}'"))),
        }
    }
    Ok((
        dimensions.ok_or_else(|| error("turbovec0 requires dimensions=N"))?,
        bit_width.ok_or_else(|| error("turbovec0 requires bit_width=N"))?,
    ))
}

/// Schema-qualified, quoted shadow-table names.
struct ShadowNames {
    meta: String,
    info: String,
    ids: String,
    chunks: String,
    /// Unqualified, because incremental BLOB I/O takes the schema separately.
    chunks_table: String,
}

fn shadow_names(database: &str, table: &str) -> ShadowNames {
    let qualified = |suffix: &str| {
        format!(
            "{}.{}",
            quote(database),
            quote(&format!("{table}_{suffix}"))
        )
    };
    ShadowNames {
        meta: qualified("meta"),
        info: qualified("info"),
        ids: qualified("ids"),
        chunks: qualified("chunks"),
        chunks_table: format!("{table}_chunks"),
    }
}

fn connection(handle: *mut ffi::sqlite3) -> Result<Connection> {
    // SAFETY: the virtual table never owns the SQLite connection. Rusqlite's
    // from_handle() creates a non-owning facade and will not close it on drop.
    unsafe { Connection::from_handle(handle) }
}

fn table_exists(connection: &Connection, database: &str, name: &str) -> Result<bool> {
    connection.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM {}.sqlite_schema \
             WHERE type='table' AND name=?1 COLLATE NOCASE)",
            quote(database)
        ),
        [name],
        |row| row.get(0),
    )
}

/// Which table holds an index's metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    /// Written by 0.2.0 and earlier: `_meta` and `_chunks`, and no rowid
    /// table, so every write must load the index to check rowids.
    Legacy,
    /// `_info`, `_ids`, and `_chunks`. `_ids` answers rowid questions, and
    /// `_info` holds the counts a commit needs, so writes can skip the index.
    Current,
}

fn detect_layout(connection: &Connection, database: &str, table: &str) -> Result<Layout> {
    let info = table_exists(connection, database, &format!("{table}_info"))?;
    let ids = table_exists(connection, database, &format!("{table}_ids"))?;
    match (info, ids) {
        (false, false) => Ok(Layout::Legacy),
        (true, true) => Ok(Layout::Current),
        _ => Err(error(format!(
            "turbovec0 {table} has only one of its _info and _ids shadow tables"
        ))),
    }
}

fn check_storage_format(format: i64) -> Result<()> {
    if format == STORAGE_FORMAT {
        return Ok(());
    }
    Err(error(format!(
        "unsupported turbovec0 storage format {format}; \
         this extension reads format {STORAGE_FORMAT}"
    )))
}

/// Decode a stored byte length. A non-negative value is the base image length.
/// While delta batches exist it is stored as `-length-1`, which made 0.1.5 and
/// earlier fail closed instead of ignoring them.
fn decode_byte_len(stored: i64) -> Result<(usize, bool)> {
    let has_deltas = stored < 0;
    let length = if has_deltas {
        stored
            .checked_neg()
            .and_then(|length| length.checked_sub(1))
            .ok_or_else(|| error("invalid persisted TurboVec byte length"))?
    } else {
        stored
    };
    let length = usize::try_from(length)
        .map_err(|_| error("negative or oversized persisted TurboVec byte length"))?;
    Ok((length, has_deltas))
}

fn sql_integer(value: usize, label: &str) -> Result<i64> {
    i64::try_from(value).map_err(|_| error(format!("turbovec0 {label} is too large")))
}

fn sql_id(id: u64) -> Result<i64> {
    i64::try_from(id).map_err(|_| error("TurboVec id is too large for a SQLite rowid"))
}

/// The `_info` row: everything a commit needs to decide between a delta and a
/// compaction without loading the index.
struct Info {
    generation: i64,
    base_bytes: usize,
    row_count: usize,
    delta_bytes: usize,
    delta_operations: usize,
}

fn read_info(connection: &Connection, info: &str) -> Result<Info> {
    let (format, generation, byte_len, row_count, delta_bytes, delta_operations): (
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    ) = connection.query_row(
        &format!(
            "SELECT format, generation, byte_len, row_count, delta_bytes, delta_operations \
             FROM {info} WHERE id=1"
        ),
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )?;
    check_storage_format(format)?;
    let count = |value: i64, label: &str| {
        usize::try_from(value).map_err(|_| error(format!("invalid turbovec0 {label} in _info")))
    };
    Ok(Info {
        generation,
        base_bytes: decode_byte_len(byte_len)?.0,
        row_count: count(row_count, "row count")?,
        delta_bytes: count(delta_bytes, "delta byte count")?,
        delta_operations: count(delta_operations, "delta operation count")?,
    })
}

fn create_info_and_ids(connection: &Connection, names: &ShadowNames) -> Result<()> {
    connection.execute_batch(&format!(
        "CREATE TABLE {}(\
           id INTEGER PRIMARY KEY CHECK(id=1),\
           format INTEGER NOT NULL,\
           dimensions INTEGER NOT NULL,\
           bit_width INTEGER NOT NULL,\
           generation INTEGER NOT NULL,\
           byte_len INTEGER NOT NULL,\
           row_count INTEGER NOT NULL,\
           delta_bytes INTEGER NOT NULL,\
           delta_operations INTEGER NOT NULL\
         );\
         CREATE TABLE {}(id INTEGER PRIMARY KEY);",
        names.info, names.ids
    ))
}

#[allow(clippy::too_many_arguments)]
fn insert_info(
    connection: &Connection,
    info: &str,
    dimensions: usize,
    bit_width: usize,
    generation: i64,
    byte_len: i64,
    row_count: usize,
    delta_bytes: usize,
    delta_operations: usize,
) -> Result<()> {
    let _rowid = PreserveLastInsertRowid {
        connection,
        rowid: connection.last_insert_rowid(),
    };
    connection.execute(
        &format!(
            "INSERT INTO {info}(id, format, dimensions, bit_width, generation, byte_len, \
             row_count, delta_bytes, delta_operations) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
        ),
        params![
            STORAGE_FORMAT,
            sql_integer(dimensions, "dimension")?,
            sql_integer(bit_width, "bit width")?,
            generation,
            byte_len,
            sql_integer(row_count, "row count")?,
            sql_integer(delta_bytes, "delta byte count")?,
            sql_integer(delta_operations, "delta operation count")?,
        ],
    )?;
    Ok(())
}

fn record_compaction(
    connection: &Connection,
    info: &str,
    generation: i64,
    byte_len: usize,
    row_count: usize,
) -> Result<()> {
    let updated = connection.execute(
        &format!(
            "UPDATE {info} SET generation=?1, byte_len=?2, row_count=?3, \
             delta_bytes=0, delta_operations=0 WHERE id=1"
        ),
        params![
            generation,
            sql_integer(byte_len, "byte length")?,
            sql_integer(row_count, "row count")?,
        ],
    )?;
    if updated != 1 {
        return Err(error("turbovec0 _info row is missing"));
    }
    Ok(())
}

fn record_delta(
    connection: &Connection,
    info: &str,
    generation: i64,
    row_count: usize,
    delta_bytes: usize,
    delta_operations: usize,
) -> Result<()> {
    let updated = connection.execute(
        &format!(
            "UPDATE {info} SET generation=?1, \
             byte_len=CASE WHEN byte_len>=0 THEN -byte_len-1 ELSE byte_len END, \
             row_count=?2, delta_bytes=?3, delta_operations=?4 WHERE id=1"
        ),
        params![
            generation,
            sql_integer(row_count, "row count")?,
            sql_integer(delta_bytes, "delta byte count")?,
            sql_integer(delta_operations, "delta operation count")?,
        ],
    )?;
    if updated != 1 {
        return Err(error("turbovec0 _info row is missing"));
    }
    Ok(())
}

/// Rows added minus rows removed by a run of transaction changes.
fn net_rows(changes: &[Change]) -> isize {
    changes
        .iter()
        .map(|change| match change {
            Change::Insert { replaces: true, .. } => 0,
            Change::Insert {
                replaces: false, ..
            } => 1,
            Change::Delete { .. } => -1,
        })
        .sum()
}

/// Validate the stored geometry and report what the base image should contain.
/// Returns the generation, the declared byte length, the chunk count, and
/// whether delta batches are expected.
///
/// This runs before anything reads chunk contents: SQLite can report BLOB
/// lengths without loading them into Rust buffers, so impossible metadata is
/// rejected without ever sizing an allocation from it.
fn validate_chunks(
    connection: &Connection,
    meta: &str,
    chunks: &str,
) -> Result<(i64, usize, i64, bool)> {
    let (generation, stored_len): (i64, i64) = connection.query_row(
        &format!("SELECT generation, byte_len FROM {meta} WHERE id=1"),
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let (expected_len, has_deltas) = decode_byte_len(stored_len)?;
    let mut statement = connection.prepare(&format!(
        "SELECT chunk_id, length(data), typeof(data) FROM {chunks} \
         WHERE chunk_id>=0 ORDER BY chunk_id"
    ))?;
    let mut rows = statement.query([])?;
    let mut count = 0_i64;
    let mut actual_len = 0_usize;
    let mut previous_len = CHUNK_SIZE;
    while let Some(row) = rows.next()? {
        let chunk_id: i64 = row.get(0)?;
        let length: i64 = row.get(1)?;
        let kind: String = row.get(2)?;
        if chunk_id != count || previous_len != CHUNK_SIZE {
            return Err(error(
                "invalid TurboVec chunk sequence or non-final partial chunk",
            ));
        }
        let length = usize::try_from(length).map_err(|_| error("invalid TurboVec chunk length"))?;
        if kind != "blob" || length == 0 || length > CHUNK_SIZE {
            return Err(error("invalid TurboVec chunk type or length"));
        }
        actual_len = actual_len
            .checked_add(length)
            .ok_or_else(|| error("oversized TurboVec chunk payload"))?;
        previous_len = length;
        count += 1;
    }
    if actual_len != expected_len {
        return Err(error(format!(
            "TurboVec chunks contain {actual_len} bytes; metadata declares {expected_len}"
        )));
    }
    Ok((generation, expected_len, count, has_deltas))
}

/// The committed base image as one byte stream, read a chunk at a time through
/// incremental BLOB I/O.
///
/// This exists so that opening an index holds one full copy of the image
/// instead of two. `IdMapIndex::from_bytes` copies the slice it is handed into
/// an owned buffer of its own, so materializing the chunks into a `Vec` first
/// doubled the peak: measured 104 MB of growth on a 48.5 MB image. Streaming
/// straight into the loader leaves only the loader's buffer.
struct ChunkReader<'a> {
    blob: Blob<'a>,
    chunk_id: i64,
    chunk_count: i64,
    remaining: usize,
}

impl<'a> ChunkReader<'a> {
    fn open(
        connection: &'a Connection,
        database: &str,
        chunks_table: &str,
        chunk_count: i64,
        expected_len: usize,
    ) -> Result<Self> {
        if chunk_count == 0 {
            return Err(error(
                "cannot open turbovec0 index: TurboVec index is too short to contain a format header",
            ));
        }
        let blob = connection.blob_open(database, chunks_table, "data", 0, true)?;
        let mut reader = Self {
            blob,
            chunk_id: 0,
            chunk_count,
            remaining: expected_len,
        };
        reader.check_chunk_len()?;
        Ok(reader)
    }

    /// The format gate has to run before any bytes reach TurboVec's loader, so
    /// read the header off the first chunk and rewind. `reopen` resets the read
    /// position, which makes the rewind free.
    fn check_format(&mut self) -> Result<()> {
        let mut header = [0_u8; 5];
        self.blob.read_exact(&mut header).map_err(|_| {
            error("cannot open turbovec0 index: TurboVec index is too short to contain a format header")
        })?;
        check_format(&header)
            .map_err(|cause| error(format!("cannot open turbovec0 index: {cause}")))?;
        self.blob.reopen(0)?;
        Ok(())
    }

    /// Chunk lengths were validated before this reader opened. Re-check each
    /// one as it is reached so a concurrent rewrite is reported rather than
    /// silently deserialized, which is what the old second pass did.
    fn check_chunk_len(&mut self) -> Result<()> {
        if self.blob.len() != self.remaining.min(CHUNK_SIZE) {
            return Err(error("TurboVec chunks changed while reading their payload"));
        }
        Ok(())
    }
}

impl Read for ChunkReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // An empty destination must not be mistaken for end-of-chunk, or it
        // would skip a chunk without reading it.
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let read = self.blob.read(buf)?;
            if read != 0 {
                self.remaining = self.remaining.checked_sub(read).ok_or_else(|| {
                    io::Error::other("TurboVec chunks changed while reading their payload")
                })?;
                return Ok(read);
            }
            self.chunk_id += 1;
            if self.chunk_id >= self.chunk_count {
                return Ok(0);
            }
            self.blob.reopen(self.chunk_id).map_err(io::Error::other)?;
            self.check_chunk_len().map_err(io::Error::other)?;
        }
    }
}

struct LoadedIndex {
    generation: i64,
    index: IdMapIndex,
    base_bytes: usize,
    delta_bytes: usize,
    delta_operations: usize,
}

fn decode_delta(blob: &[u8], dimensions: usize) -> Result<Vec<Change>> {
    if blob.len() < DELTA_HEADER_LEN + DELTA_CHECKSUM_LEN || &blob[..4] != DELTA_MAGIC {
        return Err(error("invalid turbovec0 delta header"));
    }
    let stored_dimensions = u32::from_le_bytes(blob[4..8].try_into().unwrap()) as usize;
    if stored_dimensions != dimensions {
        return Err(error("turbovec0 delta dimension disagrees with its table"));
    }
    let operations = u32::from_le_bytes(blob[8..12].try_into().unwrap()) as usize;
    let checksum_at = blob.len() - DELTA_CHECKSUM_LEN;
    if operations > checksum_at.saturating_sub(DELTA_HEADER_LEN) / 9 {
        return Err(error("invalid turbovec0 delta operation count"));
    }
    let stored_checksum = u32::from_le_bytes(blob[checksum_at..].try_into().unwrap());
    if crc32fast::hash(&blob[..checksum_at]) != stored_checksum {
        return Err(error("turbovec0 delta checksum mismatch"));
    }
    let vector_bytes = dimensions
        .checked_mul(size_of::<f32>())
        .ok_or_else(|| error("oversized turbovec0 delta vector"))?;
    let mut at = DELTA_HEADER_LEN;
    let mut changes = Vec::new();
    changes
        .try_reserve_exact(operations)
        .map_err(|_| sqlite_error(ffi::SQLITE_NOMEM, "cannot allocate turbovec0 delta"))?;
    for _ in 0..operations {
        if checksum_at.saturating_sub(at) < 9 {
            return Err(error("truncated turbovec0 delta operation"));
        }
        let operation = blob[at];
        let id = u64::from_le_bytes(blob[at + 1..at + 9].try_into().unwrap());
        at += 9;
        match operation {
            1 => {
                if checksum_at.saturating_sub(at) < vector_bytes {
                    return Err(error("truncated turbovec0 delta vector"));
                }
                let mut vector = Vec::new();
                vector.try_reserve_exact(dimensions).map_err(|_| {
                    sqlite_error(ffi::SQLITE_NOMEM, "cannot allocate turbovec0 delta vector")
                })?;
                vector.extend(
                    blob[at..at + vector_bytes]
                        .chunks_exact(size_of::<f32>())
                        .map(|bytes| f32::from_le_bytes(bytes.try_into().unwrap())),
                );
                at += vector_bytes;
                changes.push(Change::Insert {
                    id,
                    vector: Some(vector),
                    replaces: true,
                });
            }
            2 => changes.push(Change::Delete { id }),
            _ => return Err(error("unknown turbovec0 delta operation")),
        }
    }
    if at != checksum_at {
        return Err(error("trailing bytes in turbovec0 delta"));
    }
    Ok(changes)
}

fn encode_delta(changes: &[Change], dimensions: usize) -> Result<Vec<u8>> {
    let operations = u32::try_from(changes.len())
        .map_err(|_| sqlite_error(ffi::SQLITE_TOOBIG, "too many turbovec0 delta operations"))?;
    let capacity = delta_encoded_len(changes, dimensions)?;
    let mut blob = Vec::new();
    blob.try_reserve_exact(capacity)
        .map_err(|_| sqlite_error(ffi::SQLITE_NOMEM, "cannot allocate turbovec0 delta"))?;
    blob.extend_from_slice(DELTA_MAGIC);
    blob.extend_from_slice(
        &u32::try_from(dimensions)
            .map_err(|_| error("turbovec0 dimensions exceed the delta format"))?
            .to_le_bytes(),
    );
    blob.extend_from_slice(&operations.to_le_bytes());
    for change in changes {
        match change {
            Change::Insert { id, vector, .. } => {
                let vector = vector
                    .as_ref()
                    .ok_or_else(|| error("turbovec0 delta is missing an inserted vector"))?;
                if vector.len() != dimensions {
                    return Err(error("turbovec0 delta contains a wrong-sized vector"));
                }
                blob.push(1);
                blob.extend_from_slice(&id.to_le_bytes());
                for value in vector {
                    blob.extend_from_slice(&value.to_le_bytes());
                }
            }
            Change::Delete { id } => {
                blob.push(2);
                blob.extend_from_slice(&id.to_le_bytes());
            }
        }
    }
    debug_assert_eq!(blob.len() + DELTA_CHECKSUM_LEN, capacity);
    let checksum = crc32fast::hash(&blob);
    blob.extend_from_slice(&checksum.to_le_bytes());
    Ok(blob)
}

fn delta_encoded_len(changes: &[Change], dimensions: usize) -> Result<usize> {
    let vector_bytes = dimensions
        .checked_mul(size_of::<f32>())
        .ok_or_else(|| error("oversized turbovec0 delta vector"))?;
    let inserts = changes
        .iter()
        .filter(|change| matches!(change, Change::Insert { .. }))
        .count();
    DELTA_HEADER_LEN
        .checked_add(
            changes
                .len()
                .checked_mul(9)
                .ok_or_else(|| sqlite_error(ffi::SQLITE_TOOBIG, "turbovec0 delta is too large"))?,
        )
        .and_then(|length| length.checked_add(inserts.checked_mul(vector_bytes)?))
        .and_then(|length| length.checked_add(DELTA_CHECKSUM_LEN))
        .ok_or_else(|| sqlite_error(ffi::SQLITE_TOOBIG, "turbovec0 delta is too large"))
}

fn read_index(
    connection: &Connection,
    database: &str,
    meta: &str,
    chunks: &str,
    chunks_table: &str,
) -> Result<LoadedIndex> {
    let (generation, base_bytes, chunk_count, expects_deltas) =
        validate_chunks(connection, meta, chunks)?;
    let mut reader =
        ChunkReader::open(connection, database, chunks_table, chunk_count, base_bytes)?;
    reader.check_format()?;
    let mut index = IdMapIndex::load_from_reader(&mut reader)
        .map_err(|cause| error(format!("invalid chunked TurboVec index: {cause}")))?;
    if reader.remaining != 0 {
        return Err(error(format!(
            "TurboVec chunks contain {} unread bytes; metadata declares {base_bytes}",
            reader.remaining
        )));
    }
    drop(reader);
    let dimensions = index
        .dim_opt()
        .ok_or_else(|| error("persisted turbovec0 index has no dimensions"))?;
    let mut statement = connection.prepare(&format!(
        "SELECT chunk_id, data, typeof(data) FROM {chunks} \
         WHERE chunk_id<0 ORDER BY chunk_id DESC"
    ))?;
    let mut rows = statement.query([])?;
    let mut previous_generation = None;
    let mut delta_bytes = 0_usize;
    let mut delta_operations = 0_usize;
    while let Some(row) = rows.next()? {
        let chunk_id: i64 = row.get(0)?;
        let kind: String = row.get(2)?;
        let delta_generation = chunk_id
            .checked_neg()
            .ok_or_else(|| error("invalid turbovec0 delta generation"))?;
        if let Some(previous) = previous_generation
            && delta_generation != previous + 1
        {
            return Err(error("non-contiguous turbovec0 delta generations"));
        }
        let blob = row.get_ref(1)?.as_blob()?;
        if kind != "blob" {
            return Err(error("invalid turbovec0 delta storage type"));
        }
        let changes = decode_delta(blob, dimensions)?;
        TurboVecTable::replay(&mut index, &changes)?;
        delta_bytes = delta_bytes
            .checked_add(blob.len())
            .ok_or_else(|| error("oversized turbovec0 delta storage"))?;
        delta_operations = delta_operations
            .checked_add(changes.len())
            .ok_or_else(|| error("too many turbovec0 delta operations"))?;
        previous_generation = Some(delta_generation);
    }
    match previous_generation {
        Some(_) if !expects_deltas => {
            return Err(error("unexpected turbovec0 delta storage"));
        }
        Some(latest_delta) if latest_delta != generation => {
            return Err(error(
                "latest turbovec0 delta generation disagrees with metadata",
            ));
        }
        None if expects_deltas => return Err(error("missing turbovec0 delta storage")),
        _ => {}
    }
    Ok(LoadedIndex {
        generation,
        index,
        base_bytes,
        delta_bytes,
        delta_operations,
    })
}

pub(crate) fn table_info(connection: &Connection, qualified_table: &str) -> Result<String> {
    let (database, table) = match qualified_table.split_once('.') {
        Some((database, table)) if !database.is_empty() && !table.is_empty() => (database, table),
        None if !qualified_table.is_empty() => ("main", qualified_table),
        _ => {
            return Err(error(
                "turbovec_info() expects a table name or schema.table",
            ));
        }
    };
    if table.contains('.') {
        return Err(error(
            "turbovec_info() expects a table name or schema.table",
        ));
    }

    let schema = format!("{}.sqlite_schema", quote(database));
    let create_sql: String = connection
        .query_row(
            &format!("SELECT sql FROM {schema} WHERE type='table' AND name=?1"),
            [table],
            |row| row.get(0),
        )
        .map_err(|_| error(format!("unknown SQLite table {qualified_table}")))?;
    let normalized = create_sql.to_ascii_lowercase();
    if !normalized.contains("using turbovec0") {
        return Err(error(format!(
            "SQLite table {qualified_table} is not a turbovec0 virtual table"
        )));
    }

    let names = shadow_names(database, table);
    let metadata = match detect_layout(connection, database, table)? {
        Layout::Legacy => &names.meta,
        Layout::Current => &names.info,
    };
    let loaded = read_index(
        connection,
        database,
        metadata,
        &names.chunks,
        &names.chunks_table,
    )?;
    Ok(serde_json::json!({
        "table": qualified_table,
        "generation": loaded.generation,
        "count": loaded.index.len(),
        "bit_width": loaded.index.bit_width(),
        "dimensions": loaded.index.dim_opt(),
        "serialized_bytes": loaded.base_bytes + loaded.delta_bytes,
        "base_bytes": loaded.base_bytes,
        "delta_bytes": loaded.delta_bytes,
        "delta_operations": loaded.delta_operations,
        "format_version": TURBOVEC_FORMAT_VERSION,
        "format_revision": TURBOVEC_FORMAT_REVISION,
    })
    .to_string())
}

/// Shadow-table inserts must not replace the application's last inserted ID,
/// including when serialization fails after writing some chunks.
struct PreserveLastInsertRowid<'a> {
    connection: &'a Connection,
    rowid: i64,
}

impl Drop for PreserveLastInsertRowid<'_> {
    fn drop(&mut self) {
        // SAFETY: the borrowed connection outlives this guard.
        unsafe { ffi::sqlite3_set_last_insert_rowid(self.connection.handle(), self.rowid) };
    }
}

/// One new chunk and one old chunk, independent of total serialized size.
struct ChunkWriter<'a> {
    connection: &'a Connection,
    database: &'a str,
    chunks_table: &'a str,
    select: Statement<'a>,
    upsert: Statement<'a>,
    buffer: Vec<u8>,
    chunk_id: i64,
    byte_len: i64,
    // Preserve SQLite error codes across the std::io::Write interface.
    failure: Option<Error>,
}

impl ChunkWriter<'_> {
    fn write_chunk(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let old: Option<Vec<u8>> = self
            .select
            .query_row([self.chunk_id], |row| row.get(0))
            .optional()?;
        let piece = self.buffer.as_slice();
        if let Some(old) = old {
            if old == piece {
                return Ok(());
            }
            if old.len() == piece.len() {
                let start = old
                    .iter()
                    .zip(piece)
                    .position(|(before, after)| before != after)
                    .expect("different equal-length chunks have a first difference");
                let end = old
                    .iter()
                    .zip(piece)
                    .rposition(|(before, after)| before != after)
                    .expect("different equal-length chunks have a last difference")
                    + 1;
                let mut blob = self.connection.blob_open(
                    self.database,
                    self.chunks_table,
                    "data",
                    self.chunk_id,
                    false,
                )?;
                blob.write_at(&piece[start..end], start)?;
                blob.close()?;
                return Ok(());
            }
        }
        self.upsert.execute(params![self.chunk_id, piece])?;
        Ok(())
    }

    fn io_failure(&mut self, cause: Error) -> io::Error {
        self.failure = Some(cause);
        io::Error::other("cannot persist TurboVec chunk")
    }
}

impl Write for ChunkWriter<'_> {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let length = bytes.len();
        let new_len = i64::try_from(length)
            .ok()
            .and_then(|length| self.byte_len.checked_add(length))
            .ok_or_else(|| {
                self.io_failure(sqlite_error(
                    ffi::SQLITE_TOOBIG,
                    "TurboVec index is too large",
                ))
            })?;
        while !bytes.is_empty() {
            let take = bytes.len().min(CHUNK_SIZE - self.buffer.len());
            self.buffer.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.buffer.len() == CHUNK_SIZE {
                self.write_chunk().map_err(|cause| self.io_failure(cause))?;
                self.buffer.clear();
                self.chunk_id += 1;
            }
        }
        self.byte_len = new_len;
        Ok(length)
    }

    fn flush(&mut self) -> io::Result<()> {
        // A flush may occur mid-chunk. Retain it so later writes extend and
        // replace the same chunk, rather than creating a short interior chunk.
        self.write_chunk().map_err(|cause| self.io_failure(cause))
    }
}

/// Stream the index over the base chunks and drop every delta batch. Returns
/// the image length; the caller records it in the metadata row.
fn write_index(
    connection: &Connection,
    database: &str,
    chunks: &str,
    chunks_table: &str,
    index: &IdMapIndex,
) -> Result<usize> {
    let _rowid = PreserveLastInsertRowid {
        connection,
        rowid: connection.last_insert_rowid(),
    };
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(CHUNK_SIZE).map_err(|_| {
        sqlite_error(
            ffi::SQLITE_NOMEM,
            "cannot allocate TurboVec serialization buffer",
        )
    })?;
    let mut writer = ChunkWriter {
        connection,
        database,
        chunks_table,
        select: connection.prepare(&format!("SELECT data FROM {chunks} WHERE chunk_id=?1"))?,
        upsert: connection.prepare(&format!(
            "INSERT INTO {chunks}(chunk_id, data) VALUES (?1, ?2) \
             ON CONFLICT(chunk_id) DO UPDATE SET data=excluded.data"
        ))?,
        buffer,
        chunk_id: 0,
        byte_len: 0,
        failure: None,
    };
    if let Err(cause) = index.write_to_writer(&mut writer) {
        return Err(writer
            .failure
            .take()
            .unwrap_or_else(|| error(format!("cannot serialize TurboVec index: {cause}"))));
    }
    writer.write_chunk()?;
    let chunk_count = writer.chunk_id + i64::from(!writer.buffer.is_empty());
    let byte_len = writer.byte_len;
    drop(writer);
    connection.execute(
        &format!("DELETE FROM {chunks} WHERE chunk_id < 0 OR chunk_id >= ?1"),
        [chunk_count],
    )?;
    usize::try_from(byte_len).map_err(|_| error("invalid serialized turbovec0 byte length"))
}

/// Store one delta batch at `-generation`. The caller records it in the
/// metadata row.
fn write_delta(connection: &Connection, chunks: &str, generation: i64, blob: &[u8]) -> Result<()> {
    let _rowid = PreserveLastInsertRowid {
        connection,
        rowid: connection.last_insert_rowid(),
    };
    let chunk_id = generation
        .checked_neg()
        .filter(|chunk_id| *chunk_id < 0)
        .ok_or_else(|| error("invalid turbovec0 delta generation"))?;
    connection.execute(
        &format!("INSERT INTO {chunks}(chunk_id,data) VALUES(?1,?2)"),
        params![chunk_id, blob],
    )?;
    Ok(())
}

fn validate_geometry(index: &IdMapIndex, dimensions: usize, bit_width: usize) -> Result<()> {
    if index.dim_opt() != Some(dimensions) {
        return Err(error(
            "persisted TurboVec dimension disagrees with its metadata",
        ));
    }
    if index.bit_width() != bit_width {
        return Err(error(
            "persisted TurboVec bit width disagrees with its metadata",
        ));
    }
    Ok(())
}

struct State {
    layout: Layout,
    /// Generation of the committed storage this state describes.
    generation: i64,
    /// The warm index, or `None` until a search, a compaction, or a legacy
    /// layout needs it. When loaded it includes this transaction's changes.
    index: Option<IdMapIndex>,
    transaction: Option<TransactionState>,
    dirty: bool,
    base_bytes: usize,
    delta_bytes: usize,
    delta_operations: usize,
    /// Rows in storage at `generation`, before this transaction's changes.
    row_count: usize,
}

enum Change {
    Insert {
        id: u64,
        vector: Option<Vec<f32>>,
        replaces: bool,
    },
    Delete {
        id: u64,
    },
}

struct DestructiveCheckpoint {
    change_index: usize,
    payload: Vec<u8>,
}

struct TransactionState {
    start_generation: i64,
    start_base_bytes: usize,
    start_delta_bytes: usize,
    start_delta_operations: usize,
    start_row_count: usize,
    changes: Vec<Change>,
    destructive_checkpoint: Option<DestructiveCheckpoint>,
    savepoints: Vec<(c_int, usize)>,
    synced_changes: usize,
    /// This transaction's commit gave a legacy index its `_info` and `_ids`.
    /// Rolling back removes them again.
    upgraded: bool,
}

impl TransactionState {
    fn start(state: &State) -> Self {
        Self {
            start_generation: state.generation,
            start_base_bytes: state.base_bytes,
            start_delta_bytes: state.delta_bytes,
            start_delta_operations: state.delta_operations,
            start_row_count: state.row_count,
            changes: Vec::new(),
            destructive_checkpoint: None,
            savepoints: Vec::new(),
            synced_changes: 0,
            upgraded: false,
        }
    }
}

#[repr(C)]
struct TurboVecTable {
    base: ffi::sqlite3_vtab,
    db: *mut ffi::sqlite3,
    database: String,
    table: String,
    dimensions: usize,
    bit_width: usize,
    names: ShadowNames,
    state: Mutex<State>,
}

fn create_storage(
    connection: &Connection,
    database: &str,
    names: &ShadowNames,
    dimensions: usize,
    bit_width: usize,
) -> Result<State> {
    let index = IdMapIndex::new(dimensions, bit_width)
        .map_err(|cause| error(format!("invalid turbovec0 geometry: {cause}")))?;
    create_info_and_ids(connection, names)?;
    connection.execute_batch(&format!(
        "CREATE TABLE {}(chunk_id INTEGER PRIMARY KEY, data BLOB NOT NULL);",
        names.chunks
    ))?;
    let base_bytes = write_index(
        connection,
        database,
        &names.chunks,
        &names.chunks_table,
        &index,
    )?;
    insert_info(
        connection,
        &names.info,
        dimensions,
        bit_width,
        0,
        sql_integer(base_bytes, "byte length")?,
        0,
        0,
        0,
    )?;
    Ok(State {
        layout: Layout::Current,
        generation: 0,
        index: Some(index),
        transaction: None,
        dirty: false,
        base_bytes,
        delta_bytes: 0,
        delta_operations: 0,
        row_count: 0,
    })
}

fn open_storage(
    connection: &Connection,
    database: &str,
    table: &str,
    names: &ShadowNames,
    dimensions: usize,
    bit_width: usize,
) -> Result<State> {
    let layout = detect_layout(connection, database, table)?;
    let metadata = match layout {
        Layout::Legacy => &names.meta,
        Layout::Current => {
            // Check the format alone first, so a later layout that changes
            // other columns is still refused with this message.
            let format: i64 = connection.query_row(
                &format!("SELECT format FROM {} WHERE id=1", names.info),
                [],
                |row| row.get(0),
            )?;
            check_storage_format(format)?;
            &names.info
        }
    };
    let stored: (i64, i64) = connection.query_row(
        &format!("SELECT dimensions, bit_width FROM {metadata} WHERE id=1"),
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if stored != (dimensions as i64, bit_width as i64) {
        return Err(error(format!(
            "turbovec0 schema says dimensions={dimensions}, bit_width={bit_width}; \
             shadow metadata says dimensions={}, bit_width={}",
            stored.0, stored.1
        )));
    }
    if layout == Layout::Current {
        // Connecting reads one row. The index loads when a search or a
        // compaction first needs it.
        let info = read_info(connection, &names.info)?;
        return Ok(State {
            layout,
            generation: info.generation,
            index: None,
            transaction: None,
            dirty: false,
            base_bytes: info.base_bytes,
            delta_bytes: info.delta_bytes,
            delta_operations: info.delta_operations,
            row_count: info.row_count,
        });
    }
    // A legacy index has no rowid table, so the connection loads it now, as
    // every connection did before. Its first commit adds `_info` and `_ids`.
    let loaded = read_index(
        connection,
        database,
        &names.meta,
        &names.chunks,
        &names.chunks_table,
    )?;
    validate_geometry(&loaded.index, dimensions, bit_width)?;
    Ok(State {
        layout,
        generation: loaded.generation,
        row_count: loaded.index.len(),
        index: Some(loaded.index),
        transaction: None,
        dirty: false,
        base_bytes: loaded.base_bytes,
        delta_bytes: loaded.delta_bytes,
        delta_operations: loaded.delta_operations,
    })
}

impl TurboVecTable {
    fn configure(db: &mut VTabConnection) -> Result<()> {
        // SQLITE_VTAB_CONSTRAINT_SUPPORT is the only config option here with
        // a variadic third argument. Rusqlite 0.40's convenience method omits
        // it, so call the host API-table function directly.
        let pointer = *VTAB_CONFIG.get().ok_or_else(|| {
            // Naming the missing callback described the symptom. The cause is
            // almost always a host older than the callbacks this module needs,
            // which a static build inherits from whatever libsqlite3 it linked
            // against — so say that, and say which version it found.
            match HOST_VERSION.get() {
                Some(version) if *version < MIN_HOST_VERSION => error(format!(
                    "turbovec0 requires SQLite {}.{}.{} or newer; this host is {}.{}.{}. \
                     The scalar turbovec_* functions still work.",
                    MIN_HOST_VERSION / 1_000_000,
                    MIN_HOST_VERSION / 1_000 % 1_000,
                    MIN_HOST_VERSION % 1_000,
                    version / 1_000_000,
                    version / 1_000 % 1_000,
                    version % 1_000,
                )),
                _ => error(
                    "SQLite virtual-table config callback is unavailable; turbovec0 needs the \
                     host's extension API table",
                ),
            }
        })?;
        let callback: VtabConfigCallback = unsafe { std::mem::transmute(pointer) };
        let result =
            unsafe { callback(db.handle(), ffi::SQLITE_VTAB_CONSTRAINT_SUPPORT, 1 as c_int) };
        if result != ffi::SQLITE_OK {
            return Err(Error::SqliteFailure(ffi::Error::new(result), None));
        }
        db.config(VTabConfig::DirectOnly)
    }

    fn make(
        db: &mut VTabConnection,
        database_name: &[u8],
        table_name: &[u8],
        args: &[&[u8]],
        create: bool,
    ) -> Result<Self> {
        Self::configure(db)?;
        let (dimensions, bit_width) = parse_geometry(args)?;
        let database = std::str::from_utf8(database_name)?.to_owned();
        let table = std::str::from_utf8(table_name)?.to_owned();
        let names = shadow_names(&database, &table);
        let handle = unsafe { db.handle() };
        let connection = connection(handle)?;
        let state = if create {
            create_storage(&connection, &database, &names, dimensions, bit_width)?
        } else {
            open_storage(
                &connection,
                &database,
                &table,
                &names,
                dimensions,
                bit_width,
            )?
        };
        Ok(Self {
            base: ffi::sqlite3_vtab::default(),
            db: handle,
            database,
            table,
            dimensions,
            bit_width,
            names,
            state: Mutex::new(state),
        })
    }

    fn metadata_table(&self, layout: Layout) -> &str {
        match layout {
            Layout::Legacy => &self.names.meta,
            Layout::Current => &self.names.info,
        }
    }

    /// Bring the state up to committed storage. Inside a transaction this
    /// connection holds the write lock, so its state is already current.
    fn refresh(&self, state: &mut State) -> Result<()> {
        if state.transaction.is_some() {
            return Ok(());
        }
        let connection = connection(self.db)?;
        if state.layout == Layout::Legacy {
            let generation: Option<i64> = connection
                .query_row(
                    &format!("SELECT generation FROM {} WHERE id=1", self.names.meta),
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(generation) = generation else {
                // Another connection's commit upgraded the index.
                if detect_layout(&connection, &self.database, &self.table)? == Layout::Legacy {
                    return Err(error("turbovec0 metadata row is missing"));
                }
                state.layout = Layout::Current;
                state.index = None;
                return self.refresh(state);
            };
            if state.index.is_some() && generation == state.generation {
                return Ok(());
            }
            if state.index.is_some() && self.catch_up(&connection, state, generation)? {
                state.row_count = state.index.as_ref().map_or(0, IdMapIndex::len);
                return Ok(());
            }
            state.index = None;
            self.ensure_loaded(state)?;
            return Ok(());
        }
        let info = read_info(&connection, &self.names.info)?;
        if state.index.is_some()
            && info.generation != state.generation
            && !self.catch_up(&connection, state, info.generation)?
        {
            state.index = None;
        }
        state.generation = info.generation;
        state.base_bytes = info.base_bytes;
        state.delta_bytes = info.delta_bytes;
        state.delta_operations = info.delta_operations;
        state.row_count = info.row_count;
        Ok(())
    }

    /// Bring a warm copy forward by replaying only the delta batches other
    /// connections committed since its generation, instead of rebuilding the
    /// whole index. This is the difference between a read after a concurrent
    /// one-row commit costing O(index) and costing O(that commit).
    ///
    /// The base image is provably unchanged whenever the oldest surviving
    /// delta is no newer than this copy's generation: compaction rewrites the
    /// base and deletes every delta in the same statement, so a delta older
    /// than us could not have survived one. Everything else — a compaction, a
    /// gap in the chain, a generation that does not land exactly on the
    /// persisted one — returns `false`, and the caller drops the copy.
    fn catch_up(
        &self,
        connection: &Connection,
        state: &mut State,
        persisted_generation: i64,
    ) -> Result<bool> {
        if persisted_generation <= state.generation {
            return Ok(false);
        }
        // Deltas live at chunk_id = -generation, so the greatest negative
        // chunk_id is the oldest surviving batch.
        let oldest: Option<i64> = connection.query_row(
            &format!(
                "SELECT max(chunk_id) FROM {} WHERE chunk_id<0",
                self.names.chunks
            ),
            [],
            |row| row.get(0),
        )?;
        let Some(oldest) = oldest.and_then(|chunk_id| chunk_id.checked_neg()) else {
            return Ok(false);
        };
        if oldest > state.generation {
            return Ok(false);
        }
        let mut statement = connection.prepare(&format!(
            "SELECT chunk_id, data, typeof(data) FROM {} \
             WHERE chunk_id<0 AND chunk_id<?1 ORDER BY chunk_id DESC",
            self.names.chunks
        ))?;
        let mut rows = statement.query([-state.generation])?;
        // Replay applies to the warm copy itself, so a failure or a declined
        // chain part-way through leaves it between generations. Take it out
        // of the state for the walk and put it back only after a complete,
        // contiguous catch-up; otherwise the next use reloads it.
        let Some(mut index) = state.index.take() else {
            return Ok(false);
        };
        let mut generation = state.generation;
        let mut delta_bytes = state.delta_bytes;
        let mut delta_operations = state.delta_operations;
        while let Some(row) = rows.next()? {
            let chunk_id: i64 = row.get(0)?;
            let kind: String = row.get(2)?;
            if kind != "blob" {
                return Err(error("invalid turbovec0 delta storage type"));
            }
            let delta_generation = chunk_id
                .checked_neg()
                .ok_or_else(|| error("invalid turbovec0 delta generation"))?;
            if delta_generation != generation + 1 {
                return Ok(false);
            }
            let blob = row.get_ref(1)?.as_blob()?;
            let changes = decode_delta(blob, self.dimensions)?;
            Self::replay(&mut index, &changes)?;
            delta_bytes = delta_bytes
                .checked_add(blob.len())
                .ok_or_else(|| error("oversized turbovec0 delta storage"))?;
            delta_operations = delta_operations
                .checked_add(changes.len())
                .ok_or_else(|| error("too many turbovec0 delta operations"))?;
            generation = delta_generation;
        }
        if generation != persisted_generation {
            return Ok(false);
        }
        state.index = Some(index);
        state.generation = generation;
        state.delta_bytes = delta_bytes;
        state.delta_operations = delta_operations;
        Ok(true)
    }

    /// The warm index, loaded from storage first if this connection has not
    /// needed it yet. Changes the transaction has not written to storage are
    /// replayed onto the loaded copy.
    fn ensure_loaded<'a>(&self, state: &'a mut State) -> Result<&'a mut IdMapIndex> {
        if state.index.is_none() {
            let connection = connection(self.db)?;
            let loaded = read_index(
                &connection,
                &self.database,
                self.metadata_table(state.layout),
                &self.names.chunks,
                &self.names.chunks_table,
            )?;
            validate_geometry(&loaded.index, self.dimensions, self.bit_width)?;
            // Inside a transaction storage cannot move under this connection.
            // Outside one, a current layout's refresh() read `_info` in this
            // same statement. Only a legacy refresh adopts what it finds.
            let adopts_generation = state.transaction.is_none() && state.layout == Layout::Legacy;
            if !adopts_generation && loaded.generation != state.generation {
                return Err(error("turbovec0 storage changed while loading its index"));
            }
            let mut index = loaded.index;
            if state.layout == Layout::Current && index.len() != state.row_count {
                return Err(error(format!(
                    "turbovec0 index holds {} rows; _info records {}",
                    index.len(),
                    state.row_count
                )));
            }
            state.generation = loaded.generation;
            state.row_count = index.len();
            state.base_bytes = loaded.base_bytes;
            state.delta_bytes = loaded.delta_bytes;
            state.delta_operations = loaded.delta_operations;
            if let Some(transaction) = state.transaction.as_ref() {
                Self::replay(
                    &mut index,
                    &transaction.changes[transaction.synced_changes..],
                )?;
            }
            state.index = Some(index);
        }
        Ok(state.index.as_mut().expect("index loaded above"))
    }

    /// Load the index when a write cannot go without it: a legacy index has
    /// no `_ids` to check rowids against, and every commit rewrites a base
    /// under 1 MiB anyway.
    fn prepare_write(&self, state: &mut State) -> Result<()> {
        if state.layout == Layout::Legacy || state.base_bytes < MIN_DELTA_BASE_BYTES {
            self.ensure_loaded(state)?;
        }
        Ok(())
    }

    /// Give a 0.2.0-layout index `_info` and `_ids`, and empty `_meta`. Every
    /// 0.2.0 code path reads the `_meta` row first, so older builds refuse
    /// the index from then on instead of writing without maintaining `_ids`.
    /// Runs inside the first commit that writes to the index, from the copy
    /// that commit already holds.
    fn upgrade_storage(&self, connection: &Connection, state: &mut State) -> Result<()> {
        self.ensure_loaded(state)?;
        let byte_len: i64 = connection.query_row(
            &format!("SELECT byte_len FROM {} WHERE id=1", self.names.meta),
            [],
            |row| row.get(0),
        )?;
        create_info_and_ids(connection, &self.names)?;
        insert_info(
            connection,
            &self.names.info,
            self.dimensions,
            self.bit_width,
            state.generation,
            byte_len,
            state.row_count,
            state.delta_bytes,
            state.delta_operations,
        )?;
        let mut ids: Vec<u64> = state
            .index
            .as_ref()
            .expect("index loaded above")
            .iter_ids()
            .collect();
        // Ascending keys append to the B-tree instead of splitting pages.
        ids.sort_unstable();
        {
            let _rowid = PreserveLastInsertRowid {
                connection,
                rowid: connection.last_insert_rowid(),
            };
            let mut insert =
                connection.prepare(&format!("INSERT INTO {}(id) VALUES (?1)", self.names.ids))?;
            for id in ids {
                insert.execute([sql_id(id)?])?;
            }
        }
        connection.execute(&format!("DELETE FROM {} WHERE id=1", self.names.meta), [])?;
        state.layout = Layout::Current;
        if let Some(transaction) = state.transaction.as_mut() {
            transaction.upgraded = true;
        }
        Ok(())
    }

    /// Add a rowid to `_ids`. Returns false, and writes nothing, when it is
    /// already there.
    fn add_id(&self, id: u64) -> Result<bool> {
        let connection = connection(self.db)?;
        let _rowid = PreserveLastInsertRowid {
            connection: &connection,
            rowid: connection.last_insert_rowid(),
        };
        let added = connection.execute(
            &format!("INSERT OR IGNORE INTO {}(id) VALUES (?1)", self.names.ids),
            [sql_id(id)?],
        )?;
        Ok(added == 1)
    }

    /// Remove a rowid from `_ids`. Returns false when it was not there.
    fn remove_id(&self, id: u64) -> Result<bool> {
        let connection = connection(self.db)?;
        let removed = connection.execute(
            &format!("DELETE FROM {} WHERE id=?1", self.names.ids),
            [sql_id(id)?],
        )?;
        Ok(removed == 1)
    }

    fn has_id(&self, id: u64) -> Result<bool> {
        let connection = connection(self.db)?;
        connection.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM {} WHERE id=?1)",
                self.names.ids
            ),
            [sql_id(id)?],
            |row| row.get(0),
        )
    }

    fn stored_ids(&self) -> Result<Vec<u64>> {
        let connection = connection(self.db)?;
        let mut statement =
            connection.prepare(&format!("SELECT id FROM {} ORDER BY id", self.names.ids))?;
        statement
            .query_map([], |row| row.get::<_, i64>(0))?
            .map(|id| u64::try_from(id?).map_err(|_| error("negative rowid in turbovec0 _ids")))
            .collect()
    }

    /// Every rowid, from the warm index when it is loaded and from `_ids`
    /// otherwise.
    fn rowids(&self, state: &mut State) -> Result<Vec<u64>> {
        if state.layout == Layout::Legacy {
            self.ensure_loaded(state)?;
        }
        match state.index.as_ref() {
            Some(index) => Ok(index.iter_ids().collect()),
            None => self.stored_ids(),
        }
    }

    fn contains_rowid(&self, state: &mut State, id: u64) -> Result<bool> {
        if state.layout == Layout::Legacy {
            self.ensure_loaded(state)?;
        }
        match state.index.as_ref() {
            Some(index) => Ok(index.contains(id)),
            None => self.has_id(id),
        }
    }

    fn ids_disagree(&self, id: u64) -> Error {
        error(format!(
            "turbovec0 index and {}_ids disagree about rowid {id}; \
             PRAGMA integrity_check reports the difference",
            self.table
        ))
    }

    fn rename(&mut self, new_name: &str) -> Result<()> {
        let connection = connection(self.db)?;
        let mut statements = String::new();
        for suffix in SHADOW_SUFFIXES {
            let old = format!("{}_{suffix}", self.table);
            if table_exists(&connection, &self.database, &old)? {
                statements.push_str(&format!(
                    "ALTER TABLE {}.{} RENAME TO {};",
                    quote(&self.database),
                    quote(&old),
                    quote(&format!("{new_name}_{suffix}")),
                ));
            }
        }
        connection.execute_batch(&statements)?;
        self.table = new_name.to_owned();
        self.names = shadow_names(&self.database, new_name);
        Ok(())
    }

    fn integrity(&self) -> Result<()> {
        let connection = connection(self.db)?;
        let layout = detect_layout(&connection, &self.database, &self.table)?;
        let metadata = self.metadata_table(layout);
        let loaded = read_index(
            &connection,
            &self.database,
            metadata,
            &self.names.chunks,
            &self.names.chunks_table,
        )?;
        let (dimensions, bit_width): (i64, i64) = connection.query_row(
            &format!("SELECT dimensions, bit_width FROM {metadata} WHERE id=1"),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let dimensions = usize::try_from(dimensions)
            .map_err(|_| error("invalid dimension in turbovec0 metadata"))?;
        let bit_width = usize::try_from(bit_width)
            .map_err(|_| error("invalid bit width in turbovec0 metadata"))?;
        validate_geometry(&loaded.index, dimensions, bit_width)?;
        if layout == Layout::Legacy {
            return Ok(());
        }

        let info = read_info(&connection, &self.names.info)?;
        for (label, recorded, stored) in [
            ("row count", info.row_count, loaded.index.len()),
            ("delta byte count", info.delta_bytes, loaded.delta_bytes),
            (
                "delta operation count",
                info.delta_operations,
                loaded.delta_operations,
            ),
        ] {
            if recorded != stored {
                return Err(error(format!(
                    "_info records {label} {recorded}; storage holds {stored}"
                )));
            }
        }

        // `_ids` changes with each statement but the chunks only at commit.
        // Apply this connection's unwritten changes, so a check inside a
        // write transaction compares like with like.
        let mut expected: std::collections::HashSet<u64> = loaded.index.iter_ids().collect();
        {
            let state = self
                .state
                .lock()
                .map_err(|_| error("turbovec0 state lock is poisoned"))?;
            if let Some(transaction) = state.transaction.as_ref() {
                for change in &transaction.changes[transaction.synced_changes..] {
                    match change {
                        Change::Insert { id, .. } => {
                            expected.insert(*id);
                        }
                        Change::Delete { id } => {
                            expected.remove(id);
                        }
                    }
                }
            }
        }
        let stored = self.stored_ids()?;
        let extra = stored.iter().find(|id| !expected.contains(id));
        if stored.len() != expected.len() || extra.is_some() {
            let missing = expected
                .iter()
                .filter(|id| stored.binary_search(id).is_err())
                .min();
            return Err(error(format!(
                "{}_ids holds {} rowids and the index {}; first missing from _ids: {missing:?}, \
                 first absent from the index: {extra:?}",
                self.table,
                stored.len(),
                expected.len(),
            )));
        }
        Ok(())
    }

    fn ensure_transaction(state: &mut State) -> &mut TransactionState {
        if state.transaction.is_none() {
            state.transaction = Some(TransactionState::start(state));
        }
        state
            .transaction
            .as_mut()
            .expect("transaction started above")
    }

    fn ensure_destructive_checkpoint(state: &mut State) {
        Self::ensure_transaction(state);
        // Ordinary deletes need no eager image. A rollback can rebuild from
        // committed storage and replay the retained operation prefix. The
        // only fallback is a transaction that bulk-inserted without retaining
        // vectors before its first destructive change.
        let needs_checkpoint = state.transaction.as_ref().is_some_and(|transaction| {
            transaction.destructive_checkpoint.is_none()
                && transaction
                    .changes
                    .iter()
                    .any(|change| matches!(change, Change::Insert { vector: None, .. }))
        });
        let Some(index) = state.index.as_ref().filter(|_| needs_checkpoint) else {
            return;
        };
        let payload = index.to_bytes();
        let transaction = Self::ensure_transaction(state);
        transaction.destructive_checkpoint = Some(DestructiveCheckpoint {
            change_index: transaction.changes.len(),
            payload,
        });
    }

    fn replay(index: &mut IdMapIndex, changes: &[Change]) -> Result<()> {
        for change in changes {
            match change {
                Change::Insert { id, vector, .. } => {
                    let vector = vector.as_ref().ok_or_else(|| {
                        error("turbovec0 transaction replay is missing an inserted vector")
                    })?;
                    if index.contains(*id) {
                        index.remove(*id);
                    }
                    index.add_with_ids(vector, &[*id]).map_err(|cause| {
                        error(format!("cannot replay inserted vector: {cause}"))
                    })?;
                }
                Change::Delete { id } => {
                    if !index.remove(*id) {
                        return Err(error(format!(
                            "cannot replay deletion of missing turbovec0 rowid {id}"
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    fn undo_insert_prefix(index: &mut IdMapIndex, changes: &[Change]) -> Result<()> {
        for change in changes.iter().rev() {
            // Removing a replacement would lose the row it replaced.
            let Change::Insert {
                id,
                replaces: false,
                ..
            } = change
            else {
                return Err(error(
                    "turbovec0 transaction prefix contains a destructive change",
                ));
            };
            if !index.remove(*id) {
                return Err(error(format!(
                    "cannot undo insertion of missing turbovec0 rowid {id}"
                )));
            }
        }
        Ok(())
    }

    /// Return the in-memory state to an earlier point in the change list.
    /// SQLite rolls `_ids` back itself.
    fn restore_to(&self, state: &mut State, change_index: usize) -> Result<()> {
        let mut transaction = state
            .transaction
            .take()
            .ok_or_else(|| error("turbovec0 has no active transaction"))?;
        if change_index > transaction.changes.len() {
            state.transaction = Some(transaction);
            return Err(error("invalid turbovec0 savepoint change index"));
        }

        if let Some(checkpoint) = transaction.destructive_checkpoint.take() {
            let mut index = IdMapIndex::from_bytes(&checkpoint.payload)
                .map_err(|cause| error(format!("cannot restore turbovec0 checkpoint: {cause}")))?;
            if change_index < checkpoint.change_index {
                Self::undo_insert_prefix(
                    &mut index,
                    &transaction.changes[change_index..checkpoint.change_index],
                )?;
            } else {
                Self::replay(
                    &mut index,
                    &transaction.changes[checkpoint.change_index..change_index],
                )?;
                transaction.destructive_checkpoint = Some(checkpoint);
            }
            state.index = Some(index);
        } else {
            let tail = &transaction.changes[change_index..];
            let tail_is_append_only = tail.iter().all(|change| {
                matches!(
                    change,
                    Change::Insert {
                        replaces: false,
                        ..
                    }
                )
            });
            match state.index.as_mut() {
                Some(index) if tail_is_append_only => Self::undo_insert_prefix(index, tail)?,
                _ if tail.is_empty() => {}
                _ => {
                    if transaction.synced_changes != 0 {
                        state.transaction = Some(transaction);
                        return Err(error("cannot roll back a turbovec0 savepoint after xSync"));
                    }
                    // Drop the copy, if any. The next use rebuilds it from
                    // committed storage and the retained prefix.
                    state.index = None;
                }
            }
        }
        transaction.changes.truncate(change_index);
        transaction.synced_changes = transaction.synced_changes.min(change_index);
        state.dirty = !transaction.changes.is_empty();
        state.transaction = Some(transaction);
        Ok(())
    }

    /// Row count for planner estimates. `try_lock` keeps xBestIndex off the
    /// blocking path: a cursor or a writer may hold the state lock, and a
    /// stalled planner is worse than a stale row count.
    fn estimated_rows(&self) -> i64 {
        self.state
            .try_lock()
            .ok()
            .and_then(|state| match (&state.index, state.layout) {
                (Some(index), _) => Some(index.len()),
                (None, Layout::Current) => Some(state.row_count),
                (None, Layout::Legacy) => None,
            })
            .and_then(|rows| i64::try_from(rows).ok())
            .unwrap_or(DEFAULT_ESTIMATED_ROWS)
    }

    fn savepoint(&mut self, id: c_int) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| error("turbovec0 state lock is poisoned"))?;
        let transaction = Self::ensure_transaction(&mut state);
        transaction
            .savepoints
            .retain(|(existing, _)| *existing < id);
        transaction.savepoints.push((id, transaction.changes.len()));
        Ok(())
    }

    fn release(&mut self, id: c_int) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| error("turbovec0 state lock is poisoned"))?;
        let transaction = state
            .transaction
            .as_mut()
            .ok_or_else(|| error("turbovec0 has no active transaction"))?;
        transaction
            .savepoints
            .retain(|(existing, _)| *existing < id);
        Ok(())
    }

    fn rollback_to(&mut self, id: c_int) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| error("turbovec0 state lock is poisoned"))?;
        let change_index = state
            .transaction
            .as_ref()
            .ok_or_else(|| error("turbovec0 has no active transaction"))?
            .savepoints
            .iter()
            .rev()
            .find(|(existing, _)| *existing == id)
            .map(|(_, change_index)| *change_index)
            .ok_or_else(|| error(format!("unknown turbovec0 savepoint {id}")))?;
        self.restore_to(&mut state, change_index)?;
        state
            .transaction
            .as_mut()
            .expect("restore_to preserves the transaction")
            .savepoints
            .retain(|(existing, _)| *existing <= id);
        Ok(())
    }
}

unsafe impl<'vtab> VTab<'vtab> for TurboVecTable {
    type Aux = ();
    type Cursor = TurboVecCursor<'vtab>;

    fn connect(
        db: &mut VTabConnection,
        _aux: Option<&Self::Aux>,
        _module_name: &[u8],
        database_name: &[u8],
        table_name: &[u8],
        args: &[&[u8]],
    ) -> Result<(Cow<'static, CStr>, Self)> {
        Ok((
            Cow::Borrowed(c"CREATE TABLE x(embedding BLOB, score REAL HIDDEN, k INTEGER HIDDEN)"),
            Self::make(db, database_name, table_name, args, false)?,
        ))
    }

    fn best_index(&self, info: &mut IndexInfo) -> Result<bool> {
        let mut rowid = None;
        let mut rowid_is_in = false;
        let mut query = None;
        let mut unusable_query = false;
        let mut k = None;
        let mut limit = None;
        let mut offset = None;
        for (index, constraint) in info.constraints().enumerate() {
            if !constraint.is_usable() {
                if constraint.column() == COL_EMBEDDING
                    && constraint.operator() == IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_MATCH
                {
                    unusable_query = true;
                }
                continue;
            }
            match (constraint.column(), constraint.operator()) {
                (-1, IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_EQ) => {
                    rowid = Some(index);
                    rowid_is_in = vtab_in(current_index_info()?, index, -1)?;
                }
                (COL_EMBEDDING, IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_MATCH) => {
                    query = Some(index)
                }
                (COL_K, IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_EQ) => k = Some(index),
                (_, IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_LIMIT) => limit = Some(index),
                (_, IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_OFFSET) => offset = Some(index),
                _ => {}
            }
        }

        let ordered_by_score_desc = info.num_of_order_by() == 1
            && info
                .order_bys()
                .next()
                .is_some_and(|order| order.column() == COL_SCORE && order.is_order_by_desc());

        if let Some(query) = query {
            if k.is_none() && limit.is_none() {
                return Err(error(
                    "turbovec0 MATCH requires a single-table scan with ORDER BY score DESC LIMIT n \
                     (or hidden k=n); bind the query or use a scalar subquery before joining",
                ));
            }
            // LIMIT defines the TurboVec candidate count only for nearest-first
            // ordering. Otherwise fetching LIMIT winners and letting SQLite
            // reorder that subset would produce a plausible but incorrect
            // global result.
            if limit.is_some() && !ordered_by_score_desc {
                return Err(error(
                    "turbovec0 LIMIT requires ORDER BY the unmodified score column DESC",
                ));
            }
            let mut query_usage = info.constraint_usage(query);
            query_usage.set_argv_index(1);
            query_usage.set_omit(true);
            let mut argument = 2;
            let mut plan = PLAN_KNN;
            if let Some(k) = k {
                let mut usage = info.constraint_usage(k);
                usage.set_argv_index(argument);
                usage.set_omit(true);
                argument += 1;
                plan |= PLAN_HAS_K;
            }
            if let Some(limit) = limit {
                let mut usage = info.constraint_usage(limit);
                usage.set_argv_index(argument);
                usage.set_omit(true);
                argument += 1;
                plan |= PLAN_HAS_LIMIT;
            }
            if limit.is_some()
                && let Some(offset) = offset
            {
                let mut usage = info.constraint_usage(offset);
                usage.set_argv_index(argument);
                // SQLite still applies OFFSET after the module produces
                // LIMIT+OFFSET candidates.
                usage.set_omit(false);
                argument += 1;
                plan |= PLAN_HAS_OFFSET;
            }
            if let Some(rowid) = rowid {
                if rowid_is_in && !vtab_in(current_index_info()?, rowid, 1)? {
                    return Ok(false);
                }
                let mut usage = info.constraint_usage(rowid);
                usage.set_argv_index(argument);
                usage.set_omit(true);
                plan |= PLAN_HAS_ROWID_FILTER;
                if rowid_is_in {
                    plan |= PLAN_ROWID_FILTER_IN;
                }
            }
            info.set_order_by_consumed(ordered_by_score_desc);
            info.set_idx_num(plan);
            info.set_idx_str(if rowid.is_some() {
                "knn+rowid-allowlist"
            } else {
                "knn"
            });
            // One KNN call scores every eligible row, so its cost really is
            // proportional to the index. Row count is not: it is the top-k the
            // caller asked for. Reporting a full scan's worth of rows made the
            // planner size joins around this table as if it returned millions.
            let rows = self.estimated_rows();
            info.set_estimated_cost(rows as f64);
            info.set_estimated_rows(planned_k(k, limit).map_or(DEFAULT_KNN_ROWS, |k| k.min(rows)));
        } else if unusable_query {
            // Decline this plan rather than failing the statement. SQLite
            // explores join orders in turn and offers the MATCH as unusable in
            // the ones where the query vector is not yet in scope; an error
            // here killed statements whose *other* orders were perfectly
            // serviceable, which is what blocked `q JOIN v ON v.embedding
            // MATCH q.embedding AND v.k=n`. Returning false lets SQLite try
            // the order that works. When no order works it reports "no query
            // solution" instead of the tailored message, which is the whole
            // cost of this.
            return Ok(false);
        } else if let Some(rowid) = rowid {
            let mut usage = info.constraint_usage(rowid);
            usage.set_argv_index(1);
            usage.set_omit(true);
            info.set_idx_num(PLAN_ROWID);
            info.set_idx_flags(IndexFlags::SQLITE_INDEX_SCAN_UNIQUE);
            info.set_estimated_cost(1.0);
            info.set_estimated_rows(1);
        } else {
            info.set_idx_num(PLAN_FULL_SCAN);
            let rows = self.estimated_rows();
            info.set_estimated_cost(rows as f64);
            info.set_estimated_rows(rows);
        }
        Ok(true)
    }

    fn open(&'vtab mut self) -> Result<Self::Cursor> {
        Ok(TurboVecCursor::default())
    }
}

impl CreateVTab<'_> for TurboVecTable {
    const KIND: VTabKind = VTabKind::Default;

    fn create(
        db: &mut VTabConnection,
        _aux: Option<&Self::Aux>,
        _module_name: &[u8],
        database_name: &[u8],
        table_name: &[u8],
        args: &[&[u8]],
    ) -> Result<(Cow<'static, CStr>, Self)> {
        Ok((
            Cow::Borrowed(c"CREATE TABLE x(embedding BLOB, score REAL HIDDEN, k INTEGER HIDDEN)"),
            Self::make(db, database_name, table_name, args, true)?,
        ))
    }

    fn destroy(&self) -> Result<()> {
        let connection = connection(self.db)?;
        connection.execute_batch(&format!(
            "DROP TABLE IF EXISTS {}; DROP TABLE IF EXISTS {}; \
             DROP TABLE IF EXISTS {}; DROP TABLE IF EXISTS {}",
            self.names.chunks, self.names.ids, self.names.info, self.names.meta
        ))
    }
}

impl UpdateVTab<'_> for TurboVecTable {
    fn delete(&mut self, value: ValueRef<'_>) -> Result<()> {
        let id = value
            .as_i64()
            .map_err(|_| error("rowid must be a SQLite INTEGER"))?;
        let id = u64::try_from(id).map_err(|_| error("rowid must be non-negative"))?;
        let mut guard = self
            .state
            .lock()
            .map_err(|_| error("turbovec0 state lock is poisoned"))?;
        let state = &mut *guard;
        self.refresh(state)?;
        self.prepare_write(state)?;
        let present = match state.layout {
            Layout::Legacy => state.index.as_ref().is_some_and(|index| index.contains(id)),
            Layout::Current => {
                let removed = self.remove_id(id)?;
                if state
                    .index
                    .as_ref()
                    .is_some_and(|index| index.contains(id) != removed)
                {
                    return Err(self.ids_disagree(id));
                }
                removed
            }
        };
        if !present {
            return Err(error(format!("unknown turbovec0 rowid {id}")));
        }
        if state.index.is_some() {
            Self::ensure_destructive_checkpoint(state);
            if !state.index.as_mut().is_some_and(|index| index.remove(id)) {
                return Err(error(format!("unknown turbovec0 rowid {id}")));
            }
        }
        Self::ensure_transaction(state)
            .changes
            .push(Change::Delete { id });
        state.dirty = true;
        Ok(())
    }

    fn insert(&mut self, args: &Inserts<'_>) -> Result<i64> {
        let rowid: Option<i64> = args.get(1)?;
        let rowid = rowid.ok_or_else(|| error("turbovec0 INSERT requires an explicit rowid"))?;
        let id = u64::try_from(rowid).map_err(|_| error("rowid must be non-negative"))?;
        let vector = parse_vector(args.iter().nth(2).expect("embedding column"))
            .map_err(|cause| error(cause.to_string()))?;
        if vector.len() != self.dimensions {
            return Err(error(format!(
                "vector has {} dimensions; turbovec0 requires {}",
                vector.len(),
                self.dimensions
            )));
        }
        let conflict = unsafe { args.on_conflict(self.db) };
        let mut guard = self
            .state
            .lock()
            .map_err(|_| error("turbovec0 state lock is poisoned"))?;
        let state = &mut *guard;
        self.refresh(state)?;
        self.prepare_write(state)?;
        let exists = match state.layout {
            Layout::Legacy => state.index.as_ref().is_some_and(|index| index.contains(id)),
            Layout::Current => {
                // Writes nothing when the rowid exists, so a refused insert
                // leaves `_ids` as it was.
                let exists = !self.add_id(id)?;
                if state
                    .index
                    .as_ref()
                    .is_some_and(|index| index.contains(id) != exists)
                {
                    return Err(self.ids_disagree(id));
                }
                exists
            }
        };
        if exists && conflict != ConflictMode::Replace {
            return Err(sqlite_error(
                ffi::SQLITE_CONSTRAINT_PRIMARYKEY,
                format!("turbovec0 rowid {id} already exists"),
            ));
        }
        let retain_vector = if state.index.is_some() {
            if exists {
                Self::ensure_destructive_checkpoint(state);
            } else {
                Self::ensure_transaction(state);
            }
            let index = state.index.as_mut().expect("checked above");
            if exists {
                index.remove(id);
            }
            index
                .add_with_ids(&vector, &[id])
                .map_err(|cause| error(format!("cannot insert vector: {cause}")))?;
            // After the first delete or replacement every insert keeps its
            // vector. Vector-less inserts then all precede the destructive
            // checkpoint, so a rollback past it only has appends to undo.
            state.base_bytes >= MIN_DELTA_BASE_BYTES
                || exists
                || state.transaction.as_ref().is_some_and(|transaction| {
                    transaction.destructive_checkpoint.is_some()
                        || transaction.changes.iter().any(|change| {
                            matches!(
                                change,
                                Change::Delete { .. } | Change::Insert { replaces: true, .. }
                            )
                        })
                })
        } else {
            // The commit's delta needs the vector, and so does any later
            // load that replays this transaction onto the index.
            true
        };
        Self::ensure_transaction(state)
            .changes
            .push(Change::Insert {
                id,
                vector: retain_vector.then_some(vector),
                replaces: exists,
            });
        state.dirty = true;
        Ok(rowid)
    }

    fn update(&mut self, _args: &Updates<'_>) -> Result<()> {
        Err(sqlite_error(
            ffi::SQLITE_READONLY,
            "turbovec0 does not support UPDATE; DELETE the row and INSERT its replacement",
        ))
    }
}

unsafe extern "C" fn integrity(
    table: *mut ffi::sqlite3_vtab,
    schema: *const c_char,
    name: *const c_char,
    _flags: c_int,
    error_message: *mut *mut c_char,
) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        let table = unsafe { &*table.cast::<TurboVecTable>() };
        match table.integrity() {
            Ok(()) => ffi::SQLITE_OK,
            Err(cause) => {
                if error_message.is_null() {
                    return ffi::SQLITE_ERROR;
                }
                let schema = if schema.is_null() {
                    "?"
                } else {
                    unsafe { CStr::from_ptr(schema) }.to_str().unwrap_or("?")
                };
                let name = if name.is_null() {
                    "?"
                } else {
                    unsafe { CStr::from_ptr(name) }.to_str().unwrap_or("?")
                };
                let message = format!("in turbovec0 {schema}.{name}: {cause}");
                let length = message.len().saturating_add(1);
                let allocation = unsafe { ffi::sqlite3_malloc64(length as u64) }.cast::<u8>();
                if allocation.is_null() {
                    return ffi::SQLITE_NOMEM;
                }
                unsafe {
                    ptr::copy_nonoverlapping(message.as_ptr(), allocation, message.len());
                    *allocation.add(message.len()) = 0;
                    *error_message = allocation.cast();
                }
                ffi::SQLITE_OK
            }
        }
    }))
    .unwrap_or(ffi::SQLITE_ERROR)
}

impl TransactionVTab<'_> for TurboVecTable {
    fn begin(&mut self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| error("turbovec0 state lock is poisoned"))?;
        self.refresh(&mut state)?;
        Self::ensure_transaction(&mut state);
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| error("turbovec0 state lock is poisoned"))?;
        let state = &mut *guard;
        if !state.dirty {
            return Ok(());
        }
        let generation = state
            .generation
            .checked_add(1)
            .ok_or_else(|| error("turbovec0 generation overflow"))?;
        let (synced_changes, change_count) = state
            .transaction
            .as_ref()
            .map(|transaction| (transaction.synced_changes, transaction.changes.len()))
            .ok_or_else(|| error("dirty turbovec0 state has no transaction"))?;
        if synced_changes >= change_count {
            return Err(error("dirty turbovec0 state has no unsynced changes"));
        }
        let connection = connection(self.db)?;
        if state.layout == Layout::Legacy {
            self.upgrade_storage(&connection, state)?;
        }
        let pending = &state
            .transaction
            .as_ref()
            .expect("transaction checked above")
            .changes[synced_changes..];
        let row_count = state
            .row_count
            .checked_add_signed(net_rows(pending))
            .ok_or_else(|| error("turbovec0 row count is out of range"))?;
        if let Some(index) = state.index.as_ref()
            && index.len() != row_count
        {
            return Err(error(format!(
                "turbovec0 index holds {} rows; its metadata and this transaction make {row_count}",
                index.len()
            )));
        }
        let delta_len = delta_encoded_len(pending, self.dimensions)?;
        let next_delta_bytes = state
            .delta_bytes
            .checked_add(delta_len)
            .ok_or_else(|| sqlite_error(ffi::SQLITE_TOOBIG, "turbovec0 delta is too large"))?;
        let next_delta_operations = state
            .delta_operations
            .checked_add(pending.len())
            .ok_or_else(|| error("too many turbovec0 delta operations"))?;
        // The 16 MiB floor keeps small commits cheap on a large index, but it
        // must never exceed the base image itself. A high-dimensional index
        // carries a fixed rotation and codebook cost — an *empty* 1,536-dim
        // table already serializes to 1.67 MiB — so the floor alone let a few
        // thousand rows sit as raw f32 deltas several times larger than the
        // 4-bit base they describe, and every cold open replayed them. Capping
        // the budget at the base keeps deltas from outgrowing what they defer.
        let byte_limit = (state.base_bytes / 4)
            .max(MIN_COMPACTION_DELTA_BYTES)
            .min(state.base_bytes.max(MIN_DELTA_BASE_BYTES));
        let operation_limit = (row_count / 4).max(10_000);
        let compact = state.base_bytes < MIN_DELTA_BASE_BYTES
            || next_delta_bytes >= byte_limit
            || next_delta_operations >= operation_limit;
        if compact {
            // The only commit that needs the compressed codes.
            let index = self.ensure_loaded(state)?;
            let byte_len = write_index(
                &connection,
                &self.database,
                &self.names.chunks,
                &self.names.chunks_table,
                index,
            )?;
            record_compaction(
                &connection,
                &self.names.info,
                generation,
                byte_len,
                row_count,
            )?;
            state.base_bytes = byte_len;
            state.delta_bytes = 0;
            state.delta_operations = 0;
        } else {
            let blob = encode_delta(pending, self.dimensions)?;
            write_delta(&connection, &self.names.chunks, generation, &blob)?;
            record_delta(
                &connection,
                &self.names.info,
                generation,
                row_count,
                next_delta_bytes,
                next_delta_operations,
            )?;
            state.delta_bytes = next_delta_bytes;
            state.delta_operations = next_delta_operations;
        }
        state
            .transaction
            .as_mut()
            .expect("transaction checked above")
            .synced_changes = change_count;
        state.generation = generation;
        state.row_count = row_count;
        state.dirty = false;
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| error("turbovec0 state lock is poisoned"))?;
        if state.dirty {
            return Err(error("turbovec0 commit reached before xSync"));
        }
        state.transaction = None;
        state.dirty = false;
        Ok(())
    }

    fn rollback(&mut self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| error("turbovec0 state lock is poisoned"))?;
        state.dirty = false;
        let Some(transaction) = state.transaction.take() else {
            return Ok(());
        };
        state.generation = transaction.start_generation;
        state.base_bytes = transaction.start_base_bytes;
        state.delta_bytes = transaction.start_delta_bytes;
        state.delta_operations = transaction.start_delta_operations;
        state.row_count = transaction.start_row_count;
        if transaction.upgraded {
            state.layout = Layout::Legacy;
        }
        // SQLite rolls shadow-table writes back around xRollback. Reload on
        // the next use, after that rollback is complete, instead of copying
        // the full index before every potentially destructive transaction.
        state.index = None;
        Ok(())
    }
}

#[derive(Default)]
#[repr(C)]
struct TurboVecCursor<'vtab> {
    base: ffi::sqlite3_vtab_cursor,
    rows: Vec<(u64, Option<f32>)>,
    position: usize,
    phantom: PhantomData<&'vtab TurboVecTable>,
}

unsafe impl VTabCursor for TurboVecCursor<'_> {
    fn filter(&mut self, plan: c_int, _idx_str: Option<&str>, args: &Filters<'_>) -> Result<()> {
        self.position = 0;
        self.rows.clear();
        let table_pointer = self.base.pVtab.cast::<TurboVecTable>();
        // SAFETY: SQLite sets pVtab before xFilter and keeps the virtual table
        // alive until every cursor is closed.
        let table = unsafe { &*table_pointer };
        let mut state = table
            .state
            .lock()
            .map_err(|_| error("turbovec0 state lock is poisoned"))?;
        table.refresh(&mut state)?;

        let rows = match plan & PLAN_KIND_MASK {
            PLAN_FULL_SCAN => table
                .rowids(&mut state)?
                .into_iter()
                .map(|id| (id, None))
                .collect(),
            PLAN_ROWID => {
                let rowid: i64 = args.get(0)?;
                if let Ok(id) = u64::try_from(rowid)
                    && table.contains_rowid(&mut state, id)?
                {
                    vec![(id, None)]
                } else {
                    Vec::new()
                }
            }
            PLAN_KNN => {
                let query = parse_vector(args.iter().next().expect("query argument"))
                    .map_err(|cause| error(cause.to_string()))?;
                if query.len() != table.dimensions {
                    return Err(error(format!(
                        "query has {} dimensions; turbovec0 requires {}",
                        query.len(),
                        table.dimensions
                    )));
                }
                let mut argument = 1;
                let hidden_k = if plan & PLAN_HAS_K != 0 {
                    let value: i64 = args.get(argument)?;
                    argument += 1;
                    Some(
                        usize::try_from(value)
                            .map_err(|_| error("k must be a non-negative SQLite INTEGER"))?,
                    )
                } else {
                    None
                };
                let limit = if plan & PLAN_HAS_LIMIT != 0 {
                    let value: i64 = args.get(argument)?;
                    argument += 1;
                    usize::try_from(value).ok()
                } else {
                    None
                };
                let offset = if plan & PLAN_HAS_OFFSET != 0 {
                    let value: i64 = args.get(argument)?;
                    argument += 1;
                    usize::try_from(value).unwrap_or(0)
                } else {
                    0
                };
                let index = table.ensure_loaded(&mut state)?;
                let allowlist = if plan & PLAN_HAS_ROWID_FILTER != 0 {
                    let mut ids = Vec::new();
                    if plan & PLAN_ROWID_FILTER_IN != 0 {
                        append_in_values(current_filter_argument(argument)?, &mut ids)?;
                        ids.retain(|id| index.contains(*id));
                    } else {
                        let value: i64 = args.get(argument)?;
                        if let Ok(id) = u64::try_from(value)
                            && index.contains(id)
                        {
                            ids.push(id);
                        }
                    }
                    Some(ids)
                } else {
                    None
                };
                let limit_with_offset = limit
                    .map(|limit| limit.saturating_add(offset))
                    .unwrap_or_else(|| index.len());
                let k = hidden_k
                    .map(|k| k.min(limit_with_offset))
                    .unwrap_or(limit_with_offset)
                    .min(index.len());
                if allowlist.as_ref().is_some_and(Vec::is_empty) {
                    return Ok(());
                }
                let results = index
                    .try_search_with_allowlist(&query, k, allowlist.as_deref())
                    .map_err(|cause| error(format!("TurboVec search failed: {cause}")))?;
                results
                    .ids
                    .into_iter()
                    .zip(results.scores.into_iter().map(Some))
                    .collect()
            }
            _ => return Err(error(format!("unknown turbovec0 query plan {plan}"))),
        };
        drop(state);
        self.rows = rows;
        Ok(())
    }

    fn next(&mut self) -> Result<()> {
        self.position += 1;
        Ok(())
    }

    fn eof(&self) -> bool {
        self.position >= self.rows.len()
    }

    fn column(&self, context: &mut Context, column: c_int) -> Result<()> {
        let (_, score) = self
            .rows
            .get(self.position)
            .ok_or_else(|| error("cursor is not positioned on a result row"))?;
        match column {
            COL_EMBEDDING | COL_K => context.set_result(&Null),
            COL_SCORE => context.set_result(score),
            _ => Err(error(format!("unknown turbovec0 column {column}"))),
        }
    }

    fn rowid(&self) -> Result<i64> {
        let (id, _) = self
            .rows
            .get(self.position)
            .ok_or_else(|| error("cursor is not positioned on a result row"))?;
        i64::try_from(*id).map_err(|_| error("TurboVec id is too large for a SQLite rowid"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_virtual_table_geometry() {
        let args: [&[u8]; 2] = [b"dimensions=768", b"bit_width=4"];
        assert_eq!(parse_geometry(&args).unwrap(), (768, 4));

        let missing: [&[u8]; 1] = [b"dimensions=768"];
        assert!(parse_geometry(&missing).is_err());

        let duplicate: [&[u8]; 3] = [b"dimensions=768", b"dimensions=384", b"bit_width=4"];
        assert!(parse_geometry(&duplicate).is_err());

        let unknown: [&[u8]; 3] = [b"dimensions=768", b"bit_width=4", b"metric=cosine"];
        assert!(parse_geometry(&unknown).is_err());
    }

    #[test]
    fn quotes_shadow_table_names() {
        assert_eq!(quote("a\"b"), "\"a\"\"b\"");
        let names = shadow_names("main", "document_vectors");
        assert_eq!(names.meta, "\"main\".\"document_vectors_meta\"");
        assert_eq!(names.info, "\"main\".\"document_vectors_info\"");
        assert_eq!(names.ids, "\"main\".\"document_vectors_ids\"");
        assert_eq!(names.chunks, "\"main\".\"document_vectors_chunks\"");
        assert_eq!(names.chunks_table, "document_vectors_chunks");
    }

    #[test]
    fn decodes_the_delta_marker_in_byte_lengths() {
        assert_eq!(decode_byte_len(4096).unwrap(), (4096, false));
        assert_eq!(decode_byte_len(-4097).unwrap(), (4096, true));
        assert!(decode_byte_len(i64::MIN).is_err());
    }

    #[test]
    fn counts_net_rows_across_replacements() {
        let insert = |id, replaces| Change::Insert {
            id,
            vector: None,
            replaces,
        };
        let changes = [
            insert(1, false),
            insert(2, true),
            Change::Delete { id: 3 },
            Change::Delete { id: 1 },
            insert(1, false),
        ];
        assert_eq!(net_rows(&changes), 0);
        assert_eq!(net_rows(&changes[..1]), 1);
        assert_eq!(net_rows(&changes[2..4]), -2);
    }
}
