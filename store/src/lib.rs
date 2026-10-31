//! Session-directory storage for miniperf: every source writes Parquet tables
//! into the session directory, and all consumers query them through an
//! in-memory DuckDB with one view per table.

#[cfg(feature = "session")]
pub mod query;
#[cfg(feature = "session")]
mod recording;
#[cfg(feature = "session")]
mod session;

pub use miniperf_store_writer::*;
#[cfg(feature = "session")]
pub use recording::Recording;
#[cfg(feature = "session")]
pub use session::{Session, table_base_name};

#[cfg(feature = "session")]
pub use duckdb;
