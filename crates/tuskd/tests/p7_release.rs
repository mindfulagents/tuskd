//! P7 exit tests — export/import, --version (build-loop §2 P7).

use std::path::Path;
use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_opentusk"))
}

fn run_ok(vault: &Path, args: &[&str]) -> String {
    let out = bin().arg("--vault").arg(vault).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "opentusk {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn version_flag() {
    let out = bin().arg("--version").output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")), "{stdout}");
}

#[test]
fn export_then_import_roundtrip() {
    let src = tempfile::tempdir().unwrap();
    run_ok(src.path(), &["init"]);
    run_ok(
        src.path(),
        &[
            "agent",
            "create",
            "hermes-dev",
            "--read",
            "project:opentusk",
            "--promote",
            "project:opentusk",
        ],
    );

    // Put a record in via a promote through the CLI-embedded MCP path is
    // overkill here; write through a one-off stdio session instead.
    use std::io::{BufRead, BufReader, Write};
    let mut child = bin()
        .arg("--vault")
        .arg(src.path())
        .args(["mcp", "--agent", "hermes-dev"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#
    )
    .unwrap();
    stdout.read_line(&mut line).unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"memory_write","arguments":{{"content":"exported ocelot wisdom"}}}}}}"#
    )
    .unwrap();
    line.clear();
    stdout.read_line(&mut line).unwrap();
    assert!(line.contains("\"isError\":false"), "{line}");
    drop(stdin);
    let _ = child.wait();

    // Export.
    let archive = src.path().join("backup.tar.gz");
    let out = run_ok(src.path(), &["export", archive.to_str().unwrap()]);
    assert!(out.contains("exported"), "{out}");

    // Import into a fresh vault; search must find the record and the
    // keyring must have survived.
    let dst = tempfile::tempdir().unwrap();
    run_ok(dst.path(), &["import", archive.to_str().unwrap()]);
    let found = run_ok(dst.path(), &["search", "ocelot"]);
    assert!(found.contains("exported ocelot wisdom"), "{found}");
    let agents = run_ok(dst.path(), &["agent", "list"]);
    assert!(agents.contains("hermes-dev"), "{agents}");
}
