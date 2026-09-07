//! Database plugin: a SQLite file as a subgraph.
//!
//! `db.database` is a container ([`zeughaus_graph`]-style, see
//! `NodeDefinition::container`) whose `path` setting names the file. Everything
//! inside it works on that file: a `db.table` declares a schema and creates it,
//! `db.insert` writes a row when its trigger fires, `db.query` and `db.sql`
//! read.
//!
//! Two decisions worth stating. The path reaches the nodes as a hidden
//! parameter (`db_path`) that the editor derives from the enclosing
//! `db.database` -- a node inside a database must not have to repeat which
//! database it is in. And `where`/`order`/`sql` are raw SQL fragments by
//! design: this is a workbench, the user writes SQL, and pretending otherwise
//! would mean reinventing a query language badly.
//!
//! Native only: it opens files and links SQLite, so it is registered in the
//! runner unconditionally and in the editor outside wasm.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use rusqlite::Connection;
use zeughaus_core::*;

mod nodes;

pub use nodes::{DatabaseNode, InsertNode, QueryNode, SqlNode, TableNode};

pub struct DbPlugin;

impl DomainPlugin for DbPlugin {
    fn name(&self) -> &str {
        "db"
    }

    fn node_catalog(&self) -> Vec<NodeDefinition> {
        vec![
            catalog_entry("db.database", "Database", "Database", &DatabaseNode::new()).container(),
            catalog_entry("db.table", "Table", "Database", &TableNode::new()),
            catalog_entry("db.insert", "Insert", "Database", &InsertNode::new()),
            catalog_entry("db.query", "Query", "Database", &QueryNode::new()),
            catalog_entry("db.sql", "SQL", "Database", &SqlNode::new()),
        ]
    }

    fn create_node(&self, type_id: &str) -> Option<Box<dyn ExecutableNode>> {
        match type_id {
            "db.database" => Some(Box::new(DatabaseNode::new())),
            "db.table" => Some(Box::new(TableNode::new())),
            "db.insert" => Some(Box::new(InsertNode::new())),
            "db.query" => Some(Box::new(QueryNode::new())),
            "db.sql" => Some(Box::new(SqlNode::new())),
            _ => None,
        }
    }
}

/// The hidden parameter every `db.*` node takes: which database file it works
/// on. Derived by the editor from the enclosing `db.database`, so it is not in
/// any node's `settings()`.
pub const DB_PATH: &str = "db_path";

/// The type a table handle travels as. Opaque: a table is a name and a schema
/// in one file, not something to take apart on a wire.
pub fn table_ty() -> Ty {
    Ty::opaque("db.table")
}

/// A column's declared type, as the user writes it in a `name:type` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColTy {
    Int,
    Float,
    Str,
    Bool,
}

impl ColTy {
    /// Parses `int|float|str|bool`, case-insensitively. `None` for anything
    /// else, which is how an unfinished line is ignored rather than guessed.
    pub fn parse(text: &str) -> Option<ColTy> {
        match text.trim().to_ascii_lowercase().as_str() {
            "int" => Some(ColTy::Int),
            "float" => Some(ColTy::Float),
            "str" | "string" | "text" => Some(ColTy::Str),
            "bool" => Some(ColTy::Bool),
            _ => None,
        }
    }

    /// The SQLite affinity this column is declared with.
    pub fn affinity(self) -> &'static str {
        match self {
            ColTy::Int | ColTy::Bool => "INTEGER",
            ColTy::Float => "REAL",
            ColTy::Str => "TEXT",
        }
    }

    /// The graph type a value of this column travels as.
    pub fn ty(self) -> Ty {
        match self {
            ColTy::Int => Ty::Int,
            ColTy::Float => Ty::Float,
            ColTy::Str => Ty::Str,
            ColTy::Bool => Ty::Bool,
        }
    }
}

/// Parses a column list: one `name:type` per line.
///
/// Unparsable and blank lines are skipped rather than refused, because this
/// text is edited a character at a time inside a node and a half-typed line
/// must not delete the pins of the finished ones.
pub fn parse_columns(text: &str) -> Vec<(String, ColTy)> {
    text.lines()
        .filter_map(|line| {
            let (name, ty) = line.split_once(':')?;
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            Some((name.to_string(), ColTy::parse(ty)?))
        })
        .collect()
}

/// A table as it travels between nodes: which table, and what is in it.
///
/// The schema travels with the name because the reader needs both: an insert
/// binds one parameter per column, and the editor derives its pins from the
/// same list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub name: String,
    pub columns: Vec<(String, ColTy)>,
}

impl Typed for TableRef {
    fn ty() -> Ty {
        table_ty()
    }

    /// The table's name: an opaque value that shows nothing would leave a pin
    /// reading `...` where the one useful word fits.
    fn repr(&self) -> Repr<'_> {
        Repr::Str(&self.name)
    }
}

/// One open connection per database file, shared by every node that names it.
///
/// A connection per node would mean a dozen writers on one file and a lock
/// error the moment two of them run in the same pass. SQLite serializes writes
/// itself; sharing one connection is what lets it.
static POOL: LazyLock<Mutex<HashMap<String, Arc<Mutex<Connection>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Opens `path`, or hands back the connection already open on it.
///
/// `foreign_keys` because a table node declares references and they are worth
/// nothing unenforced; WAL because a reader must not block the pass that is
/// writing.
pub fn open(path: &str) -> Result<Arc<Mutex<Connection>>> {
    if path.trim().is_empty() {
        return Err(ZeughausError::ExecutionFailed(
            "not inside a database: place this node in a db.database subgraph".to_string(),
        ));
    }
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(open) = pool.get(path) {
        return Ok(Arc::clone(open));
    }
    let conn = Connection::open(path).map_err(|e| failed(format!("cannot open {path}: {e}")))?;
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;")
        .map_err(|e| failed(format!("cannot configure {path}: {e}")))?;
    let shared = Arc::new(Mutex::new(conn));
    pool.insert(path.to_string(), Arc::clone(&shared));
    Ok(shared)
}

/// A node error. Every failure a database node reports is one of these: the
/// user sees the message on the node and the pass keeps going.
pub fn failed(message: impl Into<String>) -> ZeughausError {
    ZeughausError::ExecutionFailed(message.into())
}

/// A graph value as a bound SQL parameter.
///
/// Anything that is not a scalar becomes `NULL` rather than an error: a column
/// wired to a frame is a mistake worth seeing in the row, not a pass that stops.
pub fn to_sql(value: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as Sql;
    match value.repr() {
        Repr::Bool(v) => Sql::Integer(i64::from(v)),
        Repr::Int(v) => Sql::Integer(v),
        Repr::Float(v) => Sql::Real(v),
        Repr::Str(v) => Sql::Text(v.to_string()),
        _ => Sql::Null,
    }
}

/// A SQL value as JSON, for the text a query emits.
pub fn from_sql(value: rusqlite::types::ValueRef<'_>) -> serde_json::Value {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(v) => serde_json::Value::from(v),
        ValueRef::Real(v) => serde_json::Number::from_f64(v)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        ValueRef::Text(v) => serde_json::Value::from(String::from_utf8_lossy(v).into_owned()),
        // Hex rather than base64: a blob in a workbench is something to
        // recognize, and hex is what every other tool prints.
        ValueRef::Blob(v) => serde_json::Value::from(
            v.iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
        ),
    }
}

/// Runs a query and returns its rows as a JSON array of objects.
///
/// Text, because the graph carries scalars: a row set is not one, and a
/// nominal row type would have to be built before anything could read it.
pub fn rows_to_json(
    conn: &Connection,
    sql: &str,
    params: &[rusqlite::types::Value],
) -> Result<(String, i64)> {
    let mut statement = conn
        .prepare(sql)
        .map_err(|e| failed(format!("{sql}: {e}")))?;
    let names: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    let mut rows = statement
        .query(rusqlite::params_from_iter(params.iter()))
        .map_err(|e| failed(format!("{sql}: {e}")))?;
    let mut out: Vec<serde_json::Value> = Vec::new();
    while let Some(row) = rows.next().map_err(|e| failed(format!("{sql}: {e}")))? {
        let mut object = serde_json::Map::with_capacity(names.len());
        for (index, name) in names.iter().enumerate() {
            let value = row
                .get_ref(index)
                .map_err(|e| failed(format!("{sql}: {e}")))?;
            object.insert(name.clone(), from_sql(value));
        }
        out.push(serde_json::Value::Object(object));
    }
    let count = out.len() as i64;
    Ok((serde_json::Value::Array(out).to_string(), count))
}

/// Whether a statement returns rows. Text-based on purpose: SQLite decides the
/// same way, and a workbench has to accept whatever the user typed.
pub fn is_query(sql: &str) -> bool {
    let head = sql.trim_start().to_ascii_uppercase();
    ["SELECT", "WITH", "PRAGMA", "EXPLAIN"]
        .iter()
        .any(|kind| head.starts_with(kind))
}

/// Quotes an identifier for SQL. Doubling `"` is the whole rule; the name comes
/// from a text field and must not be able to end the quoting.
pub fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_all_catalog_nodes() {
        let plugin = DbPlugin;
        for def in plugin.node_catalog() {
            assert!(
                plugin.create_node(&def.type_id).is_some(),
                "failed: {}",
                def.type_id
            );
        }
    }

    /// The column text is typed a character at a time, so an unfinished line
    /// must be skipped rather than dropping the finished ones with it.
    #[test]
    fn columns_parse_line_by_line_and_skip_what_is_not_one() {
        let parsed = parse_columns("id:int\n ts : Float \nbroken\n\nname:str\nx:nope\n");
        assert_eq!(
            parsed,
            vec![
                ("id".to_string(), ColTy::Int),
                ("ts".to_string(), ColTy::Float),
                ("name".to_string(), ColTy::Str),
            ]
        );
    }

    #[test]
    fn a_column_type_maps_to_an_affinity_and_a_graph_type() {
        assert_eq!(ColTy::Int.affinity(), "INTEGER");
        assert_eq!(ColTy::Bool.affinity(), "INTEGER");
        assert_eq!(ColTy::Float.affinity(), "REAL");
        assert_eq!(ColTy::Str.affinity(), "TEXT");
        assert_eq!(ColTy::Float.ty(), Ty::Float);
        assert_eq!(ColTy::Bool.ty(), Ty::Bool);
    }

    /// A node outside a database has no file to work on, and saying so beats
    /// creating one wherever the process happens to be running.
    #[test]
    fn an_empty_path_is_an_error_not_a_new_file() {
        assert!(open("   ").is_err());
    }

    /// A name from a text field must not be able to end its own quoting.
    #[test]
    fn identifiers_are_quoted() {
        assert_eq!(quote("samples"), "\"samples\"");
        assert_eq!(quote("we\"ird"), "\"we\"\"ird\"");
    }

    #[test]
    fn statement_kind_follows_the_leading_keyword() {
        assert!(is_query("  select * from t"));
        assert!(is_query("WITH x AS (SELECT 1) SELECT * FROM x"));
        assert!(is_query("pragma table_info('t')"));
        assert!(!is_query("INSERT INTO t VALUES (1)"));
        assert!(!is_query("create table t (a int)"));
    }
}
