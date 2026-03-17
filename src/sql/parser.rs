use sqlparser::ast::Statement;
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

use crate::error::{sql_error, Result, SqlState};

/// Parse a SQL string into a list of statements.
pub fn parse(sql: &str) -> Result<Vec<Statement>> {
    let dialect = GenericDialect {};
    Parser::parse_sql(&dialect, sql)
        .map_err(|e| sql_error(SqlState::ParseError, format!("{e}")))
}
