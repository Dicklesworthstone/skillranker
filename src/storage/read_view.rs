//! Keep a wide/rerank read on one connection's observed database revision.
//!
//! A deferred transaction protects each row read, not two separate calls to
//! CacheStore::response. Another connection can publish between those calls.
//! data_version detects those commits; total_changes also detects writes made
//! through this same connection. Neither value is compared across connections.

use super::StoreError;
use crate::cache::RequestStage;
use rusqlite::Transaction;

#[derive(Clone, Copy)]
struct Revision {
    data_version: i64,
    total_changes: i64,
}

#[derive(Default)]
pub(super) struct ResponseReadView {
    wide: Option<([u8; 32], Revision)>,
}

impl ResponseReadView {
    /// Call after check_stamp has established the transaction's read snapshot,
    /// before reading a response. A new Wide starts a fresh pair lookup. A
    /// standalone Rerank or one in another namespace does not reuse its view.
    /// Unrelated committed changes may conservatively reject a cache hit.
    pub(super) fn observe(
        &mut self,
        tx: &Transaction<'_>,
        namespace: [u8; 32],
        stage: RequestStage,
    ) -> Result<(), StoreError> {
        let revision = Revision {
            data_version: tx.pragma_query_value(None, "data_version", |row| row.get(0))?,
            total_changes: tx.query_row("SELECT total_changes()", [], |row| row.get(0))?,
        };
        match stage {
            RequestStage::Wide => self.wide = Some((namespace, revision)),
            RequestStage::Rerank => {
                if let Some((wide_namespace, wide)) = self.wide
                    && wide_namespace == namespace
                    && (wide.data_version != revision.data_version
                        || wide.total_changes != revision.total_changes)
                {
                    return Err(StoreError::StaleGeneration);
                }
            }
        }
        Ok(())
    }
}
