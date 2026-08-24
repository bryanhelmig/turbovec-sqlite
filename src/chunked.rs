//! Writable `turbovec0` virtual table with chunked SQLite-owned persistence.

use std::borrow::Cow;
use std::ffi::{CStr, c_char, c_int};
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::{Mutex, OnceLock};

use rusqlite::fallible_iterator::FallibleIterator as _;
use rusqlite::ffi;
use rusqlite::types::{Null, ValueRef};
use rusqlite::vtab::{
    ConflictMode, Context, CreateVTab, Filters, IndexConstraintOp, IndexFlags, IndexInfo, Inserts,
    Module, TransactionVTab, UpdateVTab, Updates, VTab, VTabConfig, VTabConnection, VTabCursor,
    VTabKind, escape_double_quote, parameter,
};
use rusqlite::{Connection, Error, Result, params};
use turbovec::IdMapIndex;

use crate::parse_vector;

const MODULE_NAME: &CStr = c"turbovec0";
const CHUNK_SIZE: usize = 4 * 1024 * 1024;

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
        module.xIntegrity = Some(integrity);
        Box::into_raw(Box::new(module)) as usize
    });
    pointer as *const ffi::sqlite3_module
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

fn read_index(connection: &Connection, meta: &str, chunks: &str) -> Result<(i64, IdMapIndex)> {
    let (generation, expected_len): (i64, i64) = connection.query_row(
        &format!("SELECT generation, byte_len FROM {meta} WHERE id=1"),
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let expected_len = usize::try_from(expected_len)
        .map_err(|_| error("negative or oversized persisted TurboVec byte length"))?;
    let mut statement =
        connection.prepare(&format!("SELECT data FROM {chunks} ORDER BY chunk_id"))?;
    let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    let mut payload = Vec::with_capacity(expected_len);
    for row in rows {
        payload.extend_from_slice(&row?);
        if payload.len() > expected_len {
            return Err(error(
                "TurboVec chunk payload exceeds its declared byte length",
            ));
        }
    }
    if payload.len() != expected_len {
        return Err(error(format!(
            "TurboVec chunks contain {} bytes; metadata declares {expected_len}",
            payload.len()
        )));
    }
    let index = IdMapIndex::from_bytes(&payload)
        .map_err(|cause| error(format!("invalid chunked TurboVec index: {cause}")))?;
    Ok((generation, index))
}

fn write_index(
    connection: &Connection,
    database: &str,
    meta: &str,
    chunks: &str,
    chunks_table: &str,
    generation: i64,
    index: &IdMapIndex,
) -> Result<()> {
    let payload = index.to_bytes();
    let existing: Vec<Vec<u8>> = {
        let mut statement =
            connection.prepare(&format!("SELECT data FROM {chunks} ORDER BY chunk_id"))?;
        statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<_>>()?
    };

    let pieces: Vec<&[u8]> = payload.chunks(CHUNK_SIZE).collect();
    let upsert = format!(
        "INSERT INTO {chunks}(chunk_id, data) VALUES (?1, ?2) \
         ON CONFLICT(chunk_id) DO UPDATE SET data=excluded.data"
    );
    for (chunk_id, piece) in pieces.iter().enumerate() {
        if let Some(old) = existing.get(chunk_id) {
            if old == piece {
                continue;
            }
            if old.len() == piece.len() {
                let start = old
                    .iter()
                    .zip(*piece)
                    .position(|(before, after)| before != after)
                    .expect("different equal-length chunks have a first difference");
                let end = old
                    .iter()
                    .zip(*piece)
                    .rposition(|(before, after)| before != after)
                    .expect("different equal-length chunks have a last difference")
                    + 1;
                let mut blob =
                    connection.blob_open(database, chunks_table, "data", chunk_id as i64, false)?;
                blob.write_at(&piece[start..end], start)?;
                blob.close()?;
                continue;
            }
        }
        connection.execute(&upsert, params![chunk_id as i64, piece])?;
    }
    connection.execute(
        &format!("DELETE FROM {chunks} WHERE chunk_id >= ?1"),
        [pieces.len() as i64],
    )?;
    connection.execute(
        &format!("UPDATE {meta} SET generation=?1, byte_len=?2 WHERE id=1"),
        params![generation, payload.len() as i64],
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
    transaction_snapshot: Option<(i64, Vec<u8>)>,
    savepoints: Vec<(c_int, Vec<u8>, bool)>,
    dirty: bool,
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
        db.config(VTabConfig::ConstraintSupport)?;
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

        let (generation, index) = if create {
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
            write_index(
                &connection,
                &database,
                &meta,
                &chunks,
                &chunks_table,
                0,
                &index,
            )?;
            (0, index)
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
            validate_geometry(&loaded.1, dimensions, bit_width)?;
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
                generation,
                index,
                transaction_snapshot: None,
                savepoints: Vec::new(),
                dirty: false,
            }),
        })
    }

    fn refresh<'a>(&self, state: &'a mut State) -> Result<&'a mut State> {
        if state.dirty {
            return Ok(state);
        }
        let connection = connection(self.db)?;
        let persisted_generation = read_generation(&connection, &self.meta)?;
        if persisted_generation != state.generation {
            let (generation, index) = read_index(&connection, &self.meta, &self.chunks)?;
            state.generation = generation;
            state.index = index;
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
        let (_, index) = read_index(&connection, &self.meta, &self.chunks)?;
        let (dimensions, bit_width): (i64, i64) = connection.query_row(
            &format!("SELECT dimensions, bit_width FROM {} WHERE id=1", self.meta),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let dimensions = usize::try_from(dimensions)
            .map_err(|_| error("invalid dimension in turbovec0 metadata"))?;
        let bit_width = usize::try_from(bit_width)
            .map_err(|_| error("invalid bit width in turbovec0 metadata"))?;
        validate_geometry(&index, dimensions, bit_width)
    }

    fn mutate(&self, operation: impl FnOnce(&mut IdMapIndex) -> Result<()>) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| error("turbovec0 state lock is poisoned"))?;
        self.refresh(&mut state)?;
        operation(&mut state.index)?;
        state.dirty = true;
        Ok(())
    }

    fn savepoint(&mut self, id: c_int) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| error("turbovec0 state lock is poisoned"))?;
        let payload = state.index.to_bytes();
        let dirty = state.dirty;
        state.savepoints.retain(|(existing, _, _)| *existing < id);
        state.savepoints.push((id, payload, dirty));
        Ok(())
    }

    fn release(&mut self, id: c_int) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| error("turbovec0 state lock is poisoned"))?;
        state.savepoints.retain(|(existing, _, _)| *existing < id);
        Ok(())
    }

    fn rollback_to(&mut self, id: c_int) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| error("turbovec0 state lock is poisoned"))?;
        let (_, payload, dirty) = state
            .savepoints
            .iter()
            .rev()
            .find(|(existing, _, _)| *existing == id)
            .cloned()
            .ok_or_else(|| error(format!("unknown turbovec0 savepoint {id}")))?;
        state.index = IdMapIndex::from_bytes(&payload)
            .map_err(|cause| error(format!("cannot restore savepoint: {cause}")))?;
        state.dirty = dirty;
        state.savepoints.retain(|(existing, _, _)| *existing <= id);
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
                    rowid_is_in = info.is_in_constraint(index)?;
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
                if rowid_is_in && !info.set_in_constraint(rowid, true)? {
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
        self.mutate(|index| {
            if index.remove(id) {
                Ok(())
            } else {
                Err(error(format!("unknown turbovec0 rowid {id}")))
            }
        })
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
        let exists = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| error("turbovec0 state lock is poisoned"))?;
            self.refresh(&mut state)?;
            state.index.contains(id)
        };
        if exists && conflict == ConflictMode::Ignore {
            return Ok(rowid);
        }
        if exists && conflict != ConflictMode::Replace {
            return Err(sqlite_error(
                ffi::SQLITE_CONSTRAINT_PRIMARYKEY,
                format!("turbovec0 rowid {id} already exists"),
            ));
        }
        self.mutate(|index| {
            if exists && conflict == ConflictMode::Replace {
                index.remove(id);
            }
            index
                .add_with_ids(&vector, &[id])
                .map_err(|cause| error(format!("cannot insert vector: {cause}")))
        })?;
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
        if state.transaction_snapshot.is_none() {
            state.transaction_snapshot = Some((state.generation, state.index.to_bytes()));
        }
        state.savepoints.clear();
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
        let connection = connection(self.db)?;
        write_index(
            &connection,
            &self.database,
            &self.meta,
            &self.chunks,
            &self.chunks_table,
            generation,
            &state.index,
        )?;
        state.generation = generation;
        state.dirty = false;
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| error("turbovec0 state lock is poisoned"))?;
        state.transaction_snapshot = None;
        state.savepoints.clear();
        state.dirty = false;
        Ok(())
    }

    fn rollback(&mut self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| error("turbovec0 state lock is poisoned"))?;
        if let Some((generation, payload)) = state.transaction_snapshot.take() {
            state.index = IdMapIndex::from_bytes(&payload)
                .map_err(|cause| error(format!("cannot restore rolled-back index: {cause}")))?;
            state.generation = generation;
        }
        state.savepoints.clear();
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
                        let mut values = args.in_values(argument)?;
                        while let Some(value) = values.next()? {
                            match value {
                                ValueRef::Integer(value) => {
                                    if let Ok(id) = u64::try_from(value)
                                        && state.index.contains(id)
                                    {
                                        ids.push(id);
                                    }
                                }
                                ValueRef::Null => {}
                                _ => {
                                    return Err(error(
                                        "rowid allowlist values must be SQLite INTEGERs",
                                    ));
                                }
                            }
                        }
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
