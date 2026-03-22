use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::error::{Error, Result};

/// Database configuration file name (DB2 convention).
const SQLDBCONF_FILE: &str = "SQLDBCONF";

/// Default page size in bytes, used when SQLDBCONF does not specify one.
const DEFAULT_PAGE_SIZE: usize = 4096;

/// Diagnostic level names (DB2-style DIAGLEVEL).
const VALID_DIAGLEVELS: &[&str] = &["OFF", "ERROR", "WARN", "INFO", "DEBUG", "TRACE"];

/// Default schema for unqualified table references (DB2: DFT_SCHEMA).
const DEFAULT_DFT_SCHEMA: &str = "PUBLIC";

/// Default tablespace name for user tables (DB2: DFT_TBSP).
const DEFAULT_DFT_TABLESPACE: &str = "USERTBSP";

/// Default system catalog schema name.
const DEFAULT_SYS_SCHEMA: &str = "RQSYS";

/// On-disk format version. Incremented on breaking changes (e.g. MVCC
/// tuple headers). Databases created with older versions are rejected
/// with a clear error on open.
pub const FORMAT_VERSION: u32 = 2;

/// Database configuration parameters stored in `admin/SQLDBCONF`.
///
/// Follows the DB2 convention of a per-database configuration file.
/// Each parameter is a key-value pair written as `KEY = VALUE`.
#[derive(Debug, Clone)]
pub struct DbConfig {
    /// On-disk format version for backward-compatibility detection.
    pub format_version: u32,
    /// Default page size in bytes for new tablespaces.
    pub page_size: usize,
    /// Diagnostic/logging level (OFF, ERROR, WARN, INFO, DEBUG, TRACE).
    pub diag_level: String,
    /// When true, catalog `.DAT` files use TSV text format instead of binary.
    pub text_mode: bool,
    /// Default schema for unqualified table names (DB2: DFT_SCHEMA).
    pub default_schema: String,
    /// Default tablespace name for user-created tables (DB2: DFT_TBSP).
    pub default_tablespace: String,
    /// System catalog schema name (e.g. `RQSYS`).  All catalog tables live here.
    pub sys_schema: String,
    /// Next transaction ID to allocate (persisted for monotonic TxIDs).
    pub next_txid: u64,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            page_size: DEFAULT_PAGE_SIZE,
            diag_level: "INFO".to_string(),
            text_mode: false,
            default_schema: DEFAULT_DFT_SCHEMA.to_string(),
            default_tablespace: DEFAULT_DFT_TABLESPACE.to_string(),
            sys_schema: DEFAULT_SYS_SCHEMA.to_string(),
            next_txid: 1,
        }
    }
}

impl DbConfig {
    /// Write the SQLDBCONF file into the `admin/` directory.
    pub fn write(&self, data_dir: &Path) -> Result<()> {
        let path = data_dir.join("admin").join(SQLDBCONF_FILE);
        let content = format!(
            "\
-- RQDB Database Configuration (SQLDBCONF)
-- Modify only when the database is offline.

-- On-disk format version.  Do not change manually.
FORMAT_VERSION = {}

-- Default page size (bytes) for new tablespaces.
PAGESIZE = {}

-- Diagnostic level: OFF | ERROR | WARN | INFO | DEBUG | TRACE
DIAGLEVEL = {}

-- Data file format: TRUE = tab-separated text, FALSE = binary.
TEXT_MODE = {}

-- Default schema for unqualified table names (DB2: DFT_SCHEMA).
DFT_SCHEMA = {}

-- Default tablespace for user-created tables (DB2: DFT_TBSP).
DFT_TBSP = {}

-- System catalog schema name.  Do not change after bootstrap.
SYS_SCHEMA = {}

-- Next transaction ID (monotonic counter).  Updated on shutdown.
NEXT_TXID = {}
",
            self.format_version,
            self.page_size,
            self.diag_level,
            if self.text_mode { "TRUE" } else { "FALSE" },
            self.default_schema,
            self.default_tablespace,
            self.sys_schema,
            self.next_txid,
        );
        fs::write(&path, content)?;
        log::info!("wrote {}", path.display());
        Ok(())
    }

    /// Read the SQLDBCONF file from the `admin/` directory.
    pub fn read(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("admin").join(SQLDBCONF_FILE);
        let content = fs::read_to_string(&path)?;
        let map = parse_kv(&content)?;

        // Format version check — reject databases from older (incompatible) versions.
        // Databases without FORMAT_VERSION are version 1 (pre-MVCC).
        let format_version = parse_u32(&map, "FORMAT_VERSION", 1)?;
        if format_version != FORMAT_VERSION {
            return Err(Error::Catalog(format!(
                "incompatible database format version {} (expected {}). \
                 This database was created with {} version of RQDB and cannot \
                 be opened. Please recreate the database.",
                format_version,
                FORMAT_VERSION,
                if format_version < FORMAT_VERSION { "an older" } else { "a newer" },
            )));
        }

        let page_size = parse_usize(&map, "PAGESIZE", DEFAULT_PAGE_SIZE)?;
        if !page_size.is_power_of_two() || page_size < 512 {
            return Err(Error::Catalog(format!(
                "PAGESIZE must be a power of two >= 512, got {page_size}"
            )));
        }

        let diag_level = map
            .get("DIAGLEVEL")
            .cloned()
            .unwrap_or_else(|| "INFO".to_string());
        if !VALID_DIAGLEVELS.contains(&diag_level.as_str()) {
            return Err(Error::Catalog(format!(
                "invalid DIAGLEVEL '{}'; expected one of: {}",
                diag_level,
                VALID_DIAGLEVELS.join(", ")
            )));
        }

        let text_mode = parse_bool(&map, "TEXT_MODE", false)?;

        let default_schema = map
            .get("DFT_SCHEMA")
            .cloned()
            .unwrap_or_else(|| DEFAULT_DFT_SCHEMA.to_string());

        let default_tablespace = map
            .get("DFT_TBSP")
            .cloned()
            .unwrap_or_else(|| DEFAULT_DFT_TABLESPACE.to_string());

        let sys_schema = map
            .get("SYS_SCHEMA")
            .cloned()
            .unwrap_or_else(|| DEFAULT_SYS_SCHEMA.to_string());

        let next_txid = parse_u64(&map, "NEXT_TXID", 1)?;

        Ok(Self {
            format_version,
            page_size,
            diag_level,
            text_mode,
            default_schema,
            default_tablespace,
            sys_schema,
            next_txid,
        })
    }
}

/// Parse a simple `KEY = VALUE` text format, ignoring blank lines and `--` comments.
fn parse_kv(content: &str) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    for (line_no, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("--") {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or_else(|| {
            Error::Catalog(format!(
                "SQLDBCONF line {}: expected KEY = VALUE, got: {line}",
                line_no + 1
            ))
        })?;
        map.insert(key.trim().to_uppercase(), value.trim().to_string());
    }
    Ok(map)
}

fn parse_u32(map: &HashMap<String, String>, key: &str, default: u32) -> Result<u32> {
    match map.get(key) {
        Some(v) => v.parse::<u32>().map_err(|_| {
            Error::Catalog(format!("SQLDBCONF: invalid numeric value for {key}: {v}"))
        }),
        None => Ok(default),
    }
}

fn parse_u64(map: &HashMap<String, String>, key: &str, default: u64) -> Result<u64> {
    match map.get(key) {
        Some(v) => v.parse::<u64>().map_err(|_| {
            Error::Catalog(format!("SQLDBCONF: invalid numeric value for {key}: {v}"))
        }),
        None => Ok(default),
    }
}

fn parse_usize(map: &HashMap<String, String>, key: &str, default: usize) -> Result<usize> {
    match map.get(key) {
        Some(v) => v.parse::<usize>().map_err(|_| {
            Error::Catalog(format!("SQLDBCONF: invalid numeric value for {key}: {v}"))
        }),
        None => Ok(default),
    }
}

fn parse_bool(map: &HashMap<String, String>, key: &str, default: bool) -> Result<bool> {
    match map.get(key) {
        Some(v) => match v.as_str() {
            "TRUE" => Ok(true),
            "FALSE" => Ok(false),
            _ => Err(Error::Catalog(format!(
                "SQLDBCONF: invalid boolean for {key}: {v} (expected TRUE or FALSE)"
            ))),
        },
        None => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("rqdb_test").join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("admin")).unwrap();
        dir
    }

    #[test]
    fn roundtrip_default_config() {
        let dir = tmp_dir("cfg_test");
        let cfg = DbConfig::default();
        cfg.write(&dir).unwrap();
        let loaded = DbConfig::read(&dir).unwrap();
        assert_eq!(loaded.page_size, DEFAULT_PAGE_SIZE);
        assert_eq!(loaded.diag_level, "INFO");
        assert!(!loaded.text_mode);
        assert_eq!(loaded.default_schema, "PUBLIC");
        assert_eq!(loaded.default_tablespace, "USERTBSP");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reject_bad_page_size() {
        let dir = tmp_dir("cfg_bad_ps");
        fs::write(
            dir.join("admin/SQLDBCONF"),
            format!("FORMAT_VERSION = {FORMAT_VERSION}\nPAGESIZE = 1000\n"),
        ).unwrap();
        let err = DbConfig::read(&dir).unwrap_err();
        assert!(err.to_string().contains("power of two"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reject_bad_diaglevel() {
        let dir = tmp_dir("cfg_bad_dl");
        fs::write(
            dir.join("admin/SQLDBCONF"),
            format!("FORMAT_VERSION = {FORMAT_VERSION}\nDIAGLEVEL = VERBOSE\n"),
        ).unwrap();
        let err = DbConfig::read(&dir).unwrap_err();
        assert!(err.to_string().contains("invalid DIAGLEVEL"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reject_old_format_version() {
        let dir = tmp_dir("cfg_old_ver");
        fs::write(dir.join("admin/SQLDBCONF"), "FORMAT_VERSION = 1\n").unwrap();
        let err = DbConfig::read(&dir).unwrap_err();
        assert!(err.to_string().contains("incompatible database format"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn roundtrip_next_txid() {
        let dir = tmp_dir("cfg_txid");
        let mut cfg = DbConfig::default();
        cfg.next_txid = 42;
        cfg.write(&dir).unwrap();
        let loaded = DbConfig::read(&dir).unwrap();
        assert_eq!(loaded.next_txid, 42);
        assert_eq!(loaded.format_version, FORMAT_VERSION);
        fs::remove_dir_all(&dir).unwrap();
    }
}
