//! The five database nodes.
//!
//! All of them take the hidden [`crate::DB_PATH`] parameter and open the file
//! through [`crate::open`], so which database a node works on is decided by
//! where it sits, not by what it repeats.

use zeughaus_core::*;

use crate::{
    ColTy, DB_PATH, RELATIONS, TableRef, failed, is_query, open, parse_columns,
    parse_columns_checked, parse_relations, quote, rejected, rows_to_json, table_ty, to_sql,
};

/// Reads a parameter's text, whatever scalar form it arrives in.
///
/// Settings travel as strings, but a value wired from elsewhere may be a
/// number, and refusing it would make the same setting behave differently
/// depending on where it came from.
fn text_of(value: &Value) -> Option<String> {
    match value.repr() {
        Repr::Str(s) => Some(s.to_string()),
        Repr::Int(v) => Some(v.to_string()),
        Repr::Float(v) => Some(v.to_string()),
        Repr::Bool(v) => Some(v.to_string()),
        _ => None,
    }
}

/// The columns a table has in the file, as `(name, declared type)` in file
/// order.
///
/// The file is the authority on a schema, not the node: what the node declares
/// is what the user wants, and the difference between the two is the migration.
fn table_columns(conn: &rusqlite::Connection, table: &str) -> Result<Vec<(String, String)>> {
    // The statement is generated and always the same shape, so what a failure
    // has to say is which table it was reading and what SQLite said. A node
    // error is drawn in the node body, where a pasted statement pushes the
    // message that explains it out of sight.
    let failure = |e: rusqlite::Error| failed(format!("cannot read the schema of {table}: {e}"));
    let sql = format!("PRAGMA table_info({})", quote(table));
    let mut statement = conn.prepare(&sql).map_err(failure)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })
        .map_err(failure)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(failure)
}

/// The one column a rename turned into another, read off the difference
/// between the file and what the node declares, as `(old, new)`.
///
/// Recognised as narrowly as the editor's own detection: exactly one declared
/// field missing from the file, exactly one file column no longer declared,
/// and the same SQLite affinity. Anything else -- a field added, a field
/// removed, two of each -- is not a rename, and `ALTER TABLE RENAME COLUMN`
/// on a guess moves someone's data under a name they did not choose.
///
/// Stateless on purpose. Deriving it here means a standby runner that never
/// saw the edits, a pass that failed before this point, and a chain of renames
/// typed in one go all reach the same answer: whatever the file and the node
/// currently disagree about.
fn renamed_column(
    existing: &[(String, String)],
    declared: &[(String, ColTy)],
) -> Option<(String, String)> {
    let mut added = declared
        .iter()
        .filter(|(name, _)| !existing.iter().any(|(have, _)| have == name));
    let mut dropped = existing
        .iter()
        .filter(|(name, _)| !declared.iter().any(|(want, _)| want == name));
    let (new, ty) = added.next()?;
    let (old, have) = dropped.next()?;
    if added.next().is_some() || dropped.next().is_some() {
        return None;
    }
    if !have.eq_ignore_ascii_case(ty.affinity()) {
        return None;
    }
    Some((old.clone(), new.clone()))
}

/// The database itself: a container whose `path` names the file.
///
/// It executes nothing. What it contributes is the path its children inherit
/// and the boundary the editor draws around them.
pub struct DatabaseNode {
    path: String,
}

impl Default for DatabaseNode {
    fn default() -> Self {
        Self::new()
    }
}

impl DatabaseNode {
    pub const DEFAULT_PATH: &'static str = "zeughaus.sqlite";

    pub fn new() -> Self {
        Self {
            path: Self::DEFAULT_PATH.to_string(),
        }
    }

    /// The file every node inside this database works on.
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl ExecutableNode for DatabaseNode {
    fn execute(&mut self, _inputs: &InputSet, _ctx: &mut NodeContext) -> Result<()> {
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &[]
    }

    fn settings(&self) -> Vec<SettingDef> {
        vec![SettingDef::new("path", self.path.clone())]
    }

    fn set_parameter(&mut self, name: &str, value: Value) -> Result<()> {
        if name == "path"
            && let Some(text) = text_of(&value)
        {
            self.path = text.trim().to_string();
        }
        Ok(())
    }
}

/// A table: its schema, and the `CREATE TABLE` that follows from it.
///
/// The schema is designed in the node. The name is an editable title, the
/// fields are a row editor, and every field is a bidirectional pin spanning
/// the node -- so a relation to another table is a wire between two fields
/// rather than a value on an input. See the crate docs for which end of such
/// a wire is the referenced one.
///
/// Creating is the execution, and so is following the schema afterwards: a
/// field the file does not have yet is added, a field renamed in the graph is
/// renamed in the file, and a field removed from the node is dropped from the
/// file. What stays refused is a retype -- the one edit SQLite cannot do in
/// place -- and whatever SQLite itself refuses to drop.
pub struct TableNode {
    db_path: String,
    name: String,
    columns: String,
    /// The relations this table's fields declare, as the editor derived them
    /// from the wires: one `field -> table.field` per line.
    relations: String,
    pins: Vec<PinDefinition>,
}

impl Default for TableNode {
    fn default() -> Self {
        Self::new()
    }
}

impl TableNode {
    pub const DEFAULT_NAME: &'static str = "table";
    pub const DEFAULT_COLUMNS: &'static str = "id:int\nvalue:float";

    pub fn new() -> Self {
        let mut node = Self {
            db_path: String::new(),
            name: Self::DEFAULT_NAME.to_string(),
            columns: Self::DEFAULT_COLUMNS.to_string(),
            relations: String::new(),
            pins: Vec::new(),
        };
        node.rebuild_pins();
        node
    }

    /// One bidirectional field pin per field, plus the handle and the DDL text.
    ///
    /// A field pin declares the column's own type rather than one nominal
    /// field type, which is what makes a relation between an `int` and a `str`
    /// field refuse to connect at all: the editor accepts a wire between two
    /// field pins only when their types are equal. A foreign key onto a column
    /// of another type is a constraint SQLite will keep failing, and refusing
    /// the wire says so while it is being drawn.
    fn rebuild_pins(&mut self) {
        let columns = parse_columns(&self.columns);
        let mut pins = Vec::with_capacity(columns.len() + 2);
        for (name, ty) in &columns {
            pins.push(PinDefinition::field(name.clone(), ty.ty()));
        }
        pins.push(PinDefinition::output("table", table_ty()));
        pins.push(PinDefinition::output("ddl", Ty::Str));
        self.pins = pins;
    }

    /// The `CREATE TABLE` this schema means, including the foreign keys its
    /// relations declare.
    ///
    /// An `id:int` field becomes `INTEGER PRIMARY KEY`, which is SQLite's
    /// rowid alias: a table a workbench inserts into wants one, and naming it
    /// `id` is the convention the rest of these nodes read.
    ///
    /// A relation naming a field this table does not have is dropped rather
    /// than emitted: the wire may still be there while the field it started on
    /// is being renamed, and a `FOREIGN KEY` on a column that is not in the
    /// statement is a syntax error, not a warning.
    ///
    /// This is the schema as designed, which is not always the schema in the
    /// file. `CREATE TABLE IF NOT EXISTS` does nothing to a table that is
    /// already there, and SQLite has no `ADD CONSTRAINT`: a foreign key can
    /// only be declared when the table is created, or on a column that
    /// `ALTER TABLE ... ADD COLUMN` adds. So a relation drawn onto a field
    /// the file already has shows up in this text and never in the file --
    /// dropping the table (or renaming this one) is the only way to apply it.
    pub fn ddl(&self) -> String {
        let columns = parse_columns(&self.columns);
        let relations = parse_relations(&self.relations);
        let mut parts: Vec<String> = Vec::with_capacity(columns.len() + relations.len());
        for (name, ty) in &columns {
            if name == "id" && *ty == ColTy::Int {
                parts.push(format!("{} INTEGER PRIMARY KEY", quote(name)));
            } else {
                parts.push(format!("{} {}", quote(name), ty.affinity()));
            }
        }
        for relation in &relations {
            if !columns.iter().any(|(name, _)| *name == relation.field) {
                continue;
            }
            parts.push(format!(
                "FOREIGN KEY({}) REFERENCES {}({})",
                quote(&relation.field),
                quote(&relation.table),
                quote(&relation.target)
            ));
        }
        format!(
            "CREATE TABLE IF NOT EXISTS {} ({})",
            quote(&self.name),
            parts.join(", ")
        )
    }

    pub fn table_ref(&self) -> TableRef {
        TableRef {
            name: self.name.clone(),
            columns: parse_columns(&self.columns),
        }
    }
}

impl ExecutableNode for TableNode {
    fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let columns = parse_columns(&self.columns);
        if columns.is_empty() {
            return Err(failed("no fields: give the table at least one"));
        }
        let ddl = self.ddl();

        let conn = open(&self.db_path)?;
        let conn = conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute_batch(&ddl)
            .map_err(|e| failed(format!("cannot create table {}: {e}", self.name)))?;

        // A field renamed in the graph is renamed in the file, before the
        // schemas are compared -- otherwise the same edit reads as one column
        // dropped and another added, and is refused.
        //
        // Derived from the file, not remembered. A remembered chain (`x->y`
        // then `y->z` collapsing to `x->z`) is per-process state, and there
        // are two processes: a standby accumulates the same chain without
        // executing it, so killing the owner mid-chain left the file at `y`
        // and the new owner looking for `x`, which wedged the table forever.
        // Reading the difference off the file is stateless, idempotent, and
        // survives a pass that failed before it got here.
        let existing = if let Some((old, new)) =
            renamed_column(&table_columns(&conn, &self.name)?, &columns)
        {
            let sql = format!(
                "ALTER TABLE {} RENAME COLUMN {} TO {}",
                quote(&self.name),
                quote(&old),
                quote(&new)
            );
            conn.execute_batch(&sql).map_err(|e| {
                failed(format!(
                    "cannot rename column {old} to {new} in table {}: {e}",
                    self.name
                ))
            })?;
            table_columns(&conn, &self.name)?
        } else {
            table_columns(&conn, &self.name)?
        };

        // What is in the file decides, not what this node declared. A field
        // the file does not have yet is added, a column the node no longer
        // declares is dropped, and only a column whose type changed is
        // refused -- that is the one edit SQLite cannot do in place, and
        // guessing between a cast and a rename is how data gets lost.
        let mut refused: Vec<String> = Vec::new();
        for (name, ty) in &columns {
            let Some((_, have)) = existing.iter().find(|(e, _)| e == name) else {
                continue;
            };
            if !have.eq_ignore_ascii_case(ty.affinity()) {
                refused.push(format!(
                    "{name} is {have} in the file, not {}",
                    ty.affinity()
                ));
            }
        }
        if !refused.is_empty() {
            return Err(failed(format!(
                "table {} cannot be migrated: {}; drop the table or rename this one",
                self.name,
                refused.join(", ")
            )));
        }

        // A field removed in the graph is dropped from the file. `ALTER TABLE
        // ... DROP COLUMN` has existed since SQLite 3.35 and the bundled
        // library is newer, so removing a field is an edit like any other
        // rather than a dead end.
        //
        // SQLite refuses a column that carries the primary key, that an index
        // or a foreign key names, or that a view or generated column reads.
        // Its own message is the one that reaches the user, because it is the
        // one that says which of those it is; nothing here can improve on it.
        //
        // A rename is settled above, so a field renamed in the graph never
        // reaches this loop as one column dropped and another added.
        for (name, _) in &existing {
            if columns.iter().any(|(declared, _)| declared == name) {
                continue;
            }
            let sql = format!(
                "ALTER TABLE {} DROP COLUMN {}",
                quote(&self.name),
                quote(name)
            );
            conn.execute_batch(&sql).map_err(|e| {
                failed(format!(
                    "cannot drop column {name} from table {}: {e}",
                    self.name
                ))
            })?;
        }

        // A new column may carry its foreign key, but only because SQLite
        // allows a `REFERENCES` clause on `ADD COLUMN` when the default is
        // NULL. A relation drawn onto a column that is already in the file
        // cannot be added at all -- see [`Self::ddl`].
        let relations = parse_relations(&self.relations);
        for (name, ty) in &columns {
            if existing.iter().any(|(e, _)| e == name) {
                continue;
            }
            let mut sql = format!(
                "ALTER TABLE {} ADD COLUMN {} {}",
                quote(&self.name),
                quote(name),
                ty.affinity()
            );
            if let Some(relation) = relations.iter().find(|r| &r.field == name) {
                sql.push_str(&format!(
                    " REFERENCES {}({})",
                    quote(&relation.table),
                    quote(&relation.target)
                ));
            }
            conn.execute_batch(&sql).map_err(|e| {
                failed(format!(
                    "cannot add column {name} to table {}: {e}",
                    self.name
                ))
            })?;
        }

        ctx.emit("table", Value::new(self.table_ref()));
        ctx.emit_typed("ddl", ddl);
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    fn settings(&self) -> Vec<SettingDef> {
        vec![
            SettingDef::new("name", self.name.clone()).title(),
            // The type vocabulary travels with the setting, so the editor
            // renders the row editor without linking this plugin -- which it
            // cannot do in the browser at all.
            SettingDef::new("columns", self.columns.clone()).fields(ColTy::NAMES),
        ]
    }

    fn set_parameter(&mut self, name: &str, value: Value) -> Result<()> {
        let Some(text) = text_of(&value) else {
            return Ok(());
        };
        match name {
            DB_PATH => self.db_path = text.trim().to_string(),
            "name" => {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    self.name = trimmed.to_string();
                }
            }
            // The pins follow the columns, which is why this node has to be
            // asked for them again after a setting changed
            // (`GraphExecutor::refresh_pins`).
            //
            // A rename is not remembered here: `execute` reads it off the
            // difference between the file and this list, which is what makes
            // it survive a standby taking over mid-chain.
            "columns" => {
                // Validated before anything is replaced: an unparsable line
                // used to make every field pin (and every wire on it) vanish,
                // with the only report a later generic "no fields".
                parse_columns_checked(&text)?;
                self.columns = text;
                self.rebuild_pins();
            }
            // Derived by the editor from the wires between field pins, the
            // same way `db_path` is derived from the enclosing database: a
            // relation is a fact about the graph, and the runner reads facts.
            RELATIONS => self.relations = text,
            _ => {}
        }
        Ok(())
    }
}

/// Writes one row per trigger.
///
/// `changed("insert")` rather than a value comparison: two identical readings
/// are two rows, and a reading that did not change must not write a second one.
pub struct InsertNode {
    db_path: String,
    columns: String,
    pins: Vec<PinDefinition>,
    last_id: i64,
    count: i64,
}

impl Default for InsertNode {
    fn default() -> Self {
        Self::new()
    }
}

impl InsertNode {
    pub fn new() -> Self {
        let mut node = Self {
            db_path: String::new(),
            columns: String::new(),
            pins: Vec::new(),
            last_id: 0,
            count: 0,
        };
        node.rebuild_pins();
        node
    }

    /// The table handle, the trigger, and one input per column of the table
    /// that is wired in (the editor derives the column list, see the crate
    /// header).
    fn rebuild_pins(&mut self) {
        let columns = parse_columns(&self.columns);
        let mut pins = Vec::with_capacity(columns.len() + 4);
        pins.push(PinDefinition::input("table", table_ty(), PinKind::Sample));
        pins.push(PinDefinition::input("insert", Ty::Any, PinKind::Trigger));
        for (name, ty) in &columns {
            // `id` is SQLite's rowid: it is assigned, not supplied.
            if name == "id" {
                continue;
            }
            pins.push(PinDefinition::input(name.clone(), ty.ty(), PinKind::Sample));
        }
        pins.push(PinDefinition::output("id", Ty::Int));
        pins.push(PinDefinition::output("count", Ty::Int));
        self.pins = pins;
    }

    fn emit_state(&self, ctx: &mut NodeContext) {
        ctx.emit_typed("id", self.last_id);
        ctx.emit_typed("count", self.count);
        ctx.flush();
    }
}

impl ExecutableNode for InsertNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        if !inputs.changed("insert") {
            // A pass this node runs in for someone else's reason is not a
            // press: re-emitting what it already wrote keeps its outputs
            // steady without writing a row.
            self.emit_state(ctx);
            return Ok(());
        }
        let table = inputs
            .get_value("table")
            .and_then(|v| v.downcast_ref::<TableRef>())
            .ok_or_else(|| failed("no table wired"))?
            .clone();

        let columns: Vec<String> = table
            .columns
            .iter()
            .map(|(name, _)| name.clone())
            .filter(|name| name != "id")
            .collect();
        if columns.is_empty() {
            return Err(failed(format!(
                "table {} has no columns to write",
                table.name
            )));
        }
        let values: Vec<rusqlite::types::Value> = columns
            .iter()
            .map(|name| {
                inputs
                    .get_value(name)
                    .map(to_sql)
                    .unwrap_or(rusqlite::types::Value::Null)
            })
            .collect();
        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            quote(&table.name),
            columns
                .iter()
                .map(|c| quote(c))
                .collect::<Vec<String>>()
                .join(", "),
            vec!["?"; columns.len()].join(", ")
        );

        let conn = open(&self.db_path)?;
        let conn = conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(&sql, rusqlite::params_from_iter(values.iter()))
            .map_err(|e| failed(format!("cannot insert into {}: {e}", table.name)))?;
        self.last_id = conn.last_insert_rowid();
        self.count += 1;
        drop(conn);

        self.emit_state(ctx);
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    fn set_parameter(&mut self, name: &str, value: Value) -> Result<()> {
        let Some(text) = text_of(&value) else {
            return Ok(());
        };
        match name {
            DB_PATH => self.db_path = text.trim().to_string(),
            "columns" => {
                self.columns = text;
                self.rebuild_pins();
            }
            _ => {}
        }
        Ok(())
    }
}

/// Reads a table when its trigger fires.
pub struct QueryNode {
    db_path: String,
    filter: String,
    order: String,
    limit: String,
    pins: Vec<PinDefinition>,
    rows: String,
    count: i64,
}

impl Default for QueryNode {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryNode {
    pub const DEFAULT_ORDER: &'static str = "id DESC";
    pub const DEFAULT_LIMIT: usize = 100;
    /// Upper bound on rows: the result is one JSON string in a node body, and
    /// a table with a million rows would be a million rows of text.
    pub const MAX_LIMIT: usize = 10_000;

    pub fn new() -> Self {
        Self {
            db_path: String::new(),
            filter: String::new(),
            order: Self::DEFAULT_ORDER.to_string(),
            limit: Self::DEFAULT_LIMIT.to_string(),
            pins: vec![
                PinDefinition::input("table", table_ty(), PinKind::Sample),
                PinDefinition::input("run", Ty::Any, PinKind::Trigger),
                PinDefinition::output("rows", Ty::Str),
                PinDefinition::output("count", Ty::Int),
            ],
            rows: "[]".to_string(),
            count: 0,
        }
    }

    /// The statement this node's settings mean. `where` and `order` are raw
    /// fragments by design (see the crate header).
    pub fn sql(&self, table: &str) -> String {
        let mut sql = format!("SELECT * FROM {}", quote(table));
        if !self.filter.trim().is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(self.filter.trim());
        }
        if !self.order.trim().is_empty() {
            sql.push_str(" ORDER BY ");
            sql.push_str(self.order.trim());
        }
        let limit = self
            .limit
            .trim()
            .parse::<usize>()
            .unwrap_or(Self::DEFAULT_LIMIT)
            .min(Self::MAX_LIMIT);
        sql.push_str(&format!(" LIMIT {limit}"));
        sql
    }
}

impl ExecutableNode for QueryNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        if inputs.changed("run") {
            let table = inputs
                .get_value("table")
                .and_then(|v| v.downcast_ref::<TableRef>())
                .ok_or_else(|| failed("no table wired"))?
                .name
                .clone();
            let sql = self.sql(&table);
            let conn = open(&self.db_path)?;
            let conn = conn.lock().unwrap_or_else(|e| e.into_inner());
            let (rows, count) = rows_to_json(&conn, &sql, &[])?;
            self.rows = rows;
            self.count = count;
        }
        // Emitted either way: the last result is this node's state, and a pin
        // that went empty between runs would dim the display that shows it.
        ctx.emit_typed("rows", self.rows.clone());
        ctx.emit_typed("count", self.count);
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    fn settings(&self) -> Vec<SettingDef> {
        vec![
            SettingDef::new("where", self.filter.clone()).placeholder("x > 0"),
            SettingDef::new("order", self.order.clone()),
            SettingDef::new("limit", self.limit.clone()),
        ]
    }

    fn set_parameter(&mut self, name: &str, value: Value) -> Result<()> {
        let Some(text) = text_of(&value) else {
            return Ok(());
        };
        match name {
            DB_PATH => self.db_path = text.trim().to_string(),
            "where" => self.filter = text,
            "order" => self.order = text,
            // Canonicalized here, where the user typed it. Storing any text
            // and silently falling back to the default at query time meant the
            // field showed one bound while the query used another.
            "limit" => {
                let trimmed = text.trim();
                let Ok(limit) = trimmed.parse::<usize>() else {
                    return Err(rejected(format!(
                        "limit '{trimmed}': expected a row count between 1 and {}",
                        Self::MAX_LIMIT
                    )));
                };
                if limit == 0 {
                    return Err(rejected("limit 0: a query that returns nothing is not one"));
                }
                self.limit = limit.min(Self::MAX_LIMIT).to_string();
            }
            _ => {}
        }
        Ok(())
    }
}

/// Runs whatever SQL the user wrote, when its trigger fires.
///
/// The escape hatch every database domain needs: schema changes, joins,
/// aggregates, `VACUUM`. Whether the text is a query is read off its first
/// keyword, the same way SQLite decides.
pub struct SqlNode {
    db_path: String,
    sql: String,
    pins: Vec<PinDefinition>,
    rows: String,
    count: i64,
}

impl Default for SqlNode {
    fn default() -> Self {
        Self::new()
    }
}

impl SqlNode {
    pub fn new() -> Self {
        Self {
            db_path: String::new(),
            sql: String::new(),
            pins: vec![
                PinDefinition::input("run", Ty::Any, PinKind::Trigger),
                PinDefinition::output("rows", Ty::Str),
                PinDefinition::output("count", Ty::Int),
            ],
            rows: "[]".to_string(),
            count: 0,
        }
    }
}

impl ExecutableNode for SqlNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        if inputs.changed("run") {
            let sql = self.sql.trim().to_string();
            if sql.is_empty() {
                return Err(failed("no statement: write the SQL to run"));
            }
            let conn = open(&self.db_path)?;
            let conn = conn.lock().unwrap_or_else(|e| e.into_inner());
            if is_query(&sql) {
                let (rows, count) = rows_to_json(&conn, &sql, &[])?;
                self.rows = rows;
                self.count = count;
            } else {
                let affected = conn
                    .execute(&sql, [])
                    .map_err(|e| failed(format!("{sql}: {e}")))?;
                self.rows = "[]".to_string();
                self.count = affected as i64;
            }
        }
        ctx.emit_typed("rows", self.rows.clone());
        ctx.emit_typed("count", self.count);
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    fn settings(&self) -> Vec<SettingDef> {
        vec![
            SettingDef::new("sql", self.sql.clone())
                .placeholder("SELECT count(*) FROM samples")
                .multiline(),
        ]
    }

    fn set_parameter(&mut self, name: &str, value: Value) -> Result<()> {
        let Some(text) = text_of(&value) else {
            return Ok(());
        };
        match name {
            DB_PATH => self.db_path = text.trim().to_string(),
            "sql" => self.sql = text,
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(name: &str) -> String {
        let path = std::env::temp_dir().join(format!("zeughaus-db-test-{name}.sqlite"));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
        path.to_string_lossy().into_owned()
    }

    fn set(node: &mut dyn ExecutableNode, name: &str, text: &str) {
        node.set_parameter(name, Value::new(text.to_string()))
            .expect("set");
    }

    fn ctx() -> NodeContext {
        NodeContext::new(NodeId(1), 0)
    }

    /// The DDL is what the user sees and what the file gets: an `id:int`
    /// becomes the rowid alias, and each relation becomes a foreign key
    /// naming the referenced field.
    #[test]
    fn the_ddl_names_a_primary_key_and_the_relations() {
        let mut node = TableNode::new();
        set(&mut node, "name", "orders");
        set(&mut node, "columns", "id:int\ncustomer_id:int\ntotal:float");
        set(&mut node, RELATIONS, "customer_id -> customers.id\n");
        assert_eq!(
            node.ddl(),
            "CREATE TABLE IF NOT EXISTS \"orders\" (\"id\" INTEGER PRIMARY KEY, \"customer_id\" INTEGER, \"total\" REAL, FOREIGN KEY(\"customer_id\") REFERENCES \"customers\"(\"id\"))"
        );
    }

    /// A relation on a field that is no longer there must not reach the
    /// statement: `FOREIGN KEY` on a column the statement does not declare is
    /// a syntax error, and the wire outlives the field by one edit.
    #[test]
    fn a_relation_on_a_removed_field_is_dropped_from_the_ddl() {
        let mut node = TableNode::new();
        set(&mut node, "name", "orders");
        set(&mut node, RELATIONS, "customer_id -> customers.id");
        set(&mut node, "columns", "id:int");
        assert_eq!(
            node.ddl(),
            "CREATE TABLE IF NOT EXISTS \"orders\" (\"id\" INTEGER PRIMARY KEY)"
        );
    }

    /// Every field is a pin, and a bidirectional one carrying the column's own
    /// type: that is what a relation attaches to on either border, and what
    /// keeps a wire between two fields of different types from landing.
    #[test]
    fn a_tables_pins_follow_its_fields() {
        let mut node = TableNode::new();
        set(&mut node, "columns", "id:int\nx:float\ntag:str\nok:bool");
        let pins = node.pin_definitions();
        let names: Vec<&str> = pins.iter().map(|p| &*p.name).collect();
        assert_eq!(names, vec!["id", "x", "tag", "ok", "table", "ddl"]);
        assert!(pins[..4].iter().all(|p| p.direction == PinDirection::Both));
        let types: Vec<&Ty> = pins[..4].iter().map(|p| &p.ty).collect();
        assert_eq!(types, vec![&Ty::Int, &Ty::Float, &Ty::Str, &Ty::Bool]);
    }

    /// An insert takes one input per column except the rowid, which SQLite
    /// assigns.
    #[test]
    fn an_inserts_pins_skip_the_id_column() {
        let mut node = InsertNode::new();
        set(&mut node, "columns", "id:int\nts:float\nx:float");
        let names: Vec<&str> = node.pin_definitions().iter().map(|p| &*p.name).collect();
        assert_eq!(names, vec!["table", "insert", "ts", "x", "id", "count"]);
    }

    /// The whole path, through the node API: create the table, insert on a
    /// trigger, read the row back.
    #[test]
    fn a_row_written_on_a_trigger_is_read_back_by_a_query() {
        let path = temp_db("roundtrip");

        let mut table = TableNode::new();
        set(&mut table, DB_PATH, &path);
        set(&mut table, "name", "samples");
        set(&mut table, "columns", "id:int\nx:float");
        let mut table_ctx = ctx();
        table
            .execute(&InputSet::new(), &mut table_ctx)
            .expect("create");
        let outputs = table_ctx.take_outputs();
        let handle = outputs.get("table").expect("table handle").clone();
        assert!(
            outputs
                .get("ddl")
                .and_then(Value::downcast_ref::<String>)
                .is_some_and(|ddl| ddl.contains("INTEGER PRIMARY KEY"))
        );

        let mut insert = InsertNode::new();
        set(&mut insert, DB_PATH, &path);
        set(&mut insert, "columns", "id:int\nx:float");

        // No trigger: nothing is written, and the outputs stay where they were.
        let mut inputs = InputSet::new();
        inputs.insert("table", handle.clone());
        inputs.insert("x", Value::new(1.5_f64));
        let mut quiet = ctx();
        insert.execute(&inputs, &mut quiet).expect("no-op");
        assert_eq!(
            quiet
                .take_outputs()
                .get("count")
                .and_then(Value::downcast_ref::<i64>),
            Some(&0)
        );

        // Trigger: one row.
        inputs.mark_changed("insert");
        let mut writing = ctx();
        insert.execute(&inputs, &mut writing).expect("insert");
        let written = writing.take_outputs();
        assert_eq!(
            written.get("count").and_then(Value::downcast_ref::<i64>),
            Some(&1)
        );
        assert_eq!(
            written.get("id").and_then(Value::downcast_ref::<i64>),
            Some(&1)
        );

        // The same reading again is another row: two deliveries are two rows.
        let mut again = ctx();
        insert.execute(&inputs, &mut again).expect("insert");
        assert_eq!(
            again
                .take_outputs()
                .get("count")
                .and_then(Value::downcast_ref::<i64>),
            Some(&2)
        );

        let mut query = QueryNode::new();
        set(&mut query, DB_PATH, &path);
        set(&mut query, "limit", "1");
        let mut run = InputSet::new();
        run.insert("table", handle);
        run.mark_changed("run");
        let mut query_ctx = ctx();
        query.execute(&run, &mut query_ctx).expect("query");
        let read = query_ctx.take_outputs();
        assert_eq!(
            read.get("count").and_then(Value::downcast_ref::<i64>),
            Some(&1)
        );
        let rows = read
            .get("rows")
            .and_then(Value::downcast_ref::<String>)
            .expect("rows");
        let parsed: serde_json::Value = serde_json::from_str(rows).expect("json");
        assert_eq!(parsed[0]["x"], serde_json::json!(1.5));
        assert_eq!(parsed[0]["id"], serde_json::json!(2));
    }

    /// A field added to a table whose file already exists reaches the file:
    /// designing a schema is iterative, and starting over for one more column
    /// is not a workbench.
    #[test]
    fn a_new_field_is_added_to_a_table_that_already_exists() {
        let path = temp_db("addcolumn");
        let mut node = TableNode::new();
        set(&mut node, DB_PATH, &path);
        set(&mut node, "name", "samples");
        set(&mut node, "columns", "id:int\nx:float");
        node.execute(&InputSet::new(), &mut ctx()).expect("create");

        set(&mut node, "columns", "id:int\nx:float\ntag:str");
        node.execute(&InputSet::new(), &mut ctx()).expect("migrate");
        // Running again with nothing new to do must stay a no-op rather than
        // trying to add the column twice.
        node.execute(&InputSet::new(), &mut ctx())
            .expect("idempotent");

        // The pooled connection is shared with the node, so the guard has to
        // be dropped before anything executes again.
        let columns: Vec<(String, String)> = {
            let conn = open(&path).expect("open");
            let conn = conn.lock().unwrap_or_else(|e| e.into_inner());
            let mut statement = conn
                .prepare("PRAGMA table_info(\"samples\")")
                .expect("pragma");
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
                })
                .expect("rows")
                .collect::<rusqlite::Result<Vec<_>>>()
                .expect("columns")
        };
        assert_eq!(
            columns,
            vec![
                ("id".to_string(), "INTEGER".to_string()),
                ("x".to_string(), "REAL".to_string()),
                ("tag".to_string(), "TEXT".to_string()),
            ]
        );
    }

    /// A field renamed in the graph is renamed in the file, with its data.
    /// Without this the same edit reads as one column dropped and another
    /// added, which is refused -- so fixing a typo would be a dead end.
    #[test]
    fn a_renamed_field_renames_the_column_and_keeps_its_data() {
        let path = temp_db("rename");
        let mut node = TableNode::new();
        set(&mut node, DB_PATH, &path);
        set(&mut node, "name", "samples");
        set(&mut node, "columns", "id:int\nx:float");
        node.execute(&InputSet::new(), &mut ctx()).expect("create");
        {
            let conn = open(&path).expect("open");
            let conn = conn.lock().unwrap_or_else(|e| e.into_inner());
            conn.execute_batch("INSERT INTO \"samples\" (\"x\") VALUES (1.5)")
                .expect("row");
        }

        // Two renames before the next pass, as typing produces them: they
        // collapse, because the file still holds the first name.
        set(&mut node, "columns", "id:int\ny:float");
        set(&mut node, "columns", "id:int\nspan:float");
        node.execute(&InputSet::new(), &mut ctx()).expect("rename");
        // A second pass has nothing left to rename and must not fail.
        node.execute(&InputSet::new(), &mut ctx())
            .expect("idempotent");

        let (columns, value) = {
            let conn = open(&path).expect("open");
            let conn = conn.lock().unwrap_or_else(|e| e.into_inner());
            let columns = table_columns(&conn, "samples").expect("columns");
            let value: f64 = conn
                .query_row("SELECT \"span\" FROM \"samples\"", [], |row| row.get(0))
                .expect("value");
            (columns, value)
        };
        assert_eq!(
            columns,
            vec![
                ("id".to_string(), "INTEGER".to_string()),
                ("span".to_string(), "REAL".to_string()),
            ]
        );
        assert_eq!(value, 1.5);
    }

    /// A rename is read off the file, so a process that never saw the earlier
    /// edits finishes the job. This is the standby taking over mid-chain: the
    /// owner applied `x -> y` and died, and the new owner's node declares `z`
    /// with no idea that `x` ever existed.
    #[test]
    fn a_process_that_never_saw_the_rename_still_applies_it() {
        let path = temp_db("midchain");
        let mut owner = TableNode::new();
        set(&mut owner, DB_PATH, &path);
        set(&mut owner, "name", "samples");
        set(&mut owner, "columns", "id:int\nx:float");
        owner.execute(&InputSet::new(), &mut ctx()).expect("create");
        set(&mut owner, "columns", "id:int\ny:float");
        owner.execute(&InputSet::new(), &mut ctx()).expect("x -> y");

        // A fresh node, as a standby's would be: it has never held `x`.
        let mut standby = TableNode::new();
        set(&mut standby, DB_PATH, &path);
        set(&mut standby, "name", "samples");
        set(&mut standby, "columns", "id:int\nz:float");
        standby
            .execute(&InputSet::new(), &mut ctx())
            .expect("y -> z");

        let columns = {
            let conn = open(&path).expect("open");
            let conn = conn.lock().unwrap_or_else(|e| e.into_inner());
            table_columns(&conn, "samples").expect("columns")
        };
        assert_eq!(
            columns,
            vec![
                ("id".to_string(), "INTEGER".to_string()),
                ("z".to_string(), "REAL".to_string()),
            ]
        );
    }

    /// What counts as a rename and what does not. Guessing wrong here moves
    /// someone's data under a name they did not choose.
    #[test]
    fn a_rename_is_one_column_gone_and_one_arrived_of_the_same_type() {
        let file = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(n, t)| (n.to_string(), t.to_string()))
                .collect()
        };
        let declared = |pairs: &[(&str, ColTy)]| -> Vec<(String, ColTy)> {
            pairs.iter().map(|(n, t)| (n.to_string(), *t)).collect()
        };
        let have = file(&[("id", "INTEGER"), ("x", "REAL")]);

        assert_eq!(
            renamed_column(&have, &declared(&[("id", ColTy::Int), ("y", ColTy::Float)])),
            Some(("x".to_string(), "y".to_string()))
        );
        // Nothing changed, a field added, a field removed: not renames.
        assert_eq!(
            renamed_column(&have, &declared(&[("id", ColTy::Int), ("x", ColTy::Float)])),
            None
        );
        assert_eq!(
            renamed_column(
                &have,
                &declared(&[("id", ColTy::Int), ("x", ColTy::Float), ("y", ColTy::Str)])
            ),
            None
        );
        assert_eq!(
            renamed_column(&have, &declared(&[("id", ColTy::Int)])),
            None
        );
        // Two of each is ambiguous, and a different type is not the same
        // column under another name.
        assert_eq!(
            renamed_column(&have, &declared(&[("a", ColTy::Int), ("b", ColTy::Float)])),
            None
        );
        assert_eq!(
            renamed_column(&have, &declared(&[("id", ColTy::Int), ("y", ColTy::Str)])),
            None
        );
    }

    /// A field removed from the node is dropped from the file, with the data
    /// of the columns beside it untouched. A retype is the one edit left that
    /// SQLite cannot do in place, and it has to say so.
    #[test]
    fn retyping_is_refused_and_dropping_drops() {
        let path = temp_db("mismatch");
        let mut node = TableNode::new();
        set(&mut node, DB_PATH, &path);
        set(&mut node, "name", "samples");
        set(&mut node, "columns", "id:int\nx:float\nnote:str");
        node.execute(&InputSet::new(), &mut ctx()).expect("create");
        {
            let conn = open(&path).expect("open");
            let conn = conn.lock().unwrap_or_else(|e| e.into_inner());
            conn.execute_batch("INSERT INTO \"samples\" (\"x\", \"note\") VALUES (1.5, 'hi')")
                .expect("row");
        }

        // The field is removed in the graph, so the column goes.
        set(&mut node, "columns", "id:int\nx:float");
        node.execute(&InputSet::new(), &mut ctx()).expect("drop");
        // And the next pass has nothing left to do.
        node.execute(&InputSet::new(), &mut ctx())
            .expect("idempotent");

        let (columns, value) = {
            let conn = open(&path).expect("open");
            let conn = conn.lock().unwrap_or_else(|e| e.into_inner());
            let columns = table_columns(&conn, "samples").expect("columns");
            let value: f64 = conn
                .query_row("SELECT \"x\" FROM \"samples\"", [], |row| row.get(0))
                .expect("value");
            (columns, value)
        };
        assert_eq!(
            columns,
            vec![
                ("id".to_string(), "INTEGER".to_string()),
                ("x".to_string(), "REAL".to_string()),
            ]
        );
        assert_eq!(value, 1.5);

        let mut retyped = TableNode::new();
        set(&mut retyped, DB_PATH, &path);
        set(&mut retyped, "name", "samples");
        set(&mut retyped, "columns", "id:int\nx:str");
        let error = retyped
            .execute(&InputSet::new(), &mut ctx())
            .expect_err("retyped column");
        assert!(
            error
                .to_string()
                .contains("x is REAL in the file, not TEXT")
        );
    }

    /// What SQLite will not drop, the user has to read from SQLite: the
    /// primary key is the column every table of these nodes has, and its
    /// refusal names the reason.
    #[test]
    fn a_column_sqlite_refuses_to_drop_reports_its_reason() {
        let path = temp_db("undroppable");
        let mut node = TableNode::new();
        set(&mut node, DB_PATH, &path);
        set(&mut node, "name", "samples");
        set(&mut node, "columns", "id:int\nx:float");
        node.execute(&InputSet::new(), &mut ctx()).expect("create");

        set(&mut node, "columns", "x:float");
        let error = node
            .execute(&InputSet::new(), &mut ctx())
            .expect_err("primary key");
        let text = error.to_string();
        assert!(
            text.contains("cannot drop column id from table samples"),
            "{text}"
        );
        assert!(text.to_lowercase().contains("primary key"), "{text}");
    }

    /// A node outside a database has no file, and the message has to say what
    /// to do about it.
    #[test]
    fn a_node_without_a_database_says_so() {
        let mut query = QueryNode::new();
        let mut run = InputSet::new();
        run.insert(
            "table",
            Value::new(TableRef {
                name: "samples".to_string(),
                columns: vec![("id".to_string(), ColTy::Int)],
            }),
        );
        run.mark_changed("run");
        let error = query.execute(&run, &mut ctx()).expect_err("no path");
        assert!(error.to_string().contains("db.database"));
    }

    /// The statement text is what the settings mean, in the order SQL wants.
    #[test]
    fn a_query_builds_its_statement_from_its_settings() {
        let mut node = QueryNode::new();
        set(&mut node, "where", " x > 1 ");
        set(&mut node, "limit", "3");
        assert_eq!(
            node.sql("samples"),
            "SELECT * FROM \"samples\" WHERE x > 1 ORDER BY id DESC LIMIT 3"
        );
    }

    /// A limit is canonicalized where it is typed. The field showing one
    /// bound while the query used another is what this replaces.
    #[test]
    fn a_limit_is_refused_or_clamped_when_it_is_set() {
        let mut node = QueryNode::new();
        set(&mut node, "limit", "3");

        for bad in ["lots", "", "-2", "3.5"] {
            let error = node
                .set_parameter("limit", Value::new(bad.to_string()))
                .expect_err("refused");
            // A refused setting is its own error kind, and its text is the
            // reason alone: it is drawn under the field, where "node execution
            // failed:" would say nothing the field does not already show.
            assert!(matches!(error, ZeughausError::InvalidParameter(_)), "{bad}");
            let text = error.to_string();
            assert!(text.starts_with("limit "), "{text}");
            assert!(text.contains("expected a row count"), "{text}");
        }
        let error = node
            .set_parameter("limit", Value::new("0".to_string()))
            .expect_err("refused");
        assert!(error.to_string().contains("limit 0"));
        // Every refusal left the accepted value in place.
        assert!(node.sql("samples").ends_with("LIMIT 3"));

        // An absurd one is capped rather than refused: the intent is clear.
        set(&mut node, "limit", "999999");
        assert!(node.sql("samples").ends_with("LIMIT 10000"));
    }

    /// A statement that returns no rows still reports what it did.
    #[test]
    fn raw_sql_reports_rows_for_a_query_and_changes_for_a_statement() {
        let path = temp_db("rawsql");
        let mut node = SqlNode::new();
        set(&mut node, DB_PATH, &path);
        let mut run = InputSet::new();
        run.mark_changed("run");

        set(&mut node, "sql", "CREATE TABLE t (a INTEGER)");
        node.execute(&run, &mut ctx()).expect("create");
        set(&mut node, "sql", "INSERT INTO t (a) VALUES (1), (2)");
        let mut inserting = ctx();
        node.execute(&run, &mut inserting).expect("insert");
        assert_eq!(
            inserting
                .take_outputs()
                .get("count")
                .and_then(Value::downcast_ref::<i64>),
            Some(&2)
        );

        set(&mut node, "sql", "SELECT a FROM t ORDER BY a");
        let mut reading = ctx();
        node.execute(&run, &mut reading).expect("select");
        let read = reading.take_outputs();
        assert_eq!(
            read.get("count").and_then(Value::downcast_ref::<i64>),
            Some(&2)
        );
        assert_eq!(
            read.get("rows").and_then(Value::downcast_ref::<String>),
            Some(&"[{\"a\":1},{\"a\":2}]".to_string())
        );
    }
}
