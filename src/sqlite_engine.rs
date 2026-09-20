//! Disk-free qualification of the actual linked SQLite engine.
//!
//! This leaf depends only on rusqlite and std. Engine qualification is not
//! permission to open a persistent store: each caller must still perform its
//! own path, filesystem, ownership, schema, and transaction admission checks.

use rusqlite::Connection;
use std::fmt;

pub const RUSQLITE_VERSION: &str = "0.40.2";
pub const QUALIFIED_SQLITE_VERSION: &str = "3.53.2";
pub const QUALIFIED_SQLITE_VERSION_NUMBER: i32 = 3_053_002;
pub const QUALIFIED_SQLITE_SOURCE_ID: &str =
    "2026-06-03 19:12:13 d6e03d8c777cfa2d35e3b60d8ec3e0187f3e9f99d8e2ee9cac695fd6fcdf1a24";
const MIN_SQLITE_VERSION: i32 = 3_051_003;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineIdentity {
    pub version: String,
    pub version_number: i32,
    pub source_id: String,
    pub rust_dependency: &'static str,
}

/// Retains typed SQLite errors for adapter-specific classification, without
/// exposing their details in diagnostics or an automatic error source chain.
pub enum EngineQualificationError {
    Unqualified,
    Sqlite(rusqlite::Error),
}

impl fmt::Display for EngineQualificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unqualified => "linked SQLite engine is not the qualified version and source",
            Self::Sqlite(_) => "linked SQLite engine could not be inspected",
        })
    }
}

impl fmt::Debug for EngineQualificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unqualified => f.write_str("EngineQualificationError::Unqualified"),
            Self::Sqlite(_) => f.write_str("EngineQualificationError::Sqlite(<private>)"),
        }
    }
}

impl std::error::Error for EngineQualificationError {}

impl From<rusqlite::Error> for EngineQualificationError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

fn validate_engine(
    version_number: i32,
    version: &str,
    source_id: &str,
) -> Result<(), EngineQualificationError> {
    if version_number < MIN_SQLITE_VERSION
        || version_number != QUALIFIED_SQLITE_VERSION_NUMBER
        || version != QUALIFIED_SQLITE_VERSION
        || source_id != QUALIFIED_SQLITE_SOURCE_ID
    {
        return Err(EngineQualificationError::Unqualified);
    }
    Ok(())
}

/// Inspect an in-memory connection, never a filesystem path or platform store.
pub fn linked_engine() -> Result<EngineIdentity, EngineQualificationError> {
    let version_number = rusqlite::version_number();
    if version_number < MIN_SQLITE_VERSION {
        return Err(EngineQualificationError::Unqualified);
    }
    let connection = Connection::open_in_memory()?;
    let (version, source_id): (String, String) =
        connection.query_row("SELECT sqlite_version(), sqlite_source_id()", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    validate_engine(version_number, &version, &source_id)?;
    Ok(EngineIdentity {
        version,
        version_number,
        source_id,
        rust_dependency: RUSQLITE_VERSION,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_qualified_identity_is_accepted() {
        assert!(
            validate_engine(
                QUALIFIED_SQLITE_VERSION_NUMBER,
                QUALIFIED_SQLITE_VERSION,
                QUALIFIED_SQLITE_SOURCE_ID,
            )
            .is_ok()
        );
    }

    #[test]
    fn minimum_is_not_a_substitute_for_the_exact_numeric_version() {
        for number in [
            0,
            3_050_004,
            3_051_002,
            MIN_SQLITE_VERSION,
            3_053_001,
            3_053_003,
        ] {
            assert!(matches!(
                validate_engine(number, QUALIFIED_SQLITE_VERSION, QUALIFIED_SQLITE_SOURCE_ID),
                Err(EngineQualificationError::Unqualified)
            ));
        }
    }

    #[test]
    fn version_text_is_checked_independently() {
        for version in ["", "3.51.3", "3.53.1", "3.53.3", "3.53.2 "] {
            assert!(matches!(
                validate_engine(
                    QUALIFIED_SQLITE_VERSION_NUMBER,
                    version,
                    QUALIFIED_SQLITE_SOURCE_ID
                ),
                Err(EngineQualificationError::Unqualified)
            ));
        }
    }

    #[test]
    fn source_identity_is_checked_independently() {
        for source in ["", "unexpected-source", QUALIFIED_SQLITE_VERSION] {
            assert!(matches!(
                validate_engine(
                    QUALIFIED_SQLITE_VERSION_NUMBER,
                    QUALIFIED_SQLITE_VERSION,
                    source
                ),
                Err(EngineQualificationError::Unqualified)
            ));
        }
    }

    #[test]
    fn linked_engine_is_the_qualified_bundle() {
        let identity = linked_engine().unwrap();
        assert_eq!(identity.version_number, QUALIFIED_SQLITE_VERSION_NUMBER);
        assert_eq!(identity.version, QUALIFIED_SQLITE_VERSION);
        assert_eq!(identity.source_id, QUALIFIED_SQLITE_SOURCE_ID);
        assert_eq!(identity.rust_dependency, RUSQLITE_VERSION);
    }

    #[test]
    fn inspection_diagnostics_do_not_expose_sqlite_details() {
        let error = EngineQualificationError::from(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseBusy,
                extended_code: 0,
            },
            Some("synthetic-private-sqlite-detail".to_owned()),
        ));
        assert!(!format!("{error:?}: {error}").contains("synthetic-private"));
        assert!(std::error::Error::source(&error).is_none());
        assert!(matches!(
            error,
            EngineQualificationError::Sqlite(ref sqlite)
                if sqlite.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy)
        ));
    }
}
