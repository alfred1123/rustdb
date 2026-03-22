use std::path::Path;

use crate::catalog;
use crate::catalog::cache::CatalogCache;
use crate::catalog::config::DbConfig;
use crate::error::{sql_error, SqlState};
use crate::storage::tablespace::TablespaceManager;

/// Active database state: catalog cache, tablespace manager, and metadata.
pub struct DbState {
    pub name: String,
    pub cache: CatalogCache,
    pub tsm: TablespaceManager,
}

/// Open an existing database. Errors if `admin/SQLDBCONF` does not exist.
pub fn open_database(base_dir: &Path, name: &str) -> Result<DbState, crate::error::Error> {
    let data_dir = base_dir.join(name);
    let sqldbconf = data_dir.join("admin").join("SQLDBCONF");
    if !sqldbconf.exists() {
        return Err(sql_error(
            SqlState::DatabaseNotFound,
            format!(
                "database '{}' not found at {}. Use CREATE DATABASE {} to create it.",
                name,
                data_dir.display(),
                name
            ),
        ));
    }

    let config = DbConfig::read(&data_dir)?;
    log::info!(
        "connecting to {}: PAGESIZE={}, TEXT_MODE={}",
        name, config.page_size, config.text_mode
    );

    let cat = catalog::loader::load_catalog(&data_dir, &config)?;
    let cache = CatalogCache::new(cat, config);
    let tsm = TablespaceManager::open(&data_dir, &cache)?;
    log::info!("connected to database {}", name);

    Ok(DbState { name: name.to_uppercase(), cache, tsm })
}

/// Create a new database. Errors if `admin/SQLDBCONF` already exists.
pub fn create_database(
    base_dir: &Path,
    name: &str,
    text_mode: bool,
) -> Result<DbState, crate::error::Error> {
    let data_dir = base_dir.join(name);
    let sqldbconf = data_dir.join("admin").join("SQLDBCONF");
    if sqldbconf.exists() {
        return Err(sql_error(
            SqlState::DatabaseAlreadyExists,
            format!(
                "database '{}' already exists at {}. Use CONNECT TO {} to connect.",
                name,
                data_dir.display(),
                name
            ),
        ));
    }

    let cfg = DbConfig {
        text_mode,
        ..DbConfig::default()
    };
    log::info!("creating database {} at {}", name, data_dir.display());
    catalog::bootstrap::bootstrap(&data_dir, &cfg)?;

    let cat = catalog::loader::load_catalog(&data_dir, &cfg)?;
    let cache = CatalogCache::new(cat, cfg);
    let tsm = TablespaceManager::open(&data_dir, &cache)?;
    log::info!("database {} created and connected", name);

    Ok(DbState { name: name.to_uppercase(), cache, tsm })
}
