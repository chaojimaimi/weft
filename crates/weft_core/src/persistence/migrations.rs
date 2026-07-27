//! Schema evolution helpers used by [`BlockStore::open`](super::blocks::BlockStore::open).
//!
//! `CREATE TABLE IF NOT EXISTS` does not evolve databases created by an older
//! Weft version, so [`ensure_column`] adds missing columns explicitly. The
//! `active` flag on `tabs` is checked separately because it was introduced in
//! the v1.0 D5 migration alongside the original schema.

use rusqlite::{Connection, Error as SqliteError};

/// Add `column` to `table` if it does not already exist.
///
/// `definition` is the trailing SQL used in `ALTER TABLE … ADD COLUMN`
/// (e.g. `"TEXT"` or `"INTEGER NOT NULL DEFAULT 0"`). Idempotent: a no-op when
/// the column is already present.
pub(crate) fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), SqliteError> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
    if !names
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|name| name == column)
    {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        )?;
    }
    Ok(())
}

/// v1.0 D5: ensure the `tabs.active` column exists on legacy databases.
///
/// Returns `true` when the column was already present (or was added), and
/// `false` only if a fatal SQLite error occurred (the caller propagates).
pub(crate) fn ensure_tabs_active_column(conn: &Connection) -> Result<bool, SqliteError> {
    let mut stmt = conn.prepare("PRAGMA table_info(tabs)")?;
    let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let present = columns
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|name| name == "active");
    if !present {
        conn.execute(
            "ALTER TABLE tabs ADD COLUMN active INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    Ok(!present)
}

#[cfg(test)]
mod tests {
    use super::ensure_column;
    use rusqlite::Connection;

    #[test]
    fn ensure_column_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (a INTEGER)").unwrap();
        ensure_column(&conn, "t", "b", "TEXT").unwrap();
        ensure_column(&conn, "t", "b", "TEXT").unwrap();
        let mut stmt = conn.prepare("PRAGMA table_info(t)").unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(names.iter().any(|n| n == "b"));
    }
}
