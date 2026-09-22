//! Writable `turbovec0` virtual table with chunked SQLite-owned persistence.

use std::borrow::Cow;
use std::cell::Cell;
use std::ffi::{CStr, c_char, c_int};
use std::io::{self, Write};
use std::marker::PhantomData;
use std::mem::size_of;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::{Mutex, OnceLock};

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
const CHUNK_SIZE: usize = 4 * 1024 * 1024;
const DELTA_MAGIC: &[u8; 4] = b"TVD1";
const DELTA_HEADER_LEN: usize = 12;
const DELTA_CHECKSUM_LEN: usize = 4;
const MIN_DELTA_BASE_BYTES: usize = 1024 * 1024;
const MIN_COMPACTION_DELTA_BYTES: usize = 16 * 1024 * 1024;

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

// sqlite3_api_routines pointer slots from SQLite's stable sqlite3ext.h ABI.
// The IN callbacks were appended in SQLite 3.38; this project supports 3.44+.
const API_LIBVERSION_NUMBER: usize = 67;
const API_VTAB_CONFIG: usize = 177;
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
type VtabInIterCallback =
    unsafe extern "C" fn(*mut ffi::sqlite3_value, *mut *mut ffi::sqlite3_value) -> c_int;

#[repr(C)]
struct ModuleV4 {
    module: ffi::sqlite3_module,
    x_integrity: Option<IntegrityCallback>,
}

static ORIGINAL_BEST_INDEX: OnceLock<usize> = OnceLock::new();
static ORIGINAL_FILTER: OnceLock<usize> = OnceLock::new();
static VTAB_CONFIG: OnceLock<usize> = OnceLock::new();
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
    if unsafe { version() } < 3_038_000 {
        return;
    }
    for (slot, destination) in [
        (API_VTAB_CONFIG, &VTAB_CONFIG),
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
    c_int::from(name.eq_ignore_ascii_case(b"meta") || name.eq_ignore_ascii_case(b"chunks"))
}

unsafe fn set_vtab_error(table: *mut ffi::sqlite3_vtab, message: &str) -> c_int {
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

fn names(database: &[u8], table: &[u8]) -> Result<(String, String)> {
    let database = std::str::from_utf8(database)?;
    let table = std::str::from_utf8(table)?;
    Ok((
        format!("{}.{}", quote(database), quote(&format!("{table}_meta"))),
        format!("{}.{}", quote(database), quote(&format!("{table}_chunks"))),
    ))
}

fn names_str(database: &str, table: &str) -> (String, String) {
    (
        format!("{}.{}", quote(database), quote(&format!("{table}_meta"))),
        format!("{}.{}", quote(database), quote(&format!("{table}_chunks"))),
    )
}

fn connection(handle: *mut ffi::sqlite3) -> Result<Connection> {
    // SAFETY: the virtual table never owns the SQLite connection. Rusqlite's
    // from_handle() creates a non-owning facade and will not close it on drop.
    unsafe { Connection::from_handle(handle) }
}

fn read_generation(connection: &Connection, meta: &str) -> Result<i64> {
    connection.query_row(
        &format!("SELECT generation FROM {meta} WHERE id=1"),
        [],
        |row| row.get(0),
    )
}

fn read_payload(connection: &Connection, meta: &str, chunks: &str) -> Result<(i64, Vec<u8>, bool)> {
    let (generation, stored_len): (i64, i64) = connection.query_row(
        &format!("SELECT generation, byte_len FROM {meta} WHERE id=1"),
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let has_deltas = stored_len < 0;
    let expected_len = if has_deltas {
        stored_len
            .checked_neg()
            .and_then(|length| length.checked_sub(1))
            .ok_or_else(|| error("invalid persisted TurboVec byte length"))?
    } else {
        stored_len
    };
    let expected_len = usize::try_from(expected_len)
        .map_err(|_| error("negative or oversized persisted TurboVec byte length"))?;
    // Check storage geometry before allocating from untrusted metadata. SQLite
    // can read BLOB lengths without loading their contents into Rust buffers.
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

    let mut payload = Vec::new();
    payload
        .try_reserve_exact(actual_len)
        .map_err(|_| sqlite_error(ffi::SQLITE_NOMEM, "cannot allocate TurboVec chunk payload"))?;
    let mut statement = connection.prepare(&format!(
        "SELECT chunk_id, data FROM {chunks} WHERE chunk_id>=0 ORDER BY chunk_id"
    ))?;
    let mut rows = statement.query([])?;
    let mut chunk_id = 0_i64;
    while let Some(row) = rows.next()? {
        let stored_id: i64 = row.get(0)?;
        let piece = row.get_ref(1)?.as_blob()?;
        let remaining = expected_len - payload.len();
        if stored_id != chunk_id || piece.len() != remaining.min(CHUNK_SIZE) || piece.is_empty() {
            return Err(error("TurboVec chunks changed while reading their payload"));
        }
        payload.extend_from_slice(piece);
        chunk_id += 1;
    }
    if payload.len() != expected_len {
        return Err(error(format!(
            "TurboVec chunks contain {} bytes; metadata declares {expected_len}",
            payload.len()
        )));
    }
    Ok((generation, payload, has_deltas))
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

fn read_index(connection: &Connection, meta: &str, chunks: &str) -> Result<LoadedIndex> {
    let (generation, payload, expects_deltas) = read_payload(connection, meta, chunks)?;
    check_format(&payload)
        .map_err(|cause| error(format!("cannot open turbovec0 index: {cause}")))?;
    let mut index = IdMapIndex::from_bytes(&payload)
        .map_err(|cause| error(format!("invalid chunked TurboVec index: {cause}")))?;
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
        base_bytes: payload.len(),
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

    let (meta, chunks) = names_str(database, table);
    let loaded = read_index(connection, &meta, &chunks)?;
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

fn write_index(
    connection: &Connection,
    database: &str,
    meta: &str,
    chunks: &str,
    chunks_table: &str,
    generation: i64,
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
    connection.execute(
        &format!("UPDATE {meta} SET generation=?1, byte_len=?2 WHERE id=1"),
        params![generation, byte_len],
    )?;
    usize::try_from(byte_len).map_err(|_| error("invalid serialized turbovec0 byte length"))
}

fn write_delta(
    connection: &Connection,
    meta: &str,
    chunks: &str,
    generation: i64,
    blob: &[u8],
) -> Result<()> {
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
    connection.execute(
        &format!(
            "UPDATE {meta} SET generation=?1, \
             byte_len=CASE WHEN byte_len>=0 THEN -byte_len-1 ELSE byte_len END WHERE id=1"
        ),
        [generation],
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
    generation: i64,
    index: IdMapIndex,
    transaction: Option<TransactionState>,
    dirty: bool,
    base_bytes: usize,
    delta_bytes: usize,
    delta_operations: usize,
    needs_reload: bool,
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
    changes: Vec<Change>,
    destructive_checkpoint: Option<DestructiveCheckpoint>,
    savepoints: Vec<(c_int, usize)>,
    synced_changes: usize,
}

impl TransactionState {
    fn new(
        start_generation: i64,
        start_base_bytes: usize,
        start_delta_bytes: usize,
        start_delta_operations: usize,
    ) -> Self {
        Self {
            start_generation,
            start_base_bytes,
            start_delta_bytes,
            start_delta_operations,
            changes: Vec::new(),
            destructive_checkpoint: None,
            savepoints: Vec::new(),
            synced_changes: 0,
        }
    }
}

#[repr(C)]
struct TurboVecTable {
    base: ffi::sqlite3_vtab,
    db: *mut ffi::sqlite3,
    database: String,
    dimensions: usize,
    meta: String,
    chunks: String,
    chunks_table: String,
    state: Mutex<State>,
}

impl TurboVecTable {
    fn configure(db: &mut VTabConnection) -> Result<()> {
        // SQLITE_VTAB_CONSTRAINT_SUPPORT is the only config option here with
        // a variadic third argument. Rusqlite 0.40's convenience method omits
        // it, so call the host API-table function directly.
        let pointer = *VTAB_CONFIG
            .get()
            .ok_or_else(|| error("SQLite virtual-table config callback is unavailable"))?;
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
        let table = std::str::from_utf8(table_name)?;
        let (meta, chunks) = names(database_name, table_name)?;
        let chunks_table = format!("{table}_chunks");
        let handle = unsafe { db.handle() };
        let connection = connection(handle)?;

        let loaded = if create {
            let index = IdMapIndex::new(dimensions, bit_width)
                .map_err(|cause| error(format!("invalid turbovec0 geometry: {cause}")))?;
            connection.execute_batch(&format!(
                "CREATE TABLE {meta}(\
                   id INTEGER PRIMARY KEY CHECK(id=1),\
                   dimensions INTEGER NOT NULL,\
                   bit_width INTEGER NOT NULL,\
                   generation INTEGER NOT NULL,\
                   byte_len INTEGER NOT NULL\
                 );\
                 CREATE TABLE {chunks}(\
                   chunk_id INTEGER PRIMARY KEY,\
                   data BLOB NOT NULL\
                 );"
            ))?;
            connection.execute(
                &format!(
                    "INSERT INTO {meta}(id, dimensions, bit_width, generation, byte_len) \
                     VALUES (1, ?1, ?2, 0, 0)"
                ),
                params![dimensions as i64, bit_width as i64],
            )?;
            let base_bytes = write_index(
                &connection,
                &database,
                &meta,
                &chunks,
                &chunks_table,
                0,
                &index,
            )?;
            LoadedIndex {
                generation: 0,
                index,
                base_bytes,
                delta_bytes: 0,
                delta_operations: 0,
            }
        } else {
            let stored: (i64, i64) = connection.query_row(
                &format!("SELECT dimensions, bit_width FROM {meta} WHERE id=1"),
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
            let loaded = read_index(&connection, &meta, &chunks)?;
            validate_geometry(&loaded.index, dimensions, bit_width)?;
            loaded
        };

        Ok(Self {
            base: ffi::sqlite3_vtab::default(),
            db: handle,
            database,
            dimensions,
            meta,
            chunks,
            chunks_table,
            state: Mutex::new(State {
                generation: loaded.generation,
                index: loaded.index,
                transaction: None,
                dirty: false,
                base_bytes: loaded.base_bytes,
                delta_bytes: loaded.delta_bytes,
                delta_operations: loaded.delta_operations,
                needs_reload: false,
            }),
        })
    }

    fn refresh<'a>(&self, state: &'a mut State) -> Result<&'a mut State> {
        if state.transaction.is_some() {
            return Ok(state);
        }
        let connection = connection(self.db)?;
        let persisted_generation = read_generation(&connection, &self.meta)?;
        if state.needs_reload || persisted_generation != state.generation {
            let loaded = read_index(&connection, &self.meta, &self.chunks)?;
            state.generation = loaded.generation;
            state.index = loaded.index;
            state.base_bytes = loaded.base_bytes;
            state.delta_bytes = loaded.delta_bytes;
            state.delta_operations = loaded.delta_operations;
            state.needs_reload = false;
        }
        Ok(state)
    }

    fn rename(&mut self, new_name: &str) -> Result<()> {
        let (new_meta, new_chunks) = names_str(&self.database, new_name);
        let connection = connection(self.db)?;
        connection.execute_batch(&format!(
            "ALTER TABLE {} RENAME TO {}; ALTER TABLE {} RENAME TO {}",
            self.meta,
            quote(&format!("{new_name}_meta")),
            self.chunks,
            quote(&format!("{new_name}_chunks")),
        ))?;
        self.meta = new_meta;
        self.chunks = new_chunks;
        self.chunks_table = format!("{new_name}_chunks");
        Ok(())
    }

    fn integrity(&self) -> Result<()> {
        let connection = connection(self.db)?;
        let loaded = read_index(&connection, &self.meta, &self.chunks)?;
        let (dimensions, bit_width): (i64, i64) = connection.query_row(
            &format!("SELECT dimensions, bit_width FROM {} WHERE id=1", self.meta),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let dimensions = usize::try_from(dimensions)
            .map_err(|_| error("invalid dimension in turbovec0 metadata"))?;
        let bit_width = usize::try_from(bit_width)
            .map_err(|_| error("invalid bit width in turbovec0 metadata"))?;
        validate_geometry(&loaded.index, dimensions, bit_width)
    }

    fn ensure_transaction(state: &mut State) -> &mut TransactionState {
        let start_generation = state.generation;
        let start_base_bytes = state.base_bytes;
        let start_delta_bytes = state.delta_bytes;
        let start_delta_operations = state.delta_operations;
        state.transaction.get_or_insert_with(|| {
            TransactionState::new(
                start_generation,
                start_base_bytes,
                start_delta_bytes,
                start_delta_operations,
            )
        })
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
        if !needs_checkpoint {
            return;
        }
        let payload = state.index.to_bytes();
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
            let Change::Insert { id, .. } = change else {
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
            state.index = IdMapIndex::from_bytes(&checkpoint.payload)
                .map_err(|cause| error(format!("cannot restore turbovec0 checkpoint: {cause}")))?;
            if change_index < checkpoint.change_index {
                Self::undo_insert_prefix(
                    &mut state.index,
                    &transaction.changes[change_index..checkpoint.change_index],
                )?;
            } else {
                Self::replay(
                    &mut state.index,
                    &transaction.changes[checkpoint.change_index..change_index],
                )?;
                transaction.destructive_checkpoint = Some(checkpoint);
            }
        } else {
            let tail_is_append_only = transaction.changes[change_index..].iter().all(|change| {
                matches!(
                    change,
                    Change::Insert {
                        replaces: false,
                        ..
                    }
                )
            });
            if tail_is_append_only {
                Self::undo_insert_prefix(&mut state.index, &transaction.changes[change_index..])?;
            } else {
                if transaction.synced_changes != 0 {
                    state.transaction = Some(transaction);
                    return Err(error("cannot roll back a turbovec0 savepoint after xSync"));
                }
                let connection = connection(self.db)?;
                let loaded = read_index(&connection, &self.meta, &self.chunks)?;
                if loaded.generation != transaction.start_generation {
                    state.transaction = Some(transaction);
                    return Err(error("turbovec0 changed while restoring a savepoint"));
                }
                state.index = loaded.index;
                Self::replay(&mut state.index, &transaction.changes[..change_index])?;
            }
        }
        transaction.changes.truncate(change_index);
        transaction.synced_changes = transaction.synced_changes.min(change_index);
        state.dirty = !transaction.changes.is_empty();
        state.transaction = Some(transaction);
        Ok(())
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
            info.set_estimated_cost(
                self.state
                    .lock()
                    .map_or(1_000_000.0, |s| s.index.len() as f64),
            );
            info.set_estimated_rows(10);
        } else if unusable_query {
            return Err(error(
                "turbovec0 MATCH query is not constant for this scan; use a bound value or scalar subquery",
            ));
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
            let rows = self
                .state
                .lock()
                .map_or(1_000_000, |s| s.index.len() as i64);
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
            "DROP TABLE IF EXISTS {}; DROP TABLE IF EXISTS {}",
            self.chunks, self.meta
        ))
    }
}

impl UpdateVTab<'_> for TurboVecTable {
    fn delete(&mut self, value: ValueRef<'_>) -> Result<()> {
        let id = value
            .as_i64()
            .map_err(|_| error("rowid must be a SQLite INTEGER"))?;
        let id = u64::try_from(id).map_err(|_| error("rowid must be non-negative"))?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| error("turbovec0 state lock is poisoned"))?;
        self.refresh(&mut state)?;
        if !state.index.contains(id) {
            return Err(error(format!("unknown turbovec0 rowid {id}")));
        }
        Self::ensure_destructive_checkpoint(&mut state);
        if !state.index.remove(id) {
            return Err(error(format!("unknown turbovec0 rowid {id}")));
        }
        state
            .transaction
            .as_mut()
            .expect("destructive checkpoint creates a transaction")
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
        let mut state = self
            .state
            .lock()
            .map_err(|_| error("turbovec0 state lock is poisoned"))?;
        self.refresh(&mut state)?;
        let exists = state.index.contains(id);
        if exists && conflict != ConflictMode::Replace {
            return Err(sqlite_error(
                ffi::SQLITE_CONSTRAINT_PRIMARYKEY,
                format!("turbovec0 rowid {id} already exists"),
            ));
        }
        if exists {
            Self::ensure_destructive_checkpoint(&mut state);
            state.index.remove(id);
        } else {
            Self::ensure_transaction(&mut state);
        }
        state
            .index
            .add_with_ids(&vector, &[id])
            .map_err(|cause| error(format!("cannot insert vector: {cause}")))?;
        let retain_vector = state.base_bytes >= MIN_DELTA_BASE_BYTES
            || exists
            || state.transaction.as_ref().is_some_and(|transaction| {
                transaction.destructive_checkpoint.is_some()
                    || transaction
                        .changes
                        .iter()
                        .any(|change| matches!(change, Change::Delete { .. }))
            });
        let transaction = state
            .transaction
            .as_mut()
            .expect("insert creates a transaction");
        transaction.changes.push(Change::Insert {
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
        if state.transaction.is_none() {
            state.transaction = Some(TransactionState::new(
                state.generation,
                state.base_bytes,
                state.delta_bytes,
                state.delta_operations,
            ));
        }
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| error("turbovec0 state lock is poisoned"))?;
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
        let pending = &state
            .transaction
            .as_ref()
            .expect("transaction checked above")
            .changes[synced_changes..];
        let delta_len = delta_encoded_len(pending, self.dimensions)?;
        let next_delta_bytes = state
            .delta_bytes
            .checked_add(delta_len)
            .ok_or_else(|| sqlite_error(ffi::SQLITE_TOOBIG, "turbovec0 delta is too large"))?;
        let next_delta_operations = state
            .delta_operations
            .checked_add(pending.len())
            .ok_or_else(|| error("too many turbovec0 delta operations"))?;
        let byte_limit = (state.base_bytes / 4).max(MIN_COMPACTION_DELTA_BYTES);
        let operation_limit = (state.index.len() / 4).max(10_000);
        let compact = state.base_bytes < MIN_DELTA_BASE_BYTES
            || next_delta_bytes >= byte_limit
            || next_delta_operations >= operation_limit;
        let connection = connection(self.db)?;
        if compact {
            state.base_bytes = write_index(
                &connection,
                &self.database,
                &self.meta,
                &self.chunks,
                &self.chunks_table,
                generation,
                &state.index,
            )?;
            state.delta_bytes = 0;
            state.delta_operations = 0;
        } else {
            let blob = encode_delta(pending, self.dimensions)?;
            write_delta(&connection, &self.meta, &self.chunks, generation, &blob)?;
            state.delta_bytes = next_delta_bytes;
            state.delta_operations = next_delta_operations;
        }
        state
            .transaction
            .as_mut()
            .expect("transaction checked above")
            .synced_changes = change_count;
        state.generation = generation;
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
        let Some(transaction) = state.transaction.as_ref() else {
            state.dirty = false;
            return Ok(());
        };
        let start_generation = transaction.start_generation;
        let start_base_bytes = transaction.start_base_bytes;
        let start_delta_bytes = transaction.start_delta_bytes;
        let start_delta_operations = transaction.start_delta_operations;
        state.transaction = None;
        state.generation = start_generation;
        state.base_bytes = start_base_bytes;
        state.delta_bytes = start_delta_bytes;
        state.delta_operations = start_delta_operations;
        // SQLite rolls shadow-table writes back around xRollback. Reload on
        // the next use, after that rollback is complete, instead of copying
        // the full index before every potentially destructive transaction.
        state.needs_reload = true;
        state.dirty = false;
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
            PLAN_FULL_SCAN => state.index.iter_ids().map(|id| (id, None)).collect(),
            PLAN_ROWID => {
                let rowid: i64 = args.get(0)?;
                if let Ok(id) = u64::try_from(rowid)
                    && state.index.contains(id)
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
                let allowlist = if plan & PLAN_HAS_ROWID_FILTER != 0 {
                    let mut ids = Vec::new();
                    if plan & PLAN_ROWID_FILTER_IN != 0 {
                        append_in_values(current_filter_argument(argument)?, &mut ids)?;
                        ids.retain(|id| state.index.contains(*id));
                    } else {
                        let value: i64 = args.get(argument)?;
                        if let Ok(id) = u64::try_from(value)
                            && state.index.contains(id)
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
                    .unwrap_or_else(|| state.index.len());
                let k = hidden_k
                    .map(|k| k.min(limit_with_offset))
                    .unwrap_or(limit_with_offset)
                    .min(state.index.len());
                if allowlist.as_ref().is_some_and(Vec::is_empty) {
                    return Ok(());
                }
                let results = state
                    .index
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
        assert_eq!(
            names_str("main", "document_vectors"),
            (
                "\"main\".\"document_vectors_meta\"".to_owned(),
                "\"main\".\"document_vectors_chunks\"".to_owned(),
            )
        );
    }
}
