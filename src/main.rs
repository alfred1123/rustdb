use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use rqdb::db::DbState;
use rqdb::sql;

const DEFAULT_BASE_DIR: &str = "./data";

enum Subcommand {
    ConnectTo(String),
    CreateDatabase(String),
}

struct CliArgs {
    base_dir: PathBuf,
    text_mode: bool,
    subcommand: Option<Subcommand>,
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .init();

    let args = parse_args()?;
    log::info!("base_dir={}", args.base_dir.display());

    let mut db: Option<DbState> = match args.subcommand {
        Some(Subcommand::ConnectTo(ref name)) => {
            Some(rqdb::open_database(&args.base_dir, name)?)
        }
        Some(Subcommand::CreateDatabase(ref name)) => {
            Some(rqdb::create_database(&args.base_dir, name, args.text_mode)?)
        }
        None => None,
    };

    println!("RQDB — interactive SQL shell");
    println!("Type SQL queries, CONNECT TO <db>, CREATE DATABASE <db>, DISCONNECT, or \\q to quit.\n");

    repl(&mut db, &args.base_dir, args.text_mode)?;

    if let Some(ref mut db) = db {
        db.tsm.flush_all()?;
        log::info!("flushed database {}", db.name);
    }
    log::info!("shutdown complete");

    Ok(())
}

fn repl(
    db: &mut Option<DbState>,
    base_dir: &PathBuf,
    text_mode: bool,
) -> anyhow::Result<()> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();

    loop {
        match &*db {
            Some(state) => print!("rqdb:{}> ", state.name),
            None => print!("rqdb> "),
        }
        io::stdout().flush()?;

        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            println!();
            break;
        }

        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if input == "\\q"
            || input.eq_ignore_ascii_case("quit")
            || input.eq_ignore_ascii_case("exit")
        {
            break;
        }

        if let Some(db_name) = parse_connect_to(input) {
            handle_connect(db, base_dir, &db_name);
            continue;
        }
        if let Some(db_name) = parse_create_database(input) {
            handle_create_database(db, base_dir, &db_name, text_mode);
            continue;
        }
        if parse_disconnect(input) {
            handle_disconnect(db);
            continue;
        }

        let state = match db.as_mut() {
            Some(s) => s,
            None => {
                println!(
                    "Error: no database connected. \
                     Use CONNECT TO <dbname> or CREATE DATABASE <dbname>.\n"
                );
                continue;
            }
        };

        match sql::parser::parse(input) {
            Ok(stmts) => {
                for stmt in &stmts {
                    match sql::executor::execute(stmt, &mut state.cache, &mut state.tsm) {
                        Ok(rs) => println!("{}\n", rs.display()),
                        Err(e) => println!("Error: {e}\n"),
                    }
                }
            }
            Err(e) => println!("Error: {e}\n"),
        }
    }

    Ok(())
}

/// Parse `CONNECT TO <dbname>` (case-insensitive). Returns the database name.
fn parse_connect_to(input: &str) -> Option<String> {
    let upper = input.to_uppercase();
    let trimmed = upper.trim().trim_end_matches(';');
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() == 3 && parts[0] == "CONNECT" && parts[1] == "TO" {
        Some(parts[2].to_string())
    } else {
        None
    }
}

/// Parse `CREATE DATABASE <dbname>` (case-insensitive). Returns the database name.
fn parse_create_database(input: &str) -> Option<String> {
    let upper = input.to_uppercase();
    let trimmed = upper.trim().trim_end_matches(';');
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() == 3 && parts[0] == "CREATE" && parts[1] == "DATABASE" {
        Some(parts[2].to_string())
    } else {
        None
    }
}

/// Parse `DISCONNECT` (case-insensitive).
fn parse_disconnect(input: &str) -> bool {
    let upper = input.to_uppercase();
    let trimmed = upper.trim().trim_end_matches(';');
    trimmed == "DISCONNECT"
}

fn handle_disconnect(db: &mut Option<DbState>) {
    match db.take() {
        Some(mut old) => {
            if let Err(e) = old.tsm.flush_all() {
                println!("Warning: failed to flush {}: {e}", old.name);
            }
            println!("Disconnected from {}.\n", old.name);
        }
        None => {
            println!("No database is currently connected.\n");
        }
    }
}

fn handle_connect(db: &mut Option<DbState>, base_dir: &PathBuf, name: &str) {
    if let Some(old) = db.as_mut() {
        if let Err(e) = old.tsm.flush_all() {
            println!("Warning: failed to flush {}: {e}", old.name);
        }
    }
    match rqdb::open_database(base_dir, name) {
        Ok(new_db) => {
            println!("Connected to {}.\n", new_db.name);
            *db = Some(new_db);
        }
        Err(e) => {
            println!("Error: {e}\n");
        }
    }
}

fn handle_create_database(
    db: &mut Option<DbState>,
    base_dir: &PathBuf,
    name: &str,
    text_mode: bool,
) {
    if let Some(old) = db.as_mut() {
        if let Err(e) = old.tsm.flush_all() {
            println!("Warning: failed to flush {}: {e}", old.name);
        }
    }
    match rqdb::create_database(base_dir, name, text_mode) {
        Ok(new_db) => {
            println!("Database {} created.\n", new_db.name);
            *db = Some(new_db);
        }
        Err(e) => {
            println!("Error: {e}\n");
        }
    }
}

fn parse_args() -> anyhow::Result<CliArgs> {
    let args: Vec<String> = std::env::args().collect();
    let mut base_dir = PathBuf::from(DEFAULT_BASE_DIR);
    let mut text_mode = false;
    let mut subcommand: Option<Subcommand> = None;
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "--data-dir" => {
                i += 1;
                base_dir = PathBuf::from(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("--data-dir requires a value"))?,
                );
            }
            "--text-mode" => {
                text_mode = true;
            }
            "connect" => {
                if args.get(i + 1).map(|s| s.eq_ignore_ascii_case("to")) != Some(true) {
                    anyhow::bail!("expected: connect to <DBNAME>");
                }
                let db_name = args.get(i + 2).ok_or_else(|| {
                    anyhow::anyhow!("expected: connect to <DBNAME>")
                })?;
                subcommand = Some(Subcommand::ConnectTo(db_name.to_uppercase()));
                i += 2;
            }
            "create" => {
                if args.get(i + 1).map(|s| s.eq_ignore_ascii_case("database")) != Some(true) {
                    anyhow::bail!("expected: create database <DBNAME>");
                }
                let db_name = args.get(i + 2).ok_or_else(|| {
                    anyhow::anyhow!("expected: create database <DBNAME>")
                })?;
                subcommand = Some(Subcommand::CreateDatabase(db_name.to_uppercase()));
                i += 2;
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => anyhow::bail!(
                "unknown argument: {other}\n\nUsage: rqdb [OPTIONS] [COMMAND]\n\
                 Run 'rqdb --help' for details."
            ),
        }
        i += 1;
    }

    Ok(CliArgs {
        base_dir,
        text_mode,
        subcommand,
    })
}

fn print_usage() {
    println!(
        "\
RQDB — a transactional relational database engine

USAGE:
    rqdb                                Start REPL (no database connected)
    rqdb connect to <DBNAME>            Connect to an existing database
    rqdb create database <DBNAME>       Create and connect to a new database

OPTIONS:
    --data-dir <PATH>   Base directory for databases (default: ./data)
    --text-mode         Use TSV text format instead of binary (for debugging)
    --help, -h          Show this help message

REPL COMMANDS:
    CONNECT TO <DBNAME>         Connect to an existing database
    CREATE DATABASE <DBNAME>    Create and connect to a new database
    DISCONNECT                  Disconnect from the current database
    \\q / quit / exit            Exit the shell

Database paths resolve to <data-dir>/<DBNAME> (e.g. ./data/MYDB)."
    );
}
