pub mod types;
pub mod row;
pub mod bootstrap;
pub mod cache;
pub mod config;
pub mod loader;

/// System catalog schema name.  All catalog tables live here.
/// The `SYSTEMFLAG` column in `SYSSCHEMAS` marks which schemas are protected;
/// this constant is used only for file-path construction and catalog table keys.
pub const SYSTEM_SCHEMA: &str = "RQSYS";
