use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::afc::AfcClient;
use crate::airlift::{
    LINK_PREFIX, RECOVERED_PREFIX, SOURCE_PREFIX, build_books_plist,
    build_streaming_zip_archive, build_streaming_zip_archive_multi, restore_books, snapshot_books,
    stage_streaming_zip,
};
use crate::airtraffic::sync_assets_via_airtraffic;
use crate::device::{ActiveDeviceSession, ConnectionMode};
use crate::wallet_backup::{capture_original_card, load_original_assets};

#[allow(dead_code)]
pub const TARGET_WALLET_ASSETS: &[&str] = &[
    "cardBackgroundCombined@3x.png",
    "cardBackgroundCombined@2x.png",
    "cardBackgroundCombined.pdf",
];

#[allow(dead_code)]
pub const CACHE_FILES: &[&str] = &[
    "FrontFace",
    "PlaceHolder",
    "Preview",
];

#[link(name = "bcrypt")]
unsafe extern "system" {
    fn BCryptGenRandom(
        hAlgorithm: *mut std::ffi::c_void,
        pbBuffer: *mut u8,
        cbBuffer: u32,
        dwFlags: u32,
    ) -> i32;
}

pub fn generate_token() -> String {
    let mut bytes = [0u8; 10];
    unsafe {
        let _ = BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            2, // BCRYPT_USE_SYSTEM_PREFERRED_RNG
        );
    }
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

pub fn write_system_file<L>(
    udid: &str,
    connection_mode: ConnectionMode,
    target_dir: &str,
    leaf_name: &str,
    payload: &[u8],
    mut log: L,
) -> Result<()>
where
    L: FnMut(&str),
{
    let token = generate_token();
    let source = format!("{}{}", SOURCE_PREFIX, token);
    let link_dest = format!("{}{}", LINK_PREFIX, token);
    let recovered = format!("{}{}", RECOVERED_PREFIX, token);

    let link_ident = format!("../../{}/p0/p1/p2/link", source);
    let payload_ident = format!("../../{}/payload", source);
    let target_dest = format!("{}/{}", link_dest, leaf_name);

    let books_identifiers = vec![link_ident.clone(), payload_ident.clone()];
    let assets_to_sync = [
        (link_ident.as_str(), link_dest.as_str()),
        (payload_ident.as_str(), target_dest.as_str()),
    ];

    log(&format!("Connecting AFC for {}...", leaf_name));
    let session = ActiveDeviceSession::open(Some(udid), connection_mode)
        .context("Failed to open device session for writing")?;
    log(&format!("Connected over {}.", session.transport.label()));
    let afc = AfcClient::new(&session).context("Failed to open AFC connection")?;

    let snapshot = snapshot_books(&afc).context("Failed to snapshot Books state before staging")?;

    let archive_data = build_streaming_zip_archive(target_dir, payload)
        .context("Failed to build streaming zip archive")?;

    let books_plist = build_books_plist(&books_identifiers)
        .context("Failed to build Books.plist")?;

    let write_res = (|| -> Result<()> {
        log(&format!("Staging payload archive ({} bytes) via MobileInstallation...", archive_data.len()));
        stage_streaming_zip(&session, &source, &archive_data)
            .context("Failed to stage streaming zip conduit")?;

        let link_obj = format!("{}/p0/p1/p2/link", source);
        let payload_obj = format!("{}/payload", source);
        if !afc.exists(&source) || !afc.exists(&link_obj) || !afc.exists(&payload_obj) {
            bail!("StreamingZip completed but staging link/payload object missing on AFC");
        }

        afc.make_directory_recursive("Books/Sync")?;
        afc.write_file("Books/Sync/Books.plist", &books_plist)?;
        if !afc.exists("Books/Sync/Books.plist") {
            bail!("Failed to stage Books/Sync/Books.plist");
        }

        log(&format!("Synchronizing {} with AirTraffic host daemon...", leaf_name));
        sync_assets_via_airtraffic(udid, session.transport, &assets_to_sync, &mut log)
            .context("AirTraffic sync failed")?;

        Ok(())
    })();

    let _ = afc.remove_path(&link_dest);
    let _ = afc.remove_path(&recovered);
    let _ = afc.remove_tree(&source);
    sleep(Duration::from_millis(800));

    let restore_res = restore_books(&afc, &snapshot);

    write_res?;
    restore_res.context("Failed to restore Books state during cleanup")?;
    log(&format!("Successfully written: {}", leaf_name));

    Ok(())
}

pub fn write_system_files_batch<L>(
    udid: &str,
    connection_mode: ConnectionMode,
    target_dir: &str,
    items: &[(&str, &[u8])],
    mut log: L,
) -> Result<()>
where
    L: FnMut(&str),
{
    if items.is_empty() {
        return Ok(());
    }
    if items.len() == 1 {
        return write_system_file(
            udid,
            connection_mode,
            target_dir,
            items[0].0,
            items[0].1,
            log,
        );
    }

    log(&format!("Packaging atomic batch of {} file(s) for {}...", items.len(), target_dir));

    let token = generate_token();
    let source = format!("{}{}", SOURCE_PREFIX, token);
    let link_dest = format!("{}{}", LINK_PREFIX, token);
    let recovered = format!("{}{}", RECOVERED_PREFIX, token);

    let link_ident = format!("../../{}/p0/p1/p2/link", source);
    let mut books_identifiers = Vec::with_capacity(items.len() + 1);
    books_identifiers.push(link_ident.clone());

    let mut assets_to_sync: Vec<(String, String)> = Vec::with_capacity(items.len() + 1);
    assets_to_sync.push((link_ident, link_dest.clone()));

    for (idx, (leaf, _)) in items.iter().enumerate() {
        let payload_ident = format!("../../{}/payload_{}", source, idx);
        let target_dest = format!("{}/{}", link_dest, leaf);
        books_identifiers.push(payload_ident.clone());
        assets_to_sync.push((payload_ident, target_dest));
    }

    log(&format!("Connecting AFC for batch of {} assets...", items.len()));
    let session = ActiveDeviceSession::open(Some(udid), connection_mode)
        .context("Failed to open device session for writing")?;
    log(&format!("Connected over {}.", session.transport.label()));
    let afc = AfcClient::new(&session).context("Failed to open AFC connection")?;

    let snapshot = snapshot_books(&afc).context("Failed to snapshot Books state before staging")?;

    let archive_data = build_streaming_zip_archive_multi(target_dir, items)
        .context("Failed to build multi-payload streaming zip archive")?;

    let books_plist = build_books_plist(&books_identifiers)
        .context("Failed to build Books.plist for batch")?;

    let write_res = (|| -> Result<()> {
        log(&format!("Staging multi-payload archive ({} bytes, {} files) via MobileInstallation...", archive_data.len(), items.len()));
        stage_streaming_zip(&session, &source, &archive_data)
            .context("Failed to stage streaming zip conduit")?;

        let link_obj = format!("{}/p0/p1/p2/link", source);
        let payload_obj = format!("{}/payload_0", source);
        let fallback_obj = format!("{}/payload", source);
        if !afc.exists(&source) || !afc.exists(&link_obj) || (!afc.exists(&payload_obj) && !afc.exists(&fallback_obj)) {
            bail!("StreamingZip completed but staging link/payload object missing on AFC");
        }

        afc.make_directory_recursive("Books/Sync")?;
        afc.write_file("Books/Sync/Books.plist", &books_plist)?;
        if !afc.exists("Books/Sync/Books.plist") {
            bail!("Failed to stage Books/Sync/Books.plist");
        }

        log(&format!("Synchronizing batch ({} items) with AirTraffic host daemon in single session...", items.len()));
        let assets_refs: Vec<(&str, &str)> = assets_to_sync.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        sync_assets_via_airtraffic(udid, session.transport, &assets_refs, &mut log)
            .context("AirTraffic batch sync failed")?;

        Ok(())
    })();

    let _ = afc.remove_path(&link_dest);
    let _ = afc.remove_path(&recovered);
    let _ = afc.remove_tree(&source);
    sleep(Duration::from_millis(800));

    let restore_res = restore_books(&afc, &snapshot);

    write_res?;
    restore_res.context("Failed to restore Books state during cleanup")?;
    log(&format!("Batch injection of {} file(s) completed successfully!", items.len()));

    Ok(())
}

pub struct FlashOutcome {
    pub device_hash: String,
    pub original_backed_up: bool,
}

pub fn flash_wallet_skin<F, L>(
    udid: &str,
    connection_mode: ConnectionMode,
    card_hash: &str,
    skin_png: &[u8],
    skin_pdf: &[u8],
    mut progress: F,
    mut log: L,
) -> Result<FlashOutcome>
where
    F: FnMut(usize, usize, &str),
    L: FnMut(&str),
{
    anyhow::ensure!(
        crate::scanner::is_valid_card_hash(card_hash),
        "Invalid Wallet card path identifier"
    );

    log(&format!("Target Card Hash: {}", card_hash));
    log(&format!(
        "Skin payload size: {} bytes PNG, {} bytes PDF",
        skin_png.len(),
        skin_pdf.len()
    ));
    let resolved_hash = match capture_original_card(udid, connection_mode, card_hash, &mut log) {
        Ok(Some(hash)) => hash,
        Ok(None) => card_hash.to_string(),
        Err(err) => return Err(err).context("Failed to inspect the original Wallet card face"),
    };
    let original_assets = crate::wallet_backup::original_preview_path(udid, card_hash)
        .filter(|path| image::open(path).is_ok())
        .and_then(|_| load_original_assets(udid, card_hash).ok());
    let original_backed_up = original_assets.is_some();
    if !original_backed_up {
        log("Original card artwork is unavailable. Writing the new skin without a restore backup; Restore Original will be disabled for this card.");
    }
    let pkpass_dir = format!("/var/mobile/Library/Passes/Cards/{}.pkpass", resolved_hash);

    let total_steps = 3;
    progress(1, total_steps, "Writing card artwork assets...");
    let asset_names: Vec<&str> = match original_assets.as_ref() {
        Some(assets) => assets.iter().map(|(name, _)| name.as_str()).collect(),
        None => TARGET_WALLET_ASSETS.to_vec(),
    };
    log(&format!("[1/3] Writing {} card artwork asset(s)...", asset_names.len()));
    let card_assets: Vec<(&str, &[u8])> = asset_names.iter().map(|name| {
        (*name, if name.ends_with(".pdf") { skin_pdf } else { skin_png })
    }).collect();

    if let Err(err) = write_system_files_batch(
        udid,
        connection_mode,
        &pkpass_dir,
        &card_assets,
        &mut log,
    ) {
        log(&format!("Notice: Batch write failed ({}), trying individual asset writes...", err));
        for (asset, data) in &card_assets {
            write_system_file(udid, connection_mode, &pkpass_dir, asset, data, &mut log)
                .context(format!("Failed to write card asset {}", asset))?;
        }
    }

    invalidate_wallet_caches(
        udid,
        connection_mode,
        resolved_hash.as_str(),
        &mut progress,
        &mut log,
    );

    progress(total_steps, total_steps, "Card skin updated successfully!");
    log("Card skin write finished! Close and reopen Wallet on iPhone to view.");
    Ok(FlashOutcome { device_hash: resolved_hash, original_backed_up })
}

pub fn restore_wallet_original<F, L>(
    udid: &str,
    connection_mode: ConnectionMode,
    card_hash: &str,
    device_hash: &str,
    mut progress: F,
    mut log: L,
) -> Result<()>
where
    F: FnMut(usize, usize, &str),
    L: FnMut(&str),
{
    anyhow::ensure!(
        crate::scanner::is_valid_card_hash(card_hash) && crate::scanner::is_valid_card_hash(device_hash),
        "Invalid Wallet card path identifier"
    );
    let original_assets = load_original_assets(udid, card_hash)
        .context("Could not load the original Wallet card face backup")?;
    let asset_refs: Vec<(&str, &[u8])> = original_assets
        .iter()
        .map(|(asset, data)| (asset.as_str(), data.as_slice()))
        .collect();

    log(&format!(
        "Restoring {} original card artwork asset(s) for hash {}...",
        asset_refs.len(),
        card_hash
    ));
    progress(1, 3, "Restoring original card artwork...");

    let pkpass_dir = format!("/var/mobile/Library/Passes/Cards/{}.pkpass", device_hash);
    if let Err(err) = write_system_files_batch(
        udid,
        connection_mode,
        &pkpass_dir,
        &asset_refs,
        &mut log,
    ) {
        log(&format!(
            "Notice: Restore batch write failed ({}), trying individual asset writes...",
            err
        ));
        for (asset, data) in &original_assets {
            write_system_file(
                udid,
                connection_mode,
                &pkpass_dir,
                asset,
                data,
                &mut log,
            )
            .context(format!("Failed to restore original card asset {}", asset))?;
        }
    }

    invalidate_wallet_caches(
        udid,
        connection_mode,
        device_hash,
        &mut progress,
        &mut log,
    );

    progress(3, 3, "Original card face restored successfully!");
    log("Original card face restored. Close and reopen Wallet on iPhone to view it.");
    Ok(())
}

fn invalidate_wallet_caches<F, L>(
    udid: &str,
    connection_mode: ConnectionMode,
    card_hash: &str,
    progress: &mut F,
    log: &mut L,
) where
    F: FnMut(usize, usize, &str),
    L: FnMut(&str),
{
    let cache_leaves: [(&str, &[u8]); 3] = [
        ("FrontFace", b"corrupted"),
        ("PlaceHolder", b"corrupted"),
        ("Preview", b"corrupted"),
    ];

    for (c_idx, ext) in [".cache", ".pkcache"].iter().enumerate() {
        let step = 2 + c_idx;
        let cache_dir = format!("/var/mobile/Library/Passes/Cards/{}{}", card_hash, ext);
        progress(step, 3, &format!("Clearing {} cache...", ext));
        log(&format!(
            "[{}/3] Invalidating cache leaves in {}...",
            step, cache_dir
        ));

        if write_system_files_batch(
            udid,
            connection_mode,
            &cache_dir,
            &cache_leaves,
            &mut *log,
        )
        .is_err()
        {
            for (leaf, data) in &cache_leaves {
                let _ = write_system_file(
                    udid,
                    connection_mode,
                    &cache_dir,
                    leaf,
                    data,
                    &mut *log,
                );
            }
        }
    }
}
