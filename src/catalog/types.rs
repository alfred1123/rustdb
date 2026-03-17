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
    pub id: i16,
    pub name: String,
    pub ts_type: String,
    pub page_size: i32,
    pub state: String,
}

#[derive(Debug, Clone)]
pub struct Schema {
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct Table {
    pub name: String,
    pub schema_name: String,
    pub tablespace_id: i16,
    pub col_count: i16,
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub table_name: String,
    pub schema_name: String,
    pub ordinal: i16,
    pub type_name: String,
    pub nullable: bool,
}

#[derive(Debug)]
pub struct Catalog {
    pub tablespaces: Vec<Tablespace>,
    pub schemas: Vec<Schema>,
    pub tables: Vec<Table>,
    pub columns: Vec<Column>,
}
