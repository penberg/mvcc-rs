//! Multiversion concurrency control (MVCC) for Rust.
//!
//! This module implements the main memory MVCC method outlined in the paper
//! "High-Performance Concurrency Control Mechanisms for Main-Memory Databases"
//! by Per-Åke Larson et al (VLDB, 2011).
//!
//! ## Data anomalies
//!
//! * A *dirty write* occurs when transaction T_m updates a value that is written by
//!   transaction T_n but not yet committed. The MVCC algorithm prevents dirty
//!   writes by validating that a row version is visible to transaction T_m before
//!   allowing update to it.
//!
//! * A *dirty read* occurs when transaction T_m reads a value that was written by
//!   transaction T_n but not yet committed. The MVCC algorithm prevents dirty
//!   reads by validating that a row version is visible to transaction T_m.
//!
//! * A *fuzzy read* (non-repeatable read) occurs when transaction T_m reads a
//!   different value in the course of the transaction because another
//!   transaction T_n has updated the value.
//!
//! * A *lost update* occurs when transactions T_m and T_n both attempt to update
//!   the same value, resulting in one of the updates being lost. The MVCC algorithm
//!   prevents lost updates by detecting the write-write conflict and letting the
//!   first-writer win by aborting the later transaction.
//!
//! TODO: phantom reads, cursor lost updates, read skew, write skew.
//!
//! ## TODO
//!
//! * Optimistic reads and writes
//! * Garbage collection

pub mod clock;
pub mod database;
pub mod errors;
pub mod persistent_storage;
pub mod sync;

#[cfg(feature = "c_bindings")]
mod c_bindings {
    use super::*;
    type Clock = clock::LocalClock;
    type Storage = persistent_storage::JsonOnDisk;
    type Inner = database::DatabaseInner<Clock, Storage>;
    type Db = database::Database<Clock, Storage, tokio::sync::Mutex<Inner>>;

    static INIT_RUST_LOG: std::sync::Once = std::sync::Once::new();

    #[repr(C)]
    pub struct DbContext {
        db: Db,
        runtime: tokio::runtime::Runtime,
    }

    #[no_mangle]
    pub extern "C" fn mvccrs_new_database(path: *const std::ffi::c_char) -> *mut DbContext {
        INIT_RUST_LOG.call_once(|| {
            tracing_subscriber::fmt::init();
        });

        tracing::debug!("mvccrs_new_database");

        let clock = clock::LocalClock::new();
        let path = unsafe { std::ffi::CStr::from_ptr(path) };
        let path = match path.to_str() {
            Ok(path) => path,
            Err(_) => {
                tracing::error!("Invalid UTF-8 path");
                return std::ptr::null_mut();
            }
        };
        tracing::debug!("mvccrs: opening persistent storage at {path}");
        let storage = crate::persistent_storage::JsonOnDisk::new(path);
        let db = Db::new(clock, storage);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        Box::into_raw(Box::new(DbContext { db, runtime }))
    }

    #[no_mangle]
    pub unsafe extern "C" fn mvccrs_free_database(db: *mut Db) {
        tracing::debug!("mvccrs_free_database");
        let _ = Box::from_raw(db);
    }

    #[no_mangle]
    pub unsafe extern "C" fn mvccrs_insert(
        db: *mut DbContext,
        id: u64,
        value_ptr: *const u8,
        value_len: usize,
    ) -> i32 {
        let value = std::slice::from_raw_parts(value_ptr, value_len);
        let data = match std::str::from_utf8(value) {
            Ok(value) => value.to_string(),
            Err(_) => {
                tracing::info!("Invalid UTF-8, let's base64 this fellow");
                use base64::{engine::general_purpose, Engine as _};
                general_purpose::STANDARD.encode(value)
            }
        };
        let DbContext { db, runtime } = unsafe { &mut *db };
        let row = database::Row { id, data };
        tracing::debug!("mvccrs_insert: {row:?}");
        match runtime.block_on(async move {
            let tx = db.begin_tx().await;
            db.insert(tx, row).await?;
            db.commit_tx(tx).await
        }) {
            Ok(_) => {
                tracing::debug!("mvccrs_insert: success");
                0 // SQLITE_OK
            }
            Err(e) => {
                tracing::error!("mvccrs_insert: {e}");
                778 // SQLITE_IOERR_WRITE
            }
        }
    }
}
