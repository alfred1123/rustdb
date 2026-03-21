pub mod types;
pub mod row;
pub mod bootstrap;
pub mod cache;
pub mod config;
pub mod loader;

/// System catalog schema name.  All catalog tables live here.
pub const SYSTEM_SCHEMA: &str = "RQSYS";
