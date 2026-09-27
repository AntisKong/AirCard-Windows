use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::airlift_read::read_system_file;
use crate::device::ConnectionMode;

pub const CARD_ARTWORK_ASSETS: [&str; 3] = [
    "cardBackgroundCombined@3x.png",
    "cardBackgroundCombined@2x.png",
    "cardBackgroundCombined.pdf",
];

fn backup_root() -> PathBuf {
    crate::portable_data::directory().join("wallet-backups")
}

pub fn backup_dir(udid: &str, card_hash: &str) -> PathBuf {
    let normalized_hash = card_hash.trim_end_matches('=').replace('-', "+").replace('_', "/");
    backup_root().join(format!(
        "{}-{}",
        safe_component(udid),
        stable_hash(&normalized_hash)
    ))
}

pub fn original_preview_path(udid: &str, card_hash: &str) -> Option<PathBuf> {
    let dir = backup_dir(udid, card_hash);
    ["cardBackgroundCombined@3x.png", "cardBackgroundCombined@2x.png"]
        .into_iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
}

pub fn changed_preview_path(udid: &str, card_hash: &str) -> PathBuf {
    backup_dir(udid, card_hash).join("changed.png")
}

fn safe_component(value: &str) -> String {
    let component: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if component.is_empty() {
        "unknown".to_string()
    } else {
        component
    }
}

fn stable_hash(value: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

pub fn backup_exists(udid: &str, card_hash: &str) -> bool {
    let dir = backup_dir(udid, card_hash);
    CARD_ARTWORK_ASSETS
        .iter()
        .any(|asset| dir.join(asset).is_file())
}

fn card_hash_candidates(card_hash: &str) -> Vec<String> {
    let trimmed = card_hash.trim_end_matches('=');
    let mut candidates = vec![card_hash.to_string(), trimmed.to_string()];
    if !trimmed.is_empty() {
        candidates.push(format!("{trimmed}="));
        candidates.push(format!("{trimmed}=="));
    }
    candidates.dedup();
    candidates
}

pub fn capture_original_card<L>(
    udid: &str,
    connection_mode: ConnectionMode,
    card_hash: &str,
    mut log: L,
) -> Result<Option<String>>
where
    L: FnMut(&str),
{
    let dir = backup_dir(udid, card_hash);
    log("Checking for a locked first-captured Wallet artwork backup...");
    if backup_exists(udid, card_hash) {
        log("First-captured card artwork already exists; it will never be overwritten.");
        return Ok(Some(card_hash.to_string()));
    }
    // Standard AFC exposes /var/mobile/Media, not Wallet's private directory.
    // The tested AirTraffic export moves each file into Media and writes it back
    // before this function can return any bytes.
    for resolved_hash in card_hash_candidates(card_hash) {
        let pkpass_dir = format!("/var/mobile/Library/Passes/Cards/{resolved_hash}.pkpass");
        let mut captured = 0;
        for asset in ["cardBackgroundCombined@2x.png", "cardBackgroundCombined@3x.png", "cardBackgroundCombined.pdf"] {
            let backup_path = dir.join(asset);
            if backup_path.is_file() {
                captured += 1;
                continue;
            }
            log(&format!("Trying AirTraffic artwork read: {asset}"));
            let Some(data) = read_system_file(udid, connection_mode, &pkpass_dir, asset, &mut log)
                .with_context(|| format!("Could not safely export and restore {asset}"))? else {
                continue;
            };
            if asset.ends_with(".png") {
                image::load_from_memory(&data)
                    .with_context(|| format!("Captured {asset} is not a valid image"))?;
            } else if !data.starts_with(b"%PDF") {
                bail!("Captured card PDF is invalid; no artwork backup marked complete");
            }
            write_backup_file(&backup_path, &data)
                .with_context(|| format!("Could not save the first captured {asset}"))?;
            log(&format!("Locked first-captured artwork: {asset} ({} bytes)", data.len()));
            captured += 1;
        }
        if captured > 0 {
            return Ok(Some(resolved_hash));
        }
    }
    log("No readable card artwork was found; no baseline backup was created.");
    Ok(None)
}

pub fn load_original_assets(
    udid: &str,
    card_hash: &str,
) -> Result<Vec<(String, Vec<u8>)>> {
    let dir = backup_dir(udid, card_hash);
    let mut assets = Vec::new();

    for asset in CARD_ARTWORK_ASSETS {
        let path = dir.join(asset);
        if !path.is_file() {
            continue;
        }
        let data = fs::read(&path)
            .with_context(|| format!("Could not read backup file {}", path.display()))?;
        assets.push((asset.to_string(), data));
    }

    if assets.is_empty() {
        bail!("Original card face backup not found for card hash {}", card_hash);
    }

    Ok(assets)
}

fn write_backup_file(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, data)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temporary, path)?;
    Ok(())
}
