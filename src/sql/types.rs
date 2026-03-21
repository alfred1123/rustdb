use crate::error::Result;

/// A single value in a result row.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    SmallInt(i16),
    Integer(i32),
    BigInt(i64),
    Str(String),
    Bool(bool),
    Null,
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::SmallInt(v) => write!(f, "{v}"),
            Value::Integer(v) => write!(f, "{v}"),
            Value::BigInt(v) => write!(f, "{v}"),
            Value::Str(v) => write!(f, "{v}"),
            Value::Bool(v) => write!(f, "{}", if *v { "Y" } else { "N" }),
            Value::Null => write!(f, "NULL"),
        }
    }
}

/// Result set returned by a query.
#[derive(Debug)]
pub struct ResultSet {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

impl ResultSet {
    /// Format the result set as a table for display.
    pub fn display(&self) -> String {
        if self.columns.is_empty() {
            return String::from("(empty result set)");
        }

        // Calculate column widths.
        let mut widths: Vec<usize> = self.columns.iter().map(|c| c.len()).collect();
        for row in &self.rows {
            for (i, val) in row.iter().enumerate() {
                let len = val.to_string().len();
                if len > widths[i] {
                    widths[i] = len;
                }
            }
        }

        let mut out = String::new();

        // Header.
        for (i, col) in self.columns.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            out.push_str(&format!("{:<width$}", col, width = widths[i]));
        }
        out.push('\n');

        // Separator.
        for (i, w) in widths.iter().enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            out.push_str(&"-".repeat(*w));
        }
        out.push('\n');

        // Data rows.
        for row in &self.rows {
            for (i, val) in row.iter().enumerate() {
                if i > 0 {
                    out.push_str("  ");
                }
                out.push_str(&format!("{:<width$}", val, width = widths[i]));
            }
            out.push('\n');
        }

        out.push_str(&format!("\n({} row{})", self.rows.len(),
            if self.rows.len() == 1 { "" } else { "s" }));
        out
    }
}

/// Resolved table reference (schema + table name).
#[derive(Debug, Clone)]
pub struct TableRef {
    pub schema: String,
    pub table: String,
}

impl TableRef {
    pub fn resolve(schema: Option<&str>, table: &str, default_schema: &str) -> Result<Self> {
        let schema = schema.unwrap_or(default_schema).to_uppercase();
        let table = table.to_uppercase();
        Ok(Self { schema, table })
    }
}
