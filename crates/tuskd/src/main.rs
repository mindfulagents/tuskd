#![forbid(unsafe_code)]

use clap::Parser;
use tuskd::cli::Cli;

fn main() {
    let cli = Cli::parse();
    // FTS5 boot probe: fail loudly before touching any vault (spec §2.3).
    if let Err(e) = tusk_core::fts::verify_fts5() {
        eprintln!("fatal: {e}");
        std::process::exit(1);
    }
    std::process::exit(tuskd::commands::run(cli));
}
