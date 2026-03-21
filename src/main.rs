use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use rustdb::catalog;
use rustdb::sql;
use rustdb::storage;

use catalog::config::DbConfig;

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .init();

    let (data_dir, text_mode) = parse_args()?;
    log::info!("data_dir={}, text_mode={}", data_dir.display(), text_mode);

    let sqldbconf_path = data_dir.join("admin").join("SQLDBCONF");

    let config = if !sqldbconf_path.exists() {
        log::info!("bootstrapping new database at {}", data_dir.display());
        let cfg = DbConfig {
            text_mode,
            ..DbConfig::default()
        };
        catalog::bootstrap::bootstrap(&data_dir, &cfg)?;
        cfg
    } else {
        DbConfig::read(&data_dir)?
    };
    log::info!("config: PAGESIZE={}, DIAGLEVEL={}, TEXT_MODE={}",
        config.page_size, config.diag_level, config.text_mode);

    let catalog = catalog::loader::load_catalog(&data_dir, &config)?;
    let mut cache = catalog::cache::CatalogCache::new(catalog, config);
    log::info!("catalog cache built");

    let mut tsm = storage::tablespace::TablespaceManager::open(&data_dir, &cache)?;
    log::info!("tablespace manager opened");

    println!("RustDB — interactive SQL shell");
    println!("Type SQL queries or \\q to quit.\n");

    repl(&mut cache, &mut tsm)?;

    tsm.flush_all()?;
    log::info!("shutdown complete");

    Ok(())
}

fn repl(
    cache: &mut catalog::cache::CatalogCache,
    tsm: &mut storage::tablespace::TablespaceManager,
) -> anyhow::Result<()> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();

    loop {
        print!("rustdb> ");
        io::stdout().flush()?;

        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            // EOF
            println!();
            break;
        }

        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if input == "\\q" || input.eq_ignore_ascii_case("quit") || input.eq_ignore_ascii_case("exit") {
            break;
        }

        match sql::parser::parse(input) {
            Ok(stmts) => {
                for stmt in &stmts {
                    match sql::executor::execute(stmt, cache, tsm) {
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

fn parse_args() -> anyhow::Result<(PathBuf, bool)> {
    let args: Vec<String> = std::env::args().collect();
    let mut data_dir = PathBuf::from("./TESTDB");
    let mut text_mode = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--data-dir" => {
                i += 1;
                data_dir = PathBuf::from(
                    args.get(i)
                        .ok_or_else(|| anyhow::anyhow!("--data-dir requires a value"))?,
                );
            }
            "--text-mode" => {
                text_mode = true;
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
        i += 1;
    }
    Ok((data_dir, text_mode))
}
