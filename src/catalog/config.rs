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

/// Database configuration parameters stored in `admin/SQLDBCONF`.
///
/// Follows the DB2 convention of a per-database configuration file.
/// Each parameter is a key-value pair written as `KEY = VALUE`.
#[derive(Debug, Clone)]
pub struct DbConfig {
    /// Default page size in bytes for new tablespaces.
    pub page_size: usize,
    /// Diagnostic/logging level (OFF, ERROR, WARN, INFO, DEBUG, TRACE).
    pub diag_level: String,
    /// When true, catalog `.DAT` files use TSV text format instead of binary.
    pub text_mode: bool,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            page_size: DEFAULT_PAGE_SIZE,
            diag_level: "INFO".to_string(),
            text_mode: false,
        }
    }
}

impl DbConfig {
    /// Write the SQLDBCONF file into the `admin/` directory.
    pub fn write(&self, data_dir: &Path) -> Result<()> {
        let path = data_dir.join("admin").join(SQLDBCONF_FILE);
        let content = format!(
            "\
-- RustDB Database Configuration (SQLDBCONF)
-- Modify only when the database is offline.

-- Default page size (bytes) for new tablespaces.
PAGESIZE = {}

-- Diagnostic level: OFF | ERROR | WARN | INFO | DEBUG | TRACE
DIAGLEVEL = {}

-- Data file format: TRUE = tab-separated text, FALSE = binary.
TEXT_MODE = {}
",
            self.page_size,
            self.diag_level,
            if self.text_mode { "TRUE" } else { "FALSE" },
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

        Ok(Self {
            page_size,
            diag_level,
            text_mode,
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
        let dir = std::env::temp_dir().join("rustdb_test").join(name);
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
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reject_bad_page_size() {
        let dir = tmp_dir("cfg_bad_ps");
        fs::write(dir.join("admin/SQLDBCONF"), "PAGESIZE = 1000\n").unwrap();
        let err = DbConfig::read(&dir).unwrap_err();
        assert!(err.to_string().contains("power of two"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reject_bad_diaglevel() {
        let dir = tmp_dir("cfg_bad_dl");
        fs::write(dir.join("admin/SQLDBCONF"), "DIAGLEVEL = VERBOSE\n").unwrap();
        let err = DbConfig::read(&dir).unwrap_err();
        assert!(err.to_string().contains("invalid DIAGLEVEL"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
