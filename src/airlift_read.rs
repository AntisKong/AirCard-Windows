//! Export a private iOS file into AFC Media, read it, and immediately write it back.
//! AirTraffic MOVE semantics make preserving Media/recovered on failure essential.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail, ensure};

use crate::afc::AfcClient;
use crate::airlift::{
    LINK_PREFIX, RECOVERED_PREFIX, SOURCE_PREFIX, BooksSnapshot, TRACKED_BOOKS_FILES,
    build_books_plist, build_streaming_zip_archive, restore_books, snapshot_books,
    stage_streaming_zip,
};
use crate::airtraffic::sync_assets_via_airtraffic;
use crate::device::{ActiveDeviceSession, ConnectionMode};
use crate::flasher::{generate_token, write_system_file_in_session};

const AIRLOCK_ROOT: &str = "/var/mobile/Media/Airlock/Book";
const MAX_READ_SIZE: usize = 32 * 1024 * 1024;

fn relative_identifier(target_dir: &str, leaf: &str) -> Result<String> {
    ensure!(target_dir.starts_with('/'), "Device path must be absolute");
    ensure!(!leaf.is_empty() && leaf != "." && leaf != ".." && !leaf.contains(['/', '\\', '\0']), "Invalid device file name");
    let base: Vec<&str> = AIRLOCK_ROOT.split('/').filter(|part| !part.is_empty()).collect();
    let mut target: Vec<&str> = target_dir.split('/').filter(|part| !part.is_empty()).collect();
    ensure!(target.iter().all(|part| *part != "." && *part != ".."), "Invalid device path component");
    target.push(leaf);
    let shared = base.iter().zip(&target).take_while(|(a, b)| a == b).count();
    let mut parts = vec![".."; base.len() - shared];
    parts.extend_from_slice(&target[shared..]);
    Ok(parts.join("/"))
}

fn recovery_root() -> PathBuf {
    crate::portable_data::directory().join("recovery")
}

fn persist_books_snapshot(folder: &Path, snapshot: &BooksSnapshot) -> Result<()> {
    fs::create_dir_all(folder)?;
    let mut manifest = String::new();
    for (index, path) in TRACKED_BOOKS_FILES.iter().enumerate() {
        let data = snapshot.files.get(*path).and_then(Option::as_ref);
        manifest.push_str(&format!("{index}\t{}\t{path}\n", if data.is_some() { "present" } else { "absent" }));
        if let Some(data) = data {
            fs::write(folder.join(format!("books-{index}.bin")), data)?;
        }
    }
    fs::write(folder.join("books-manifest.txt"), manifest)?;
    Ok(())
}

fn clean_stage(afc: &AfcClient, snapshot: &BooksSnapshot, source: &str, link: &str) -> Result<()> {
    // Restore Books even when deleting staging fails; both operations are independently useful.
    let link_result = afc.remove_path(link);
    let source_result = afc.remove_tree(source);
    let books_result = restore_books(afc, snapshot);
    link_result?;
    source_result?;
    books_result?;
    Ok(())
}

/// Returns None when the named asset is not available. A returned Some means
/// the bytes were saved locally, written back to the device and cleanup reported success.
pub fn read_system_file<L>(
    udid: &str,
    mode: ConnectionMode,
    target_dir: &str,
    leaf: &str,
    log: L,
) -> Result<Option<Vec<u8>>>
where
    L: FnMut(&str),
{
    read_system_file_with_preview(udid, mode, target_dir, leaf, log, |_| {}, None)
}

pub fn read_system_file_with_preview<L, P>(
    udid: &str,
    mode: ConnectionMode,
    target_dir: &str,
    leaf: &str,
    mut log: L,
    mut preview: P,
    cancel: Option<&AtomicBool>,
) -> Result<Option<Vec<u8>>>
where
    L: FnMut(&str),
    P: FnMut(&[u8]),
{
    let started = std::time::Instant::now();
    let cancelled = || cancel.is_some_and(|flag| flag.load(Ordering::Relaxed));
    if cancelled() { return Ok(None); }
    let target_ident = relative_identifier(target_dir, leaf)?;
    let token = generate_token();
    let source = format!("{SOURCE_PREFIX}{token}");
    let link = format!("{LINK_PREFIX}{token}");
    let recovered = format!("{RECOVERED_PREFIX}{token}");
    let link_ident = format!("../../{source}/p0/p1/p2/link");
    let journal = recovery_root().join(&token);

    let session = ActiveDeviceSession::open(Some(udid), mode)
        .context("Cannot open iPhone session for artwork read")?;
    let afc = AfcClient::new(&session).context("Cannot open AFC for artwork read")?;
    ensure!(!afc.exists(&recovered), "Recovery name already exists on iPhone");
    let snapshot = snapshot_books(&afc).context("Cannot snapshot Books metadata")?;
    persist_books_snapshot(&journal, &snapshot)
        .context("Cannot save recoverable Books snapshot beside EXE")?;
    fs::write(journal.join("target.txt"), format!("{target_dir}/{leaf}\nMedia/{recovered}\n"))?;

    let archive = build_streaming_zip_archive(target_dir, b"aircard-read-stage")?;
    let plist = build_books_plist(&[link_ident.clone(), target_ident.clone()])?;
    if let Err(err) = stage_streaming_zip(&session, &source, &archive) {
        if clean_stage(&afc, &snapshot, &source, &link).is_ok() { let _ = fs::remove_dir_all(&journal); }
        return Err(err).context("Could not stage artwork read; Wallet file was not moved");
    }
    if !afc.exists(&format!("{source}/p0/p1/p2/link")) {
        if clean_stage(&afc, &snapshot, &source, &link).is_ok() { let _ = fs::remove_dir_all(&journal); }
        bail!("Read staging link missing; Wallet file was not moved");
    }
    if let Err(err) = afc.make_directory_recursive("Books/Sync")
        .and_then(|()| afc.write_file("Books/Sync/Books.plist", &plist)) {
        if clean_stage(&afc, &snapshot, &source, &link).is_ok() { let _ = fs::remove_dir_all(&journal); }
        return Err(err).context("Cannot stage Books metadata; Wallet file was not moved");
    }
    if cancelled() {
        clean_stage(&afc, &snapshot, &source, &link)
            .context("Could not clean cancelled artwork staging; Wallet file was not moved")?;
        fs::remove_dir_all(&journal).context("Cannot remove cancelled recovery snapshot")?;
        log("Preview read cancelled before exporting the Wallet file.");
        return Ok(None);
    }
    let sync_result = sync_assets_via_airtraffic(
        udid, session.transport,
        &[(link_ident.as_str(), link.as_str()), (target_ident.as_str(), recovered.as_str())],
        &mut log,
    );
    if let Err(err) = &sync_result {
        log(&format!("Artwork export sync reported: {err:#}"));
    }

    if !afc.exists(&recovered) {
        let cleanup = clean_stage(&afc, &snapshot, &source, &link);
        if cleanup.is_ok() {
            let _ = fs::remove_dir_all(&journal);
        }
        cleanup.context("Books cleanup failed after an unavailable asset")?;
        if sync_result.is_err() {
            log(&format!("No Media recovery file was produced for {leaf}; treating this asset as unavailable."));
        }
        return Ok(None);
    }

    log(&format!("Artwork export ready after {:.2}s; reading file bytes...", started.elapsed().as_secs_f32()));

    // From this point the source may have been moved off its original path.
    // Never delete Media/recovered until write-back has reported success.
    let capture = (|| -> Result<Vec<u8>> {
        let size = afc.file_size(&recovered)
            .context(format!("Recovery file Media/{recovered} has no size"))?;
        ensure!(size > 0 && size <= MAX_READ_SIZE, "Recovery file has unsafe size {size}");
        let bytes = afc.read_file(&recovered)
            .with_context(|| format!("Cannot read Media/{recovered}"))?;
        fs::write(journal.join("recovered.bin"), &bytes)
            .context("Cannot save local recovery copy")?;
        Ok(bytes)
    })();
    let bytes = match capture {
        Ok(bytes) => bytes,
        Err(err) => {
            let _ = restore_books(&afc, &snapshot);
            return Err(err).with_context(|| format!(
                "Device file may be in Media/{recovered}; do not flash until it is recovered. Snapshot: {}",
                journal.display()
            ));
        }
    };
    log(&format!("Artwork bytes received after {:.2}s; restoring device file...", started.elapsed().as_secs_f32()));
    if !cancelled() && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| preview(&bytes))).is_err() {
        log("Preview callback failed; continuing mandatory device write-back.");
    }
    if let Err(err) = write_system_file_in_session(&session, &afc, target_dir, leaf, &bytes, &mut log) {
        let _ = restore_books(&afc, &snapshot);
        return Err(err).with_context(|| format!(
            "Write-back uncertain. Do not flash this card. Recovery copies: iPhone Media/{recovered} and {}",
            journal.join("recovered.bin").display()
        ));
    }
    afc.remove_path(&recovered)
        .with_context(|| format!("Restored source, but Media/{recovered} cleanup failed"))?;
    clean_stage(&afc, &snapshot, &source, &link)
        .context("Original file was written back, but read staging cleanup failed")?;
    fs::remove_dir_all(&journal).context("Cannot remove successful temporary recovery snapshot")?;
    log(&format!("Read and restored {leaf}: {} bytes in {:.2}s", bytes.len(), started.elapsed().as_secs_f32()));
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_preview_does_not_connect_or_create_recovery_files() {
        let cancelled = AtomicBool::new(true);
        let result = read_system_file_with_preview("unused", ConnectionMode::Auto,
            "/var/tmp", "unused.png", |_| panic!("No operation should start"),
            |_| panic!("No preview should be produced"), Some(&cancelled)).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn relative_identifier_targets_correct_ios_path() {
        assert_eq!(relative_identifier("/var/tmp", "probe.bin").unwrap(), "../../../../tmp/probe.bin");
        assert_eq!(relative_identifier("/var/mobile/Library/Passes/Cards/hash.pkpass", "cardBackgroundCombined@2x.png").unwrap(),
            "../../../Library/Passes/Cards/hash.pkpass/cardBackgroundCombined@2x.png");
        assert!(relative_identifier("/var/tmp", "../escape").is_err());
    }
}
