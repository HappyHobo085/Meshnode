//! Test-only helpers (`test-hooks` feature). Never enable in production builds.

use crate::StoreError;
use crate::keys;
use crate::tables::{META, meta};
use redb::Database;
use std::path::Path;

/// Rewrites the `schema` meta row of the closed store at `path`.
pub fn set_schema(path: &Path, schema: u64) -> Result<(), StoreError> {
    let db = Database::create(path)?;
    let tx = db.begin_write()?;
    {
        let mut m = tx.open_table(META)?;
        m.insert(meta::SCHEMA.as_bytes(), keys::u64_value(schema).as_slice())?;
    }
    tx.commit()?;
    Ok(())
}
