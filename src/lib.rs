//! SQLite loadable extension backed by TurboVec.
//!
//! `turbovec0` is the recommended writable virtual table. It keeps a warmed
//! `IdMapIndex` and persists it in fixed-size SQLite shadow-table chunks. The
//! scalar BLOB functions remain as a small reference implementation and useful
//! baseline.

mod chunked;

use std::borrow::Cow;
use std::error::Error as StdError;
use std::ffi::{CStr, c_char, c_int};
use std::marker::PhantomData;

use rusqlite::ffi;
use rusqlite::functions::{Aggregate, Context as FunctionContext, FunctionFlags};
use rusqlite::types::{Null, ValueRef};
use rusqlite::vtab::{
    Context, Filters, IndexConstraintOp, IndexInfo, Module, VTab, VTabConfig, VTabConnection,
    VTabCursor,
};
use rusqlite::{Connection, Error, Result};
use turbovec::IdMapIndex;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MODULE_NAME: &CStr = c"turbovec_knn";

const COL_ID: c_int = 0;
const COL_SCORE: c_int = 1;
const COL_INDEX: c_int = 2;
const COL_QUERY: c_int = 3;
const COL_K: c_int = 4;

const PLAN_KNN: c_int = 1;

fn function_error(message: impl Into<String>) -> Error {
    #[derive(Debug)]
    struct Message(String);

    impl std::fmt::Display for Message {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }

    impl StdError for Message {}

    Error::UserFunctionError(Box::new(Message(message.into())))
}

fn module_error(message: impl Into<String>) -> Error {
    Error::ModuleError(message.into())
}

fn checked_usize(value: i64, label: &str) -> Result<usize> {
    usize::try_from(value).map_err(|_| function_error(format!("{label} must be non-negative")))
}

fn checked_id(value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| function_error("id must be a non-negative SQLite INTEGER"))
}

fn parse_vector(value: ValueRef<'_>) -> Result<Vec<f32>> {
    let vector: Vec<f32> = match value {
        ValueRef::Blob(bytes) => {
            if bytes.len() % size_of::<f32>() != 0 {
                return Err(function_error(format!(
                    "float32 vector BLOB length must be divisible by 4, got {} bytes",
                    bytes.len()
                )));
            }
            bytes
                .chunks_exact(size_of::<f32>())
                .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("four-byte chunk")))
                .collect()
        }
        ValueRef::Text(json) => serde_json::from_slice(json)
            .map_err(|error| function_error(format!("invalid JSON float vector: {error}")))?,
        other => {
            return Err(function_error(format!(
                "vector must be a float32 BLOB or JSON array, got {:?}",
                other.data_type()
            )));
        }
    };
    if let Some((coordinate, value)) = vector
        .iter()
        .enumerate()
        .find(|(_, value)| !value.is_finite() || value.abs() >= 1.0e16)
    {
        return Err(function_error(format!(
            "invalid vector coordinate {coordinate}: {value} must be finite and have magnitude below 1e16"
        )));
    }
    Ok(vector)
}

fn vector_blob(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(std::mem::size_of_val(vector));
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn load_index(bytes: &[u8]) -> Result<IdMapIndex> {
    IdMapIndex::from_bytes(bytes)
        .map_err(|error| function_error(format!("invalid TurboVec index BLOB: {error}")))
}

fn validate_vector_dim(index: &IdMapIndex, vector: &[f32]) -> Result<()> {
    let dim = index
        .dim_opt()
        .ok_or_else(|| function_error("TurboVec index has no committed dimension"))?;
    if vector.len() != dim {
        return Err(function_error(format!(
            "vector has {} dimensions; index requires {dim}",
            vector.len()
        )));
    }
    Ok(())
}

fn version(_ctx: &FunctionContext<'_>) -> Result<&'static str> {
    Ok(VERSION)
}

fn f32_blob(ctx: &FunctionContext<'_>) -> Result<Vec<u8>> {
    let vector = parse_vector(ctx.get_raw(0))?;
    Ok(vector_blob(&vector))
}

fn new_index(ctx: &FunctionContext<'_>) -> Result<Vec<u8>> {
    let dim = checked_usize(ctx.get(0)?, "dimensions")?;
    let bit_width = checked_usize(ctx.get(1)?, "bit_width")?;
    let index = IdMapIndex::new(dim, bit_width)
        .map_err(|error| function_error(format!("cannot create TurboVec index: {error}")))?;
    Ok(index.to_bytes())
}

fn add_vector(ctx: &FunctionContext<'_>) -> Result<Vec<u8>> {
    let payload: Vec<u8> = ctx.get(0)?;
    let id = checked_id(ctx.get(1)?)?;
    let vector = parse_vector(ctx.get_raw(2))?;
    let mut index = load_index(&payload)?;
    validate_vector_dim(&index, &vector)?;
    index
        .add_with_ids(&vector, &[id])
        .map_err(|error| function_error(format!("cannot add vector: {error}")))?;
    Ok(index.to_bytes())
}

fn remove_vector(ctx: &FunctionContext<'_>) -> Result<Vec<u8>> {
    let payload: Vec<u8> = ctx.get(0)?;
    let id = checked_id(ctx.get(1)?)?;
    let mut index = load_index(&payload)?;
    if !index.remove(id) {
        return Err(function_error(format!("cannot remove unknown id {id}")));
    }
    Ok(index.to_bytes())
}

fn index_len(ctx: &FunctionContext<'_>) -> Result<i64> {
    let payload: Vec<u8> = ctx.get(0)?;
    i64::try_from(load_index(&payload)?.len())
        .map_err(|_| function_error("index length does not fit in a SQLite INTEGER"))
}

fn index_dim(ctx: &FunctionContext<'_>) -> Result<i64> {
    let payload: Vec<u8> = ctx.get(0)?;
    let dim = load_index(&payload)?
        .dim_opt()
        .ok_or_else(|| function_error("TurboVec index has no committed dimension"))?;
    i64::try_from(dim).map_err(|_| function_error("index dimension does not fit in an INTEGER"))
}

fn index_bit_width(ctx: &FunctionContext<'_>) -> Result<i64> {
    let payload: Vec<u8> = ctx.get(0)?;
    i64::try_from(load_index(&payload)?.bit_width())
        .map_err(|_| function_error("bit width does not fit in an INTEGER"))
}

struct BuildAggregate;

struct BuildState {
    index: IdMapIndex,
    dim: usize,
    bit_width: usize,
}

impl Aggregate<BuildState, Vec<u8>> for BuildAggregate {
    fn init(&self, ctx: &mut FunctionContext<'_>) -> Result<BuildState> {
        let dim = checked_usize(ctx.get(0)?, "dimensions")?;
        let bit_width = checked_usize(ctx.get(1)?, "bit_width")?;
        let index = IdMapIndex::new(dim, bit_width)
            .map_err(|error| function_error(format!("cannot create TurboVec index: {error}")))?;
        Ok(BuildState {
            index,
            dim,
            bit_width,
        })
    }

    fn step(&self, ctx: &mut FunctionContext<'_>, state: &mut BuildState) -> Result<()> {
        let dim = checked_usize(ctx.get(0)?, "dimensions")?;
        let bit_width = checked_usize(ctx.get(1)?, "bit_width")?;
        if dim != state.dim || bit_width != state.bit_width {
            return Err(function_error(
                "dimensions and bit_width must be constant within turbovec_build()",
            ));
        }

        let id = checked_id(ctx.get(2)?)?;
        let vector = parse_vector(ctx.get_raw(3))?;
        if vector.len() != state.dim {
            return Err(function_error(format!(
                "vector has {} dimensions; index requires {}",
                vector.len(),
                state.dim
            )));
        }
        state
            .index
            .add_with_ids(&vector, &[id])
            .map_err(|error| function_error(format!("cannot add vector: {error}")))
    }

    fn finalize(
        &self,
        _ctx: &mut FunctionContext<'_>,
        state: Option<BuildState>,
    ) -> Result<Vec<u8>> {
        state
            .map(|state| state.index.to_bytes())
            .ok_or_else(|| function_error("turbovec_build() needs at least one input row"))
    }
}

#[repr(C)]
struct KnnTable {
    base: ffi::sqlite3_vtab,
}

unsafe impl<'vtab> VTab<'vtab> for KnnTable {
    type Aux = ();
    type Cursor = KnnCursor<'vtab>;

    fn connect(
        db: &mut VTabConnection,
        _aux: Option<&Self::Aux>,
        _module_name: &[u8],
        _database_name: &[u8],
        _table_name: &[u8],
        _args: &[&[u8]],
    ) -> Result<(Cow<'static, CStr>, Self)> {
        // Deserializing caller-controlled BLOBs can consume meaningful CPU and
        // memory. Keep this out of schema objects, triggers, and views.
        db.config(VTabConfig::DirectOnly)?;
        Ok((
            Cow::Borrowed(c"CREATE TABLE x(id INTEGER, score REAL, index_blob BLOB HIDDEN, query BLOB HIDDEN, k INTEGER HIDDEN)"),
            Self {
                base: ffi::sqlite3_vtab::default(),
            },
        ))
    }

    fn best_index(&self, info: &mut IndexInfo) -> Result<bool> {
        let mut hidden_constraints = [None; 3];
        for (constraint_index, constraint) in info.constraints().enumerate() {
            if !constraint.is_usable()
                || constraint.operator() != IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_EQ
            {
                continue;
            }
            let slot = match constraint.column() {
                COL_INDEX => Some(0),
                COL_QUERY => Some(1),
                COL_K => Some(2),
                _ => None,
            };
            if let Some(slot) = slot {
                hidden_constraints[slot] = Some(constraint_index);
            }
        }

        if hidden_constraints.iter().any(Option::is_none) {
            return Ok(false);
        }

        for (argument, constraint_index) in hidden_constraints.into_iter().enumerate() {
            let mut usage = info.constraint_usage(constraint_index.expect("checked above"));
            usage.set_argv_index((argument + 1) as c_int);
            usage.set_omit(true);
        }

        let consumes_score_order = info.num_of_order_by() == 1
            && info
                .order_bys()
                .next()
                .is_some_and(|order| order.column() == COL_SCORE && order.is_order_by_desc());
        info.set_order_by_consumed(consumes_score_order);
        info.set_idx_num(PLAN_KNN);
        info.set_estimated_cost(10.0);
        info.set_estimated_rows(10);
        Ok(true)
    }

    fn open(&'vtab mut self) -> Result<Self::Cursor> {
        Ok(KnnCursor::default())
    }
}

#[derive(Default)]
#[repr(C)]
struct KnnCursor<'vtab> {
    base: ffi::sqlite3_vtab_cursor,
    rows: Vec<(u64, f32)>,
    position: usize,
    phantom: PhantomData<&'vtab KnnTable>,
}

unsafe impl VTabCursor for KnnCursor<'_> {
    fn filter(&mut self, idx_num: c_int, _idx_str: Option<&str>, args: &Filters<'_>) -> Result<()> {
        self.rows.clear();
        self.position = 0;
        if idx_num != PLAN_KNN || args.len() != 3 {
            return Err(module_error(
                "use turbovec_knn(index_blob, query, k) with all three arguments",
            ));
        }

        let payload: Vec<u8> = args.get(0)?;
        let query = parse_vector(args.iter().nth(1).expect("three arguments"))?;
        let k_raw: i64 = args.get(2)?;
        let k = usize::try_from(k_raw)
            .map_err(|_| module_error("k must be a non-negative SQLite INTEGER"))?;

        let index = load_index(&payload).map_err(|error| module_error(error.to_string()))?;
        validate_vector_dim(&index, &query).map_err(|error| module_error(error.to_string()))?;
        let results = index
            .try_search(&query, k)
            .map_err(|error| module_error(format!("TurboVec search failed: {error}")))?;
        self.rows = results.ids.into_iter().zip(results.scores).collect();
        Ok(())
    }

    fn next(&mut self) -> Result<()> {
        self.position += 1;
        Ok(())
    }

    fn eof(&self) -> bool {
        self.position >= self.rows.len()
    }

    fn column(&self, ctx: &mut Context, column: c_int) -> Result<()> {
        let Some((id, score)) = self.rows.get(self.position).copied() else {
            return Err(module_error("cursor is not positioned on a result row"));
        };
        match column {
            COL_ID => ctx.set_result(
                &i64::try_from(id)
                    .map_err(|_| module_error("TurboVec id is too large for a SQLite INTEGER"))?,
            ),
            COL_SCORE => ctx.set_result(&f64::from(score)),
            COL_INDEX | COL_QUERY | COL_K => ctx.set_result(&Null),
            _ => Err(module_error(format!("unknown result column {column}"))),
        }
    }

    fn rowid(&self) -> Result<i64> {
        i64::try_from(self.position + 1)
            .map_err(|_| module_error("result position does not fit in a SQLite rowid"))
    }
}

fn extension_init(db: Connection) -> Result<bool> {
    let deterministic = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    let pure = deterministic | FunctionFlags::SQLITE_INNOCUOUS;
    let resource_heavy = deterministic | FunctionFlags::SQLITE_DIRECTONLY;

    db.create_scalar_function(c"turbovec_version", 0, pure, version)?;
    db.create_scalar_function(c"turbovec_f32", 1, pure, f32_blob)?;
    db.create_scalar_function(c"turbovec_new", 2, resource_heavy, new_index)?;
    db.create_scalar_function(c"turbovec_add", 3, resource_heavy, add_vector)?;
    db.create_scalar_function(c"turbovec_remove", 2, resource_heavy, remove_vector)?;
    db.create_scalar_function(c"turbovec_len", 1, resource_heavy, index_len)?;
    db.create_scalar_function(c"turbovec_dimensions", 1, resource_heavy, index_dim)?;
    db.create_scalar_function(c"turbovec_bit_width", 1, resource_heavy, index_bit_width)?;
    db.create_aggregate_function(c"turbovec_build", 4, resource_heavy, BuildAggregate)?;

    const MODULE: Module<KnnTable> = Module::eponymous_only_module();
    db.create_module(MODULE_NAME, &MODULE, None)?;
    chunked::register(&db)?;
    Ok(false)
}

/// SQLite's conventional loadable-extension entry point.
///
/// # Safety
///
/// SQLite calls this with a valid connection, error pointer, and API table.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_extension_init(
    db: *mut ffi::sqlite3,
    error_message: *mut *mut c_char,
    api: *mut ffi::sqlite3_api_routines,
) -> c_int {
    unsafe {
        chunked::initialize_api(api);
        Connection::extension_init2(db, error_message, api, extension_init)
    }
}

/// Named entry point, useful for static linking and explicit loaders.
///
/// # Safety
///
/// Same contract as [`sqlite3_extension_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_turbovec_init(
    db: *mut ffi::sqlite3,
    error_message: *mut *mut c_char,
    api: *mut ffi::sqlite3_api_routines,
) -> c_int {
    unsafe { sqlite3_extension_init(db, error_message, api) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_and_little_endian_blob_vectors() {
        let expected = vec![1.25, -2.5, 0.0];
        assert_eq!(
            parse_vector(ValueRef::Text(br#"[1.25, -2.5, 0.0]"#)).unwrap(),
            expected
        );

        let blob = vector_blob(&expected);
        assert_eq!(parse_vector(ValueRef::Blob(&blob)).unwrap(), expected);
    }

    #[test]
    fn rejects_invalid_vector_encodings_and_coordinates() {
        assert!(parse_vector(ValueRef::Blob(&[0, 1, 2])).is_err());
        assert!(parse_vector(ValueRef::Integer(1)).is_err());

        let non_finite = vector_blob(&[f32::INFINITY]);
        assert!(parse_vector(ValueRef::Blob(&non_finite)).is_err());

        let oversized = vector_blob(&[1.0e16]);
        assert!(parse_vector(ValueRef::Blob(&oversized)).is_err());
    }

    #[test]
    fn rejects_negative_sizes_and_ids() {
        assert!(checked_usize(-1, "dimensions").is_err());
        assert!(checked_id(-1).is_err());
        assert_eq!(checked_usize(8, "dimensions").unwrap(), 8);
        assert_eq!(checked_id(42).unwrap(), 42);
    }
}
