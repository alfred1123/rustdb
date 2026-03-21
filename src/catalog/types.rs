/// Minimum valid length for CHAR / VARCHAR columns (ANSI SQL).
pub const MIN_CHAR_LENGTH: u64 = 1;

/// Maximum valid length for CHAR / VARCHAR columns.
/// Follows the DB2 convention: 32 672 bytes per column.
pub const MAX_CHAR_LENGTH: u64 = 32672;

#[derive(Debug, Clone, PartialEq)]
pub enum DataType {
    SmallInt,
    Integer,
    BigInt,
    Char(u16),
    Varchar(u16),
    Double,
    Timestamp,
}

#[derive(Debug, Clone)]
pub struct Tablespace {
    pub tbspaceid: i32,
    pub tbspace: String,
    pub tbspacetype: String,
    pub datatype: String,
    pub pagesize: i32,
    pub state: String,
    pub bufferpoolid: i32,
}

#[derive(Debug, Clone)]
pub struct Schema {
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct Table {
    pub tableid: i32,
    pub name: String,
    pub schemaname: String,
    pub tbspaceid: i16,
    pub colcount: i16,
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub tabname: String,
    pub schemaname: String,
    pub ordinal: i16,
    pub typename: String,
    pub nullable: bool,
}

#[derive(Debug, Clone)]
pub struct BufferPool {
    pub bpid: i32,
    pub bpname: String,
    pub pagesize: i32,
    pub npages: i32,
}

#[derive(Debug)]
pub struct Catalog {
    pub tablespaces: Vec<Tablespace>,
    pub schemas: Vec<Schema>,
    pub tables: Vec<Table>,
    pub columns: Vec<Column>,
    pub bufferpools: Vec<BufferPool>,
}
