use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("catalog error: {0}")]
    Catalog(String),

    #[error("data corruption: {0}")]
    Corruption(String),

    #[error("SQLSTATE {state}: {message}")]
    Sql { state: SqlState, message: String },
}

pub type Result<T> = std::result::Result<T, Error>;

/// ANSI SQL SQLSTATE codes.
///
/// Each code is a 5-character string: 2-char class + 3-char subclass.
/// Class "00" = success, "01" = warning, "02" = no data, all others = error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlState {
    /// 42000 — Syntax error or access rule violation
    SyntaxError,
    /// 42S02 — Base table or view not found
    TableNotFound,
    /// 42S22 — Column not found
    ColumnNotFound,
    /// 42601 — Syntax error (specific)
    ParseError,
    /// 42803 — Grouping error
    GroupingError,
    /// 0A000 — Feature not supported
    FeatureNotSupported,
    /// 22000 — Data exception (general)
    DataException,
    /// 22003 — Numeric value out of range
    NumericValueOutOfRange,
    /// 22005 — Error in assignment (type mismatch)
    AssignmentError,
    /// 23502 — NOT NULL integrity constraint violation
    NotNullViolation,
    /// 21S01 — Insert value list does not match column list
    InsertValueListMismatch,
    /// 26000 — Invalid SQL statement name
    InvalidSqlStatement,
    /// 42S01 — Base table already exists
    TableAlreadyExists,
    /// 54010 — Row size too large for page size
    RowTooLarge,
    /// 42711 — Duplicate column name in definition
    DuplicateColumnName,
    /// 42611 — Invalid length for character type
    InvalidColumnLength,
    /// 54011 — Too many columns in table definition
    TooManyColumns,
    /// 42508 — Cannot create object in system schema
    SystemSchemaViolation,
    /// 42P04 — Database already exists (PostgreSQL convention)
    DatabaseAlreadyExists,
    /// 3D000 — Invalid catalog name (database not found)
    DatabaseNotFound,
}

impl std::fmt::Display for SqlState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = match self {
            SqlState::SyntaxError => "42000",
            SqlState::TableNotFound => "42S02",
            SqlState::ColumnNotFound => "42S22",
            SqlState::ParseError => "42601",
            SqlState::GroupingError => "42803",
            SqlState::FeatureNotSupported => "0A000",
            SqlState::DataException => "22000",
            SqlState::NumericValueOutOfRange => "22003",
            SqlState::AssignmentError => "22005",
            SqlState::NotNullViolation => "23502",
            SqlState::InsertValueListMismatch => "21S01",
            SqlState::InvalidSqlStatement => "26000",
            SqlState::TableAlreadyExists => "42S01",
            SqlState::RowTooLarge => "54010",
            SqlState::DuplicateColumnName => "42711",
            SqlState::InvalidColumnLength => "42611",
            SqlState::TooManyColumns => "54011",
            SqlState::SystemSchemaViolation => "42508",
            SqlState::DatabaseAlreadyExists => "42P04",
            SqlState::DatabaseNotFound => "3D000",
        };
        write!(f, "{code}")
    }
}

/// Helper to construct an Sql error.
pub fn sql_error(state: SqlState, message: impl Into<String>) -> Error {
    Error::Sql {
        state,
        message: message.into(),
    }
}
