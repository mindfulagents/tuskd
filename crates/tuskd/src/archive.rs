//! `tuskd export | import` — tar.gz of the vault. The index is derived
//! and excluded; import rebuilds it.

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use std::fs::File;
use std::path::{Component, Path, PathBuf};
use tusk_core::error::CoreError;

/// Skip derived/runtime/secret files: SQLite index (+WAL/SHM), lock, sockets,
/// the daemon's per-run operator token, agent private signing keys
/// (D17 — secrets never travel in archives; applies to import too as
/// defense in depth against hand-crafted archives), and device-local sync
/// state (D21 — `.tusk/sync/` holds the device private key plus a journal
/// and identity that belong to this install; a restored vault re-derives
/// its journal via the reconciliation scan).
pub(crate) fn skip(rel: &Path) -> bool {
    let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.starts_with("index.db")
        || name == "lock"
        || name.ends_with(".sock")
        || name == "admin-token"
        || rel.starts_with(".tusk/keyring/keys")
        || rel.starts_with(".tusk/sync")
}

/// Returns the number of files archived (callers print/report).
pub fn export(vault: &Path, archive: &Path) -> Result<usize, CoreError> {
    if !vault.join(".tusk").is_dir() {
        return Err(CoreError::Other(format!(
            "{} is not a vault (no .tusk directory)",
            vault.display()
        )));
    }
    let file =
        File::create(archive).map_err(|e| CoreError::io(archive.display().to_string(), e))?;
    // If the archive lands inside the vault, never tar it into itself.
    let archive_abs = std::fs::canonicalize(archive).ok();
    let enc = GzEncoder::new(file, Compression::default());
    let mut tar = tar::Builder::new(enc);

    let mut count = 0usize;
    let mut stack = vec![vault.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let rel = match path.strip_prefix(vault) {
                Ok(r) => r.to_path_buf(),
                Err(_) => continue,
            };
            if skip(&rel) {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                if archive_abs.is_some() && std::fs::canonicalize(&path).ok() == archive_abs {
                    continue;
                }
                tar.append_path_with_name(&path, &rel)
                    .map_err(|e| CoreError::io(path.display().to_string(), e))?;
                count += 1;
            }
        }
    }
    tar.into_inner()
        .and_then(|enc| enc.finish())
        .map_err(|e| CoreError::io(archive.display().to_string(), e))?;
    Ok(count)
}

pub fn import(vault: &Path, archive: &Path) -> Result<(), CoreError> {
    // Own the vault while importing (no daemon may be running).
    let lock = crate::platform::VaultLock::acquire(vault)?;
    let file = File::open(archive).map_err(|e| CoreError::io(archive.display().to_string(), e))?;
    let mut tar = tar::Archive::new(GzDecoder::new(file));
    let mut count = 0usize;
    for entry in tar
        .entries()
        .map_err(|e| CoreError::io(archive.display().to_string(), e))?
    {
        let mut entry = entry.map_err(|e| CoreError::io(archive.display().to_string(), e))?;
        let rel: PathBuf = entry
            .path()
            .map_err(|e| CoreError::io(archive.display().to_string(), e))?
            .components()
            .filter(|c| matches!(c, Component::Normal(_)))
            .collect();
        if rel.as_os_str().is_empty() || skip(&rel) {
            continue;
        }
        let dest = vault.join(&rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CoreError::io(parent.display().to_string(), e))?;
        }
        entry
            .unpack(&dest)
            .map_err(|e| CoreError::io(dest.display().to_string(), e))?;
        count += 1;
    }
    drop(lock);
    // Rebuild the derived index from the imported files.
    let cfg = crate::config::load(vault)?;
    let host = crate::runtime::CoreHost::open(&cfg, false)?;
    host.shutdown();
    println!("imported {count} files into {}", vault.display());
    Ok(())
}
