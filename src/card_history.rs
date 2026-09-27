//! Only successful card writes become portable, per-phone history entries.

use std::fs;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::scanner::is_valid_card_hash;
use crate::wallet_backup::{backup_dir, changed_preview_path, original_preview_path};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtVersion {
    pub id: String,
    pub changed_at: u64,
    pub file: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhoneRecord {
    pub udid: String,
    pub name: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct HistoryStore {
    #[serde(default)]
    phones: Vec<PhoneRecord>,
    #[serde(default)]
    cards: Vec<CardRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardRecord {
    pub udid: String,
    pub hash: String,
    #[serde(default)]
    pub device_hash: String,
    pub name: String,
    pub changed_at: u64,
    #[serde(default)]
    pub original_backed_up: bool,
    #[serde(default)]
    pub baseline_from_prior_change: bool,
    #[serde(default)]
    pub versions: Vec<ArtVersion>,
}

fn history_path() -> PathBuf {
    crate::portable_data::directory().join("history.json")
}

pub fn load() -> Vec<CardRecord> {
    let Ok(store) = read_store() else { return Vec::new() };
    store.cards
        .into_iter()
        .filter(|record| !record.udid.is_empty() && is_valid_card_hash(&record.hash))
        .collect()
}

fn read_store() -> Result<HistoryStore> {
    let path = history_path();
    if !path.is_file() { return Ok(HistoryStore::default()); }
    let bytes = fs::read(&path).context("Could not read card history")?;
    if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'[') {
        let cards = serde_json::from_slice::<Vec<CardRecord>>(&bytes)
            .context("Legacy card history is invalid; leaving it unchanged")?;
        let phones = cards.iter().map(|card| PhoneRecord { udid: card.udid.clone(), name: String::new() })
            .fold(Vec::<PhoneRecord>::new(), |mut acc, phone| {
                if !acc.iter().any(|saved| saved.udid == phone.udid) { acc.push(phone); }
                acc
            });
        Ok(HistoryStore { phones, cards })
    } else {
        serde_json::from_slice(&bytes).context("Card history is invalid; leaving it unchanged")
    }
}

fn save_store(store: &HistoryStore) -> Result<()> {
    let path = history_path();
    let parent = path.parent().context("Card history has no parent")?;
    fs::create_dir_all(parent)?;
    let staged = path.with_extension("json.tmp");
    fs::write(&staged, serde_json::to_vec_pretty(store)?)?;
    replace_file(&staged, &path)
}

pub fn phones() -> Vec<PhoneRecord> {
    read_store().map(|store| store.phones).unwrap_or_default()
}

pub fn add_phone(udid: &str, name: &str) -> Result<()> {
    let udid = udid.trim();
    ensure!(!udid.is_empty() && udid.len() <= 128 && udid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'), "Invalid phone identifier");
    let mut store = read_store()?;
    if let Some(phone) = store.phones.iter_mut().find(|phone| phone.udid == udid) {
        phone.name = name.trim().to_string();
    } else {
        store.phones.push(PhoneRecord { udid: udid.to_string(), name: name.trim().to_string() });
    }
    save_store(&store)
}

pub fn add_card(udid: &str, hash: &str, name: &str) -> Result<()> {
    ensure!(!udid.is_empty() && is_valid_card_hash(hash), "Invalid card identity");
    let mut store = read_store()?;
    ensure!(!store.cards.iter().any(|card| card.udid == udid && card.hash == hash), "Card already exists");
    store.cards.push(CardRecord {
        udid: udid.to_string(), hash: hash.to_string(), device_hash: hash.to_string(),
        name: if name.trim().is_empty() { format!("Card {}", store.cards.len() + 1) } else { name.trim().to_string() },
        changed_at: 0, original_backed_up: false, baseline_from_prior_change: false, versions: Vec::new(),
    });
    if !store.phones.iter().any(|phone| phone.udid == udid) {
        store.phones.push(PhoneRecord { udid: udid.to_string(), name: String::new() });
    }
    save_store(&store)
}

pub fn rename_card(udid: &str, hash: &str, name: &str) -> Result<()> {
    let name = name.trim();
    ensure!(!name.is_empty() && name.chars().count() <= 80, "Card name must be 1–80 characters");
    let mut store = read_store()?;
    let record = store.cards.iter_mut().find(|card| card.udid == udid && card.hash == hash)
        .context("Card record not found")?;
    record.name = name.to_string();
    save_store(&store)
}

pub fn delete_version(udid: &str, hash: &str, version_id: &str) -> Result<()> {
    let mut store = read_store()?;
    let record = store.cards.iter_mut().find(|card| card.udid == udid && card.hash == hash)
        .context("Card record not found")?;
    let index = record.versions.iter().position(|version| version.id == version_id)
        .context("Artwork version not found")?;
    let was_latest = index + 1 == record.versions.len();
    let version = record.versions.remove(index);
    if was_latest { record.changed_at = record.versions.last().map_or(0, |version| version.changed_at); }
    let remaining_latest = if was_latest { record.versions.last().cloned() } else { None };
    let path = version_image_path(udid, hash, &version).context("Unsafe artwork filename")?;
    save_store(&store)?;
    if path.is_file() { fs::remove_file(&path).context("Version index deleted, but image could not be removed")?; }
    if was_latest {
        let changed_path = changed_preview_path(udid, hash);
        if let Some(latest) = remaining_latest {
            let source = version_image_path(udid, hash, &latest).context("Unsafe remaining artwork filename")?;
            let staged = changed_path.with_extension("png.tmp");
            fs::copy(source, &staged).context("Could not stage latest artwork preview")?;
            replace_file(&staged, &changed_path)?;
        } else if changed_path.is_file() {
            fs::remove_file(changed_path).context("Could not clear the latest artwork preview")?;
        }
    }
    Ok(())
}

pub fn delete_legacy_version(udid: &str, hash: &str) -> Result<()> {
    let mut store = read_store()?;
    let record = store.cards.iter_mut().find(|card| card.udid == udid && card.hash == hash)
        .context("Card record not found")?;
    ensure!(record.versions.is_empty() && record.changed_at > 0, "No legacy artwork version found");
    record.changed_at = 0;
    save_store(&store)?;
    let path = changed_preview_path(udid, hash);
    if path.is_file() { fs::remove_file(path).context("Legacy version index deleted, but image could not be removed")?; }
    Ok(())
}

pub fn version_image_path(udid: &str, hash: &str, version: &ArtVersion) -> Option<PathBuf> {
    let file = &version.file;
    if !file.starts_with("version-") || !file.ends_with(".png") || file.contains(['/', '\\']) {
        return None;
    }
    Some(backup_dir(udid, hash).join("history").join(file))
}

fn replace_file(staged: &Path, destination: &Path) -> Result<()> {
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    let source: Vec<u16> = staged.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = destination.as_os_str().encode_wide().chain(Some(0)).collect();
    // REPLACE_EXISTING | WRITE_THROUGH keeps the previous file until replacement succeeds.
    let ok = unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0x1 | 0x8) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error()).context("Could not atomically replace portable history file");
    }
    Ok(())
}

pub fn save_successful_change(udid: &str, hash: &str, device_hash: &str, name: &str, original_backed_up: bool, png: &[u8]) -> Result<()> {
    ensure!(!udid.is_empty() && is_valid_card_hash(hash) && is_valid_card_hash(device_hash), "Invalid card identity");
    if original_backed_up {
        ensure!(original_preview_path(udid, hash).is_some(), "First-captured artwork backup is missing");
    }
    ensure!(!png.is_empty(), "Changed artwork is empty");

    let mut store = read_store()?;
    let changed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let dir = backup_dir(udid, hash);
    let history_dir = dir.join("history");
    fs::create_dir_all(&history_dir).context("Could not create portable artwork history")?;
    let changed_path = changed_preview_path(udid, hash);
    if let Some(record) = store.cards.iter_mut().find(|r| r.udid == udid && r.hash == hash) {
        // Migrate the one-image legacy format before changed.png is replaced.
        if record.versions.is_empty() && record.changed_at > 0 && changed_path.is_file() {
            let legacy_file = format!("version-legacy-{}.png", record.changed_at);
            let legacy_path = history_dir.join(&legacy_file);
            if !legacy_path.is_file() { fs::copy(&changed_path, &legacy_path)?; }
            record.versions.push(ArtVersion { id: format!("legacy-{}", record.changed_at), changed_at: record.changed_at, file: legacy_file });
        }
        if !record.original_backed_up && original_backed_up {
            // This phone may already have been changed before the first capture.
            record.baseline_from_prior_change = record.changed_at > 0;
        }
        record.device_hash = device_hash.to_string();
        if !name.is_empty() {
            record.name = name.to_string();
        }
        record.changed_at = changed_at;
        record.original_backed_up |= original_backed_up;
    } else {
        store.cards.push(CardRecord {
            udid: udid.to_string(),
            hash: hash.to_string(),
            device_hash: device_hash.to_string(),
            name: if name.is_empty() { format!("Card {}", store.cards.len() + 1) } else { name.to_string() },
            changed_at,
            original_backed_up,
            baseline_from_prior_change: false,
            versions: Vec::new(),
        });
    }
    if !store.phones.iter().any(|phone| phone.udid == udid) {
        store.phones.push(PhoneRecord { udid: udid.to_string(), name: String::new() });
    }
    let token = crate::flasher::generate_token();
    let version_file = format!("version-{changed_at}-{token}.png");
    let version_path = history_dir.join(&version_file);
    fs::write(&version_path, png).context("Could not save new artwork version")?;
    let record = store.cards.iter_mut().find(|r| r.udid == udid && r.hash == hash).expect("record was created above");
    record.versions.push(ArtVersion { id: token, changed_at, file: version_file });
    let temporary = changed_path.with_extension("png.tmp");
    fs::write(&temporary, png).context("Could not stage changed artwork")?;
    replace_file(&temporary, &changed_path).context("Could not save changed artwork")?;
    save_store(&store).context("Could not save card history index")?;
    Ok(())
}

pub fn delete_card(udid: &str, hash: &str) -> Result<()> {
    let mut store = read_store()?;
    let before = store.cards.len();
    store.cards.retain(|card| !(card.udid == udid && card.hash == hash));
    ensure!(store.cards.len() < before, "Card record not found");
    save_store(&store)?;
    let dir = backup_dir(udid, hash);
    if dir.is_dir() { fs::remove_dir_all(&dir).context("Card index deleted, but artwork folder could not be removed")?; }
    Ok(())
}

pub fn mark_baseline_capture(udid: &str, hash: &str) -> Result<()> {
    ensure!(original_preview_path(udid, hash).is_some(), "No captured image is available");
    let mut store = read_store()?;
    let record = store.cards.iter_mut().find(|card| card.udid == udid && card.hash == hash)
        .context("Save the card record before capturing its face")?;
    if !record.original_backed_up {
        record.baseline_from_prior_change = record.changed_at > 0;
        record.original_backed_up = true;
    }
    save_store(&store)
}

pub fn delete_phone(udid: &str) -> Result<()> {
    let mut store = read_store()?;
    let hashes: Vec<String> = store.cards.iter().filter(|card| card.udid == udid).map(|card| card.hash.clone()).collect();
    store.cards.retain(|card| card.udid != udid);
    store.phones.retain(|phone| phone.udid != udid);
    save_store(&store)?;
    for hash in hashes {
        let dir = backup_dir(udid, &hash);
        if dir.is_dir() { fs::remove_dir_all(&dir).context("Phone index deleted, but a card artwork folder could not be removed")?; }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_record_round_trip() {
        let record = CardRecord {
            udid: "phone".into(),
            hash: "sqSXbSxN1AQs2S6BMjjFt8-EBnA=".into(),
            device_hash: "sqSXbSxN1AQs2S6BMjjFt8-EBnA=".into(),
            name: "Card 1".into(),
            changed_at: 1,
            original_backed_up: true,
            baseline_from_prior_change: false,
            versions: Vec::new(),
        };
        let decoded: CardRecord = serde_json::from_slice(&serde_json::to_vec(&record).unwrap()).unwrap();
        assert_eq!(decoded.udid, record.udid);
        assert_eq!(decoded.hash, record.hash);
        assert!(decoded.original_backed_up);
    }

    #[test]
    fn older_history_defaults_to_no_unverified_restore() {
        let record: CardRecord = serde_json::from_str(r#"{"udid":"phone","hash":"sqSXbSxN1AQs2S6BMjjFt8-EBnA=","name":"Card 1","changed_at":1}"#).unwrap();
        assert!(!record.original_backed_up);
    }
}
