#![forbid(unsafe_code)]

use clap::Parser;
use tuskd::cli::Cli;

fn main() {
    let cli = Cli::parse();
    // Phases P1–P7 fill in command dispatch; the FTS5 boot probe runs before
    // anything touches a vault.
    if let Err(e) = tusk_core::fts::verify_fts5() {
        eprintln!("fatal: {e}");
        std::process::exit(1);
    }
    eprintln!("not yet implemented: {:?}", cli.command);
    std::process::exit(2);
}
