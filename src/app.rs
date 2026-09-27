use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::thread;

use eframe::egui;
use image::DynamicImage;

use crate::apple;
use crate::card_history::{self, CardRecord, PhoneRecord};
use crate::device::{ConnectionMode, DeviceInfo, DeviceTransport, list_connected_devices};
use crate::flasher::{flash_wallet_skin, restore_wallet_original};
use crate::image_skin::{PreparedSkin, crop_uv_for_card, png_to_pdf};
use crate::i18n::Language;
use crate::scanner::scan_syslog_for_cards;
use crate::wallet_backup::{backup_exists, capture_original_card, changed_preview_path, original_preview_path};

enum BackgroundTaskMessage {
    Progress { step: usize, total: usize, message: String },
    Log(String),
    CardFound { hash: String, name: String },
    Done(Result<String, String>),
}

#[cfg(windows)]
fn current_timestamp() -> String {
    #[repr(C)]
    struct SystemTime {
        w_year: u16,
        w_month: u16,
        w_day_of_week: u16,
        w_day: u16,
        w_hour: u16,
        w_minute: u16,
        w_second: u16,
        w_milliseconds: u16,
    }
    unsafe extern "system" {
        fn GetLocalTime(lpSystemTime: *mut SystemTime);
    }
    let mut st = std::mem::MaybeUninit::<SystemTime>::uninit();
    unsafe {
        GetLocalTime(st.as_mut_ptr());
        let st = st.assume_init();
        format!(
            "{:02}:{:02}:{:02}.{:03}",
            st.w_hour, st.w_minute, st.w_second, st.w_milliseconds
        )
    }
}

#[cfg(not(windows))]
fn current_timestamp() -> String {
    "00:00:00.000".to_string()
}

pub struct AirCardApp {
    language: Language,
    apple_ready: bool,

    // Device management
    devices: Vec<DeviceInfo>,
    selected_udid: Option<String>,
    connection_mode: ConnectionMode,
    page: Page,
    phone_records: Vec<PhoneRecord>,
    new_phone_udid: String,
    new_phone_name: String,
    new_card_hash: String,
    new_card_name: String,
    adding_phone: bool,
    adding_card: bool,
    pending_delete_card: Option<(String, String)>,
    pending_delete_version: Option<(String, String, SelectedArtwork)>,
    pending_delete_phone: Option<String>,
    selected_artwork: Option<SelectedArtwork>,
    renaming_card: Option<(String, String)>,
    rename_text: String,
    auto_scan_for: Option<String>,
    resume_auto_scan_after_task: bool,
    history_textures: HashMap<PathBuf, egui::TextureHandle>,

    // Wallet tab
    card_hash: String,
    card_records: Vec<CardRecord>,
    scanned_name: String,
    record_preview_key: String,
    original_texture: Option<egui::TextureHandle>,
    changed_texture: Option<egui::TextureHandle>,
    source_path: Option<PathBuf>,
    source_image: Option<DynamicImage>,
    source_texture: Option<egui::TextureHandle>,
    crop_focus: [f32; 2],
    crop_dirty: bool,
    skin: Option<PreparedSkin>,
    scanning_syslog: bool,
    scan_stop_flag: Option<Arc<AtomicBool>>,

    // Worker thread & progress
    is_busy: bool,
    progress_step: usize,
    progress_total: usize,
    progress_msg: String,
    status_msg: String,
    task_rx: Option<Receiver<BackgroundTaskMessage>>,
    logs: Vec<String>,
    show_logs_window: bool,
}

impl AirCardApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_custom_fonts(&cc.egui_ctx);
        setup_custom_theme(&cc.egui_ctx);

        let (apple_ready, apple_status) = match apple::verify_support() {
            Ok(msg) => (true, msg),
            Err(err) => (false, err.to_string()),
        };

        let language = Language::load();
        let card_records = card_history::load();
        let phone_records = card_history::phones();
        let archive_phone = card_records.first().map(|record| record.udid.clone())
            .or_else(|| phone_records.first().map(|phone| phone.udid.clone()));
        let mut app = Self {
            language,
            apple_ready,

            devices: Vec::new(),
            selected_udid: archive_phone,
            connection_mode: ConnectionMode::Auto,
            page: Page::Cards,
            phone_records,
            new_phone_udid: String::new(),
            new_phone_name: String::new(),
            new_card_hash: String::new(),
            new_card_name: String::new(),
            adding_phone: false,
            adding_card: false,
            pending_delete_card: None,
            pending_delete_version: None,
            pending_delete_phone: None,
            selected_artwork: None,
            renaming_card: None,
            rename_text: String::new(),
            auto_scan_for: None,
            resume_auto_scan_after_task: false,
            history_textures: HashMap::new(),

            card_hash: String::new(),
            card_records,
            scanned_name: String::new(),
            record_preview_key: String::new(),
            original_texture: None,
            changed_texture: None,
            source_path: None,
            source_image: None,
            source_texture: None,
            crop_focus: [0.5, 0.5],
            crop_dirty: false,
            skin: None,
            scanning_syslog: false,
            scan_stop_flag: None,

            is_busy: false,
            progress_step: 0,
            progress_total: 0,
            progress_msg: String::new(),
            status_msg: language
                .text("Ready. Connect iPhone via USB or paired WiFi and unlock it.")
                .to_string(),
            task_rx: None,
            logs: Vec::new(),
            show_logs_window: false,
        };

        app.add_log("AirCard initialized");
        app.add_log(format!("Apple Support Runtime: {apple_status}"));
        app.add_log(format!("Loaded {} changed card(s) from portable history", app.card_records.len()));

        if app.apple_ready {
            app.refresh_devices();
        }

        app
    }

    fn add_log(&mut self, text: impl AsRef<str>) {
        let ts = current_timestamp();
        self.logs.push(format!("[{}] {}", ts, text.as_ref()));
        if self.logs.len() > 1000 {
            self.logs.remove(0);
        }
    }

    fn refresh_record_previews(&mut self, ctx: &egui::Context) {
        let key = format!("{}:{}", self.selected_udid.as_deref().unwrap_or(""), self.card_hash.trim());
        if key == self.record_preview_key {
            return;
        }
        self.record_preview_key = key;
        self.original_texture = None;
        self.changed_texture = None;
        let Some(udid) = self.selected_udid.as_deref() else { return };
        let hash = self.card_hash.trim();
        let Some(record) = self.card_records.iter().find(|r| r.udid == udid && r.hash == hash) else {
            return;
        };
        if record.original_backed_up {
            self.original_texture = original_preview_path(udid, hash)
                .as_deref()
                .and_then(|path| load_record_texture(ctx, "original-card-record", path));
        }
        self.changed_texture = load_record_texture(ctx, "changed-card-record", &changed_preview_path(udid, hash));
    }

    fn refresh_devices(&mut self) {
        self.add_log("Scanning for connected iOS devices via usbmuxd...");
        match list_connected_devices() {
            Ok(devs) => {
                self.devices = devs;
                let selection_still_exists = self.selected_udid.as_ref().is_some_and(|selected| {
                    self.devices
                        .iter()
                        .any(|device| device.udid.eq_ignore_ascii_case(selected))
                });
                if !selection_still_exists && !self.devices.is_empty() {
                    self.selected_udid = Some(self.devices[0].udid.clone());
                }
                if self.devices.is_empty() {
                    if self.selected_udid.is_none() {
                        self.selected_udid = self.card_records.first().map(|record| record.udid.clone());
                    }
                    self.add_log("No devices detected. Connect by USB, or enable WiFi sync after initial USB pairing.");
                    self.status_msg = self
                        .language
                        .text("No iPhone connected via USB or paired WiFi.")
                        .to_string();
                } else {
                    let dev_logs: Vec<String> = self.devices.iter().enumerate().map(|(i, d)| {
                        format!("Device #{}: {} - UDID: {}", i + 1, d, d.udid)
                    }).collect();
                    for line in dev_logs {
                        self.add_log(line);
                    }
                    self.status_msg = format!(
                        "{} {} {} {}",
                        self.language.text("Found"),
                        self.devices.len(),
                        self.language.text("connected device(s); transport mode:"),
                        self.language.text(self.connection_mode.label())
                    );
                }
            }
            Err(err) => {
                self.add_log(format!("Device scan error: {}", err));
                self.status_msg = format!(
                    "{} {}",
                    self.language.text("Could not enumerate devices:"),
                    err
                );
            }
        }
    }

    fn selected_transport_available(&self) -> bool {
        self.selected_udid.as_ref().is_some_and(|selected| {
            self.devices.iter().any(|device| {
                if !device.udid.eq_ignore_ascii_case(selected) {
                    return false;
                }
                if self.connection_mode == ConnectionMode::Wifi {
                    return device.has_transport(DeviceTransport::Wifi)
                        && !device.has_transport(DeviceTransport::Usb);
                }
                device.supports(self.connection_mode)
            })
        })
    }

    fn validate_selected_transport(&mut self, operation: &str) -> bool {
        if self.selected_udid.is_none() {
            self.add_log(format!("{} failed: No connected iPhone selected.", operation));
            self.status_msg = self.language.text("Please select a connected iPhone.").to_string();
            return false;
        }
        if !self.selected_transport_available() {
            let wifi_has_usb_attached = self.connection_mode == ConnectionMode::Wifi
                && self.selected_udid.as_ref().is_some_and(|selected| {
                    self.devices.iter().any(|device| {
                        device.udid.eq_ignore_ascii_case(selected)
                            && device.has_transport(DeviceTransport::Wifi)
                            && device.has_transport(DeviceTransport::Usb)
                    })
                });
            self.add_log(format!(
                "{} failed: Selected device is unavailable in {} mode.",
                operation,
                self.connection_mode.label()
            ));
            self.status_msg = if wifi_has_usb_attached {
                self.language
                    .text("Disconnect the USB cable and refresh to guarantee the full AirTraffic path uses WiFi.")
                    .to_string()
            } else {
                format!(
                    "{} {} {}",
                    self.language.text("Selected iPhone has no"),
                    self.language.text(self.connection_mode.label()),
                    self.language.text("connection. Refresh devices or change transport mode.")
                )
            };
            return false;
        }
        true
    }

    fn select_skin(&mut self, ctx: &egui::Context) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Images", &["png", "jpg", "jpeg", "webp"])
            .pick_file()
        else {
            return;
        };

        self.add_log(format!("Opening skin image: {}", path.display()));
        match image::open(&path) {
            Ok(source_image) => {
                let source_width = source_image.width();
                let source_height = source_image.height();
                let skin = match PreparedSkin::from_image_with_focus(
                    source_image.clone(),
                    0.5,
                    0.5,
                ) {
                    Ok(skin) => skin,
                    Err(error) => {
                        self.add_log(format!("Image preparation failed: {error:#}"));
                        self.status_msg = format!("Could not prepare image: {error:#}");
                        return;
                    }
                };
                let source_rgba = source_image.thumbnail(2048, 2048).to_rgba8();
                let source_preview = egui::ColorImage::from_rgba_unmultiplied(
                    [source_rgba.width() as usize, source_rgba.height() as usize],
                    source_rgba.as_raw(),
                );

                self.add_log(format!(
                    "Skin processed: source {}x{} resampled to 1536x969 PNG ({:.1} KB)",
                    skin.source_width,
                    skin.source_height,
                    skin.png.len() as f32 / 1024.0,
                ));
                self.source_texture = Some(ctx.load_texture(
                    "card-skin-source-preview",
                    source_preview,
                    egui::TextureOptions::LINEAR,
                ));
                self.status_msg = format!(
                    "Prepared {} ({}x{} -> 1536x969 PNG, {:.1} KB)",
                    path.file_name().and_then(|n| n.to_str()).unwrap_or("image"),
                    source_width,
                    source_height,
                    skin.png.len() as f32 / 1024.0,
                );
                self.source_path = Some(path);
                self.source_image = Some(source_image);
                self.crop_focus = [0.5, 0.5];
                self.crop_dirty = false;
                self.skin = Some(skin);
            }
            Err(error) => {
                self.add_log(format!("Image decode failed: {error:#}"));
                self.status_msg = format!("Could not decode image: {error:#}");
            }
        }
    }

    fn rebuild_skin_from_source(&mut self) {
        let Some(source_image) = self.source_image.as_ref() else {
            return;
        };

        match PreparedSkin::from_image_with_focus(
            source_image.clone(),
            self.crop_focus[0],
            self.crop_focus[1],
        ) {
            Ok(skin) => {
                self.add_log(format!(
                    "Crop updated: focus ({:.2}, {:.2}), prepared PNG {:.1} KB",
                    self.crop_focus[0],
                    self.crop_focus[1],
                    skin.png.len() as f32 / 1024.0,
                ));
                self.skin = Some(skin);
                self.crop_dirty = false;
                self.status_msg = self
                    .language
                    .text("Crop position updated.")
                    .to_string();
            }
            Err(error) => {
                self.add_log(format!("Crop preparation failed: {error:#}"));
                self.status_msg = format!("Could not update crop: {error:#}");
            }
        }
    }

    fn save_prepared_png(&mut self) {
        let Some(skin) = &self.skin else {
            return;
        };
        let Some(path) = rfd::FileDialog::new()
            .set_file_name("aircard-skin.png")
            .save_file()
        else {
            return;
        };
        match std::fs::write(&path, &skin.png) {
            Ok(()) => {
                self.add_log(format!("Exported prepared card skin PNG: {}", path.display()));
                self.status_msg = format!("Saved prepared PNG: {}", path.display());
            }
            Err(err) => {
                self.add_log(format!("Failed to save PNG: {err}"));
                self.status_msg = format!("Could not save PNG: {err}");
            }
        }
    }

    fn toggle_syslog_scan(&mut self) {
        if self.scanning_syslog {
            if let Some(flag) = self.scan_stop_flag.take() {
                flag.store(true, Ordering::Relaxed);
            }
            self.scanning_syslog = false;
            self.add_log("Syslog scanning stopped by user.");
            self.status_msg = self.language.text("Syslog scanning stopped.").to_string();
            return;
        }

        if !self.validate_selected_transport("Syslog scan") {
            return;
        }

        let stop_flag = Arc::new(AtomicBool::new(false));
        self.scan_stop_flag = Some(Arc::clone(&stop_flag));
        self.scanning_syslog = true;
        self.add_log("Initiating syslog monitor session...");
        self.status_msg = self
            .language
            .text("Scanning syslog... Open Wallet or tap your card on iPhone.")
            .to_string();

        let (tx, rx) = channel();
        self.task_rx = Some(rx);
        let udid = self.selected_udid.clone();
        let connection_mode = self.connection_mode;
        let language = self.language;

        thread::spawn(move || {
            let tx_card = tx.clone();
            let tx_log = tx.clone();
            let res = scan_syslog_for_cards(
                udid.as_deref(),
                connection_mode,
                stop_flag,
                move |hash, name| {
                    let _ = tx_card.send(BackgroundTaskMessage::CardFound { hash, name });
                },
                move |msg| {
                    let _ = tx_log.send(BackgroundTaskMessage::Log(msg));
                },
            );
            match res {
                Ok(()) => {
                    let _ = tx.send(BackgroundTaskMessage::Done(Ok(
                        language.text("Syslog scan finished").into(),
                    )));
                }
                Err(e) => {
                    let _ = tx.send(BackgroundTaskMessage::Done(Err(e.to_string())));
                }
            }
        });
    }

    fn flash_card(&mut self) {
        if !self.validate_selected_transport("Card flash") {
            return;
        }
        let Some(udid) = self.selected_udid.clone() else {
            return;
        };
        let hash = self.card_hash.trim().to_string();
        if hash.is_empty() {
            self.add_log("Flash failed: Target card hash is empty.");
            self.status_msg = self
                .language
                .text("Please enter or scan a target card hash.")
                .to_string();
            return;
        }
        let Some(skin) = self.skin.as_ref() else {
            self.add_log("Flash failed: No skin image prepared.");
            self.status_msg = self
                .language
                .text("Please choose a card skin image first.")
                .to_string();
            return;
        };

        let png_bytes = skin.png.clone();
        let pdf_bytes = skin.pdf.clone();
        let card_name = self.scanned_name.clone();
        if let Some(ref flag) = self.scan_stop_flag {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.scanning_syslog = false;
        self.is_busy = true;
        self.resume_auto_scan_after_task = self.page == Page::Artwork;
        self.progress_step = 0;
        self.progress_total = 3;
        self.progress_msg = "Initiating card flash...".to_string();
        self.status_msg = self.language.text("Writing card skin to iPhone...").to_string();
        let connection_mode = self.connection_mode;
        let language = self.language;
        self.add_log(format!(
            "Starting card skin flash for hash: {} (UDID: {}, transport: {})",
            hash,
            udid,
            connection_mode.label()
        ));

        let (tx, rx) = channel();
        self.task_rx = Some(rx);

        thread::spawn(move || {
            let tx_progress = tx.clone();
            let tx_log = tx.clone();
            let res = flash_wallet_skin(
                &udid,
                connection_mode,
                &hash,
                &png_bytes,
                &pdf_bytes,
                move |step, total, msg| {
                    let _ = tx_progress.send(BackgroundTaskMessage::Progress {
                        step,
                        total,
                        message: msg.to_string(),
                    });
                },
                move |msg| {
                    let _ = tx_log.send(BackgroundTaskMessage::Log(msg.to_string()));
                },
            );

            match res {
                Ok(flash) => {
                    let outcome = match card_history::save_successful_change(&udid, &hash, &flash.device_hash, &card_name, flash.original_backed_up, &png_bytes) {
                        Ok(()) => Ok(language.text(if flash.original_backed_up {
                            "Card skin successfully flashed! Force quit Wallet on iPhone and reopen it."
                        } else {
                            "Card skin written without original backup; restore is unavailable. Force quit Wallet and reopen it."
                        }).into()),
                        Err(err) => Err(format!("iPhone artwork was changed, but portable history could not be saved: {err:#}")),
                    };
                    let _ = tx.send(BackgroundTaskMessage::Done(outcome));
                }
                Err(e) => {
                    let _ = tx.send(BackgroundTaskMessage::Done(Err(format!("{:#}", e))));
                }
            }
        });
    }

    fn capture_first_face(&mut self) {
        if self.is_busy || !self.validate_selected_transport("Capture card face") { return; }
        if let Some(flag) = self.scan_stop_flag.take() {
            flag.store(true, Ordering::Relaxed);
        }
        self.scanning_syslog = false;
        let Some(udid) = self.selected_udid.clone() else { return };
        let hash = self.card_hash.trim().to_string();
        if !self.card_records.iter().any(|record| record.udid == udid && record.hash == hash) {
            self.status_msg = "Add this card record before capturing its face.".into();
            return;
        }
        let mode = self.connection_mode;
        self.is_busy = true;
        self.status_msg = "Capturing first readable card face; keep iPhone unlocked...".into();
        let (tx, rx) = channel();
        self.task_rx = Some(rx);
        thread::spawn(move || {
            let result = capture_original_card(&udid, mode, &hash,
                |line| { let _ = tx.send(BackgroundTaskMessage::Log(line.to_string())); })
                .and_then(|captured| {
                    if captured.is_none() { anyhow::bail!("No readable card face was found"); }
                    card_history::mark_baseline_capture(&udid, &hash)?;
                    Ok("First readable face captured and locked. It may already have been edited.".to_string())
                })
                .map_err(|err| format!("{err:#}"));
            let _ = tx.send(BackgroundTaskMessage::Done(result));
        });
    }

    fn restore_original_card(&mut self) {
        if !self.validate_selected_transport("Restore original card") {
            return;
        }
        let Some(udid) = self.selected_udid.clone() else {
            return;
        };
        let hash = self.card_hash.trim().to_string();
        if hash.is_empty() {
            self.add_log("Restore failed: Target card hash is empty.");
            self.status_msg = self
                .language
                .text("Please enter or scan a target card hash.")
                .to_string();
            return;
        }
        if !backup_exists(&udid, &hash) {
            self.add_log("Restore failed: Original card face backup not found.");
            self.status_msg = self
                .language
                .text("Original card backup not found.")
                .to_string();
            return;
        }
        let Some(record) = self.card_records.iter().find(|record| record.udid == udid && record.hash == hash) else {
            self.status_msg = self.language.text("No successful write record for this card.").to_string();
            return;
        };
        if !record.original_backed_up {
            self.status_msg = self.language.text("Original artwork was not readable; restore is unavailable.").to_string();
            return;
        }
        let device_hash = if record.device_hash.is_empty() { hash.clone() } else { record.device_hash.clone() };

        if let Some(ref flag) = self.scan_stop_flag {
            flag.store(true, Ordering::Relaxed);
        }
        self.scanning_syslog = false;
        self.is_busy = true;
        self.resume_auto_scan_after_task = self.page == Page::Artwork;
        self.progress_step = 0;
        self.progress_total = 3;
        self.progress_msg = "Restoring original card face...".to_string();
        self.status_msg = self
            .language
            .text("Restoring original card face...")
            .to_string();
        let connection_mode = self.connection_mode;
        let language = self.language;
        self.add_log(format!(
            "Starting original card face restore for hash: {} (UDID: {}, transport: {})",
            hash,
            udid,
            connection_mode.label()
        ));

        let (tx, rx) = channel();
        self.task_rx = Some(rx);

        thread::spawn(move || {
            let tx_progress = tx.clone();
            let tx_log = tx.clone();
            let res = restore_wallet_original(
                &udid,
                connection_mode,
                &hash,
                &device_hash,
                move |step, total, msg| {
                    let _ = tx_progress.send(BackgroundTaskMessage::Progress {
                        step,
                        total,
                        message: msg.to_string(),
                    });
                },
                move |msg| {
                    let _ = tx_log.send(BackgroundTaskMessage::Log(msg.to_string()));
                },
            );

            match res {
                Ok(()) => {
                    let _ = tx.send(BackgroundTaskMessage::Done(Ok(
                        language
                            .text("Original card face restored. Force close Wallet and reopen it.")
                            .into(),
                    )));
                }
                Err(e) => {
                    let _ = tx.send(BackgroundTaskMessage::Done(Err(format!("{:#}", e))));
                }
            }
        });
    }

    fn handle_messages(&mut self) {
        let mut messages = Vec::new();
        if let Some(ref rx) = self.task_rx {
            while let Ok(msg) = rx.try_recv() {
                messages.push(msg);
            }
        }

        let mut finished = false;
        for msg in messages {
            match msg {
                BackgroundTaskMessage::Progress { step, total, message } => {
                    self.progress_step = step;
                    self.progress_total = total;
                    let localized_message = self.language.text(&message).to_string();
                    self.progress_msg = localized_message.clone();
                    let msg_str = format!("[{}/{}] {}", step, total, localized_message);
                    self.add_log(&msg_str);
                    self.status_msg = msg_str;
                }
                BackgroundTaskMessage::Log(log_line) => {
                    self.add_log(log_line);
                }
                BackgroundTaskMessage::CardFound { hash, name } => {
                    if self.card_hash.trim().is_empty() {
                        self.card_hash = hash.clone();
                        self.scanned_name = name.clone();
                    }
                    let msg_str = format!(
                        "{}: {} ({})",
                        self.language.text("Card captured"),
                        name,
                        hash
                    );
                    self.add_log(&msg_str);
                    self.status_msg = msg_str;
                }
                BackgroundTaskMessage::Done(res) => {
                    self.is_busy = false;
                    self.scanning_syslog = false;
                    if self.resume_auto_scan_after_task {
                        self.auto_scan_for = None;
                        self.resume_auto_scan_after_task = false;
                    }
                    self.card_records = card_history::load();
                    self.phone_records = card_history::phones();
                    self.record_preview_key.clear();
                    self.history_textures.clear();
                    finished = true;
                    match res {
                        Ok(ok_msg) => {
                            self.add_log(format!("Operation completed: {}", ok_msg));
                            self.status_msg = ok_msg;
                        }
                        Err(err_msg) => {
                            self.add_log(format!("Operation failed: {}", err_msg));
                            self.status_msg = format!(
                                "{}{}",
                                self.language.text("Error: "),
                                err_msg
                            );
                        }
                    }
                }
            }
        }
        if finished {
            self.task_rx = None;
        }
    }
}

pub mod md3 {
    use eframe::egui::Color32;

    // Archive-terminal palette: paper, graphite, precise amber signals.
    pub const SURFACE: Color32 = Color32::from_rgb(238, 236, 231);
    pub const SURFACE_CONTAINER: Color32 = Color32::from_rgb(250, 249, 245);
    pub const SURFACE_CONTAINER_HIGH: Color32 = Color32::from_rgb(230, 228, 221);
    pub const SURFACE_CONTAINER_HIGHEST: Color32 = Color32::from_rgb(217, 214, 205);
    pub const ON_SURFACE: Color32 = Color32::from_rgb(28, 32, 32);
    pub const ON_SURFACE_VARIANT: Color32 = Color32::from_rgb(94, 98, 96);
    pub const OUTLINE: Color32 = Color32::from_rgb(125, 128, 123);
    pub const OUTLINE_VARIANT: Color32 = Color32::from_rgb(194, 195, 187);

    // Primary
    pub const PRIMARY: Color32 = Color32::from_rgb(31, 37, 37);
    pub const ON_PRIMARY: Color32 = Color32::from_rgb(251, 250, 246);
    pub const PRIMARY_CONTAINER: Color32 = Color32::from_rgb(255, 210, 113);
    pub const ON_PRIMARY_CONTAINER: Color32 = Color32::from_rgb(41, 37, 28);

    // Secondary
    pub const SECONDARY_CONTAINER: Color32 = Color32::from_rgb(222, 221, 214);
    pub const ON_SECONDARY_CONTAINER: Color32 = Color32::from_rgb(40, 43, 41);

    // Tertiary
    pub const TERTIARY_CONTAINER: Color32 = Color32::from_rgb(255, 239, 199);
    pub const ON_TERTIARY_CONTAINER: Color32 = Color32::from_rgb(91, 63, 26);

    // Error
    pub const ERROR: Color32 = Color32::from_rgb(166, 57, 48);
    pub const ERROR_CONTAINER: Color32 = Color32::from_rgb(249, 219, 212);

    // Extra
    pub const SUCCESS: Color32 = Color32::from_rgb(48, 128, 91);
    pub const SIGNAL: Color32 = Color32::from_rgb(231, 165, 57);
}

fn draw_status_dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SelectedArtwork {
    Origin,
    Legacy,
    Version(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page { Cards, Artwork }

fn load_record_texture(ctx: &egui::Context, key: &str, path: &std::path::Path) -> Option<egui::TextureHandle> {
    let image = image::open(path).ok()?.thumbnail(640, 404).to_rgba8();
    let color = egui::ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize], image.as_raw(),
    );
    Some(ctx.load_texture(key, color, egui::TextureOptions::LINEAR))
}

fn setup_custom_fonts(ctx: &egui::Context) {
    let Some(windows_dir) = std::env::var_os("WINDIR").map(PathBuf::from) else {
        return;
    };
    let font_candidates = [
        windows_dir.join("Fonts").join("Deng.ttf"),
        windows_dir.join("Fonts").join("simhei.ttf"),
        windows_dir.join("Fonts").join("simsunb.ttf"),
    ];

    let Some(font_bytes) = font_candidates
        .iter()
        .find_map(|path| std::fs::read(path).ok())
    else {
        return;
    };

    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "windows-cjk".to_owned(),
        egui::FontData::from_owned(font_bytes).into(),
    );
    fonts
        .families
        .get_mut(&egui::FontFamily::Proportional)
        .expect("egui proportional font family should exist")
        .insert(0, "windows-cjk".to_owned());
    fonts
        .families
        .get_mut(&egui::FontFamily::Monospace)
        .expect("egui monospace font family should exist")
        .insert(0, "windows-cjk".to_owned());
    ctx.set_fonts(fonts);
}

fn setup_custom_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::light();

    visuals.panel_fill = md3::SURFACE;
    visuals.window_fill = md3::SURFACE;
    visuals.extreme_bg_color = md3::SURFACE_CONTAINER;
    visuals.faint_bg_color = md3::SURFACE_CONTAINER;

    visuals.window_corner_radius = 4.into();
    visuals.menu_corner_radius = 4.into();

    visuals.widgets.noninteractive.corner_radius = 3.into();
    visuals.widgets.noninteractive.bg_fill = md3::SURFACE_CONTAINER;
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::NONE;
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0_f32, md3::ON_SURFACE);

    visuals.widgets.inactive.bg_fill = md3::SURFACE_CONTAINER_HIGH;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::NONE;
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, md3::ON_SURFACE_VARIANT);
    visuals.widgets.inactive.corner_radius = 3.into();

    visuals.widgets.hovered.bg_fill = md3::SURFACE_CONTAINER_HIGHEST;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::NONE;
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0_f32, md3::ON_SURFACE);
    visuals.widgets.hovered.corner_radius = 3.into();

    visuals.widgets.active.bg_fill = md3::PRIMARY_CONTAINER;
    visuals.widgets.active.bg_stroke = egui::Stroke::NONE;
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0_f32, md3::ON_PRIMARY_CONTAINER);
    visuals.widgets.active.corner_radius = 3.into();

    visuals.widgets.open.bg_fill = md3::SURFACE_CONTAINER_HIGHEST;
    visuals.widgets.open.corner_radius = 3.into();
    visuals.widgets.open.bg_stroke = egui::Stroke::NONE;

    visuals.selection.bg_fill = md3::PRIMARY_CONTAINER;
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, md3::PRIMARY);

    ctx.set_visuals(visuals);

    ctx.style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(16.0, 8.0);
    });
}

fn m3_card<R>(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(md3::SURFACE_CONTAINER)
        .stroke(egui::Stroke::new(1.0_f32, md3::OUTLINE_VARIANT))
        .corner_radius(2)
        .inner_margin(egui::Margin::same(22))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.vertical(add_contents).inner
        })
        .inner
}

fn m3_button_filled(ui: &mut egui::Ui, label: &str) -> bool {
    let btn = egui::Button::new(
        egui::RichText::new(label).size(13.0).color(md3::ON_PRIMARY),
    )
    .fill(md3::PRIMARY)
    .corner_radius(2)
    .stroke(egui::Stroke::NONE);
    ui.add(btn).clicked()
}

fn m3_button_tonal(ui: &mut egui::Ui, label: &str) -> bool {
    let btn = egui::Button::new(
        egui::RichText::new(label).size(13.0).color(md3::ON_SECONDARY_CONTAINER),
    )
    .fill(md3::SECONDARY_CONTAINER)
    .corner_radius(2)
    .stroke(egui::Stroke::NONE);
    ui.add(btn).clicked()
}

fn m3_button_outlined(ui: &mut egui::Ui, label: &str) -> bool {
    let btn = egui::Button::new(
        egui::RichText::new(label).size(13.0).color(md3::PRIMARY),
    )
    .fill(egui::Color32::TRANSPARENT)
    .corner_radius(2)
    .stroke(egui::Stroke::new(1.0_f32, md3::OUTLINE));
    ui.add(btn).clicked()
}

fn picker<R>(
    ui: &mut egui::Ui,
    id: &str,
    label: &str,
    width: f32,
    enabled: bool,
    indicator: Option<egui::Color32>,
    contents: impl FnOnce(&mut egui::Ui) -> R,
) -> Option<R> {
    let button = egui::Button::new(
        egui::RichText::new(if indicator.is_some() { "" } else { label })
            .size(12.0)
            .color(if enabled { md3::ON_SURFACE } else { md3::ON_SURFACE_VARIANT }),
    )
    .fill(md3::SURFACE_CONTAINER_HIGH)
    .stroke(egui::Stroke::new(1.0_f32, md3::OUTLINE_VARIANT))
    .corner_radius(2)
    .min_size(egui::vec2(width, 36.0));
    let response = ui.add_enabled(enabled, button);
    if let Some(color) = indicator {
        let pulse = if color == md3::SUCCESS { 0.7 * (ui.input(|input| input.time) as f32 * 3.0).sin() } else { 0.0 };
        ui.painter().circle_filled(response.rect.left_center() + egui::vec2(15.0, 0.0), 3.2 + pulse, color);
        ui.painter().text(response.rect.left_center() + egui::vec2(27.0, 0.0), egui::Align2::LEFT_CENTER,
            label, egui::FontId::proportional(12.0), if enabled { md3::ON_SURFACE } else { md3::ON_SURFACE_VARIANT });
        if color == md3::SUCCESS { ui.ctx().request_repaint_after(std::time::Duration::from_millis(60)); }
    }
    let x = response.rect.right() - 17.0;
    let y = response.rect.center().y;
    ui.painter().add(egui::Shape::convex_polygon(
        vec![egui::pos2(x - 4.0, y - 2.0), egui::pos2(x + 4.0, y - 2.0), egui::pos2(x, y + 3.0)],
        if enabled { md3::SIGNAL } else { md3::OUTLINE },
        egui::Stroke::NONE,
    ));
    egui::Popup::menu(&response)
        .id(egui::Id::new(id))
        .width(response.rect.width())
        .frame(
            egui::Frame::new()
                .fill(md3::SURFACE_CONTAINER_HIGH)
                .stroke(egui::Stroke::new(1.0_f32, md3::OUTLINE_VARIANT))
                .corner_radius(2)
                .inner_margin(egui::Margin::same(8)),
        )
        .show(contents)
        .map(|inner| inner.inner)
}

fn refresh_icon_button(ui: &mut egui::Ui, tooltip: &str) -> bool {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(34.0, 34.0), egui::Sense::click());
    let color = if response.hovered() { md3::SIGNAL } else { md3::PRIMARY };
    let center = rect.center();
    let radius = 8.0;
    let points: Vec<egui::Pos2> = (0..=24).map(|step| {
        let angle = (-145.0_f32 + step as f32 * 11.5).to_radians();
        center + egui::vec2(angle.cos() * radius, angle.sin() * radius)
    }).collect();
    ui.painter().add(egui::Shape::line(points, egui::Stroke::new(1.8_f32, color)));
    let tip = center + egui::vec2((-145.0_f32).to_radians().cos() * radius, (-145.0_f32).to_radians().sin() * radius);
    ui.painter().line_segment([tip, tip + egui::vec2(0.0, -5.0)], egui::Stroke::new(1.8_f32, color));
    ui.painter().line_segment([tip, tip + egui::vec2(5.0, 0.0)], egui::Stroke::new(1.8_f32, color));
    response.on_hover_text(tooltip).clicked()
}

fn picker_option(ui: &mut egui::Ui, label: &str, selected: bool) -> bool {
    let clicked = ui
        .add(
            egui::Button::new(
                egui::RichText::new(label)
                    .size(12.0)
                    .color(if selected { md3::ON_PRIMARY_CONTAINER } else { md3::ON_SURFACE }),
            )
            .fill(if selected { md3::PRIMARY_CONTAINER } else { md3::SURFACE_CONTAINER_HIGH })
            .stroke(egui::Stroke::NONE)
            .corner_radius(2)
            .min_size(egui::vec2(ui.available_width(), 30.0)),
        )
        .clicked();
    if clicked {
        ui.close();
    }

    clicked
}

impl AirCardApp {
    fn apply_selected_artwork(&mut self) {
        let Some(selection) = self.selected_artwork.clone() else { return };
        if selection == SelectedArtwork::Origin {
            self.restore_original_card();
            return;
        }
        if !self.validate_selected_transport("Restore artwork version") { return; }
        let Some(udid) = self.selected_udid.clone() else { return };
        let hash = self.card_hash.trim().to_string();
        let Some(record) = self.card_records.iter().find(|record| record.udid == udid && record.hash == hash) else { return };
        let path = match selection {
            SelectedArtwork::Legacy => changed_preview_path(&udid, &hash),
            SelectedArtwork::Version(id) => {
                let Some(version) = record.versions.iter().find(|version| version.id == id) else { return };
                let Some(path) = card_history::version_image_path(&udid, &hash, version) else { return };
                path
            }
            SelectedArtwork::Origin => unreachable!(),
        };
        let png = match std::fs::read(&path) {
            Ok(bytes) if image::load_from_memory(&bytes).is_ok() => bytes,
            _ => { self.status_msg = "Selected artwork image is unavailable or invalid.".into(); return; }
        };
        let pdf = match png_to_pdf(&png) {
            Ok(pdf) => pdf,
            Err(err) => { self.status_msg = format!("Cannot prepare selected artwork: {err:#}"); return; }
        };
        let name = record.name.clone();
        if let Some(flag) = self.scan_stop_flag.take() { flag.store(true, Ordering::Relaxed); }
        self.scanning_syslog = false;
        self.is_busy = true;
        self.progress_step = 0;
        self.progress_total = 3;
        self.resume_auto_scan_after_task = self.page == Page::Artwork;
        self.status_msg = "Applying selected artwork to iPhone...".into();
        let connection_mode = self.connection_mode;
        let (tx, rx) = channel();
        self.task_rx = Some(rx);
        thread::spawn(move || {
            let tx_progress = tx.clone();
            let tx_log = tx.clone();
            let result = flash_wallet_skin(&udid, connection_mode, &hash, &png, &pdf,
                move |step, total, message| { let _ = tx_progress.send(BackgroundTaskMessage::Progress { step, total, message: message.into() }); },
                move |message| { let _ = tx_log.send(BackgroundTaskMessage::Log(message.into())); });
            let result = match result {
                Ok(outcome) => card_history::save_successful_change(&udid, &hash, &outcome.device_hash, &name, outcome.original_backed_up, &png)
                    .map(|_| "Selected artwork applied and recorded. Reopen Wallet on iPhone.".to_string())
                    .map_err(|err| format!("iPhone artwork changed, but history save failed: {err:#}")),
                Err(err) => Err(format!("{err:#}")),
            };
            let _ = tx.send(BackgroundTaskMessage::Done(result));
        });
    }

}

impl eframe::App for AirCardApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_messages();
        self.refresh_record_previews(ctx);
        let language = self.language;

        if self.is_busy || self.scanning_syslog {
            ctx.request_repaint_after(std::time::Duration::from_millis(80));
        }

        egui::TopBottomPanel::top("header")
            .frame(
                egui::Frame::new()
                    .fill(md3::SURFACE_CONTAINER)
                    .inner_margin(egui::Margin::symmetric(28, 18)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    egui::Frame::new()
                        .fill(md3::PRIMARY)
                        .inner_margin(egui::Margin::symmetric(11, 8))
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new("A").strong().size(25.0).color(md3::SIGNAL));
                        });
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new("AirCard").strong().size(25.0).color(md3::ON_SURFACE));
                        ui.label(egui::RichText::new("WALLET / LOCAL TERMINAL").monospace().size(10.0).color(md3::ON_SURFACE_VARIANT));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let mut next_language = self.language;
                        picker(ui, "language_picker", language.option_label(next_language), 112.0, true, None, |ui| {
                            for option in [Language::English, Language::SimplifiedChinese] {
                                if picker_option(ui, language.option_label(option), next_language == option) {
                                    next_language = option;
                                }
                            }
                        });
                        if next_language != self.language {
                            self.language = next_language;
                            self.language.save();
                            self.status_msg = self.language.text("Language changed.").to_string();
                        }
                    });
                });
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.add_space((ui.available_width() - 280.0).max(0.0) / 2.0);
                    for (page, label) in [(Page::Cards, "CARDS"), (Page::Artwork, "ARTWORK")] {
                        let active = self.page == page;
                        let button = egui::Button::new(egui::RichText::new(label).monospace().strong().size(12.0)
                            .color(if active { md3::ON_PRIMARY } else { md3::ON_SURFACE_VARIANT }))
                            .fill(if active { md3::PRIMARY } else { md3::SURFACE_CONTAINER })
                            .stroke(egui::Stroke::NONE)
                            .corner_radius(2)
                            .min_size(egui::vec2(124.0, 34.0));
                        if ui.add(button).clicked() && self.page != page {
                            if self.scanning_syslog {
                                if let Some(flag) = self.scan_stop_flag.take() { flag.store(true, Ordering::Relaxed); }
                                self.scanning_syslog = false;
                            }
                            self.page = page;
                            self.auto_scan_for = None;
                        }
                    }
                });
                ui.add_space(16.0);
                ui.painter().hline(ui.available_rect_before_wrap().x_range(), ui.cursor().top(), egui::Stroke::new(1.0_f32, md3::OUTLINE_VARIANT));
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if refresh_icon_button(ui, language.text("Refresh")) {
                                    self.refresh_devices();
                                }
                            let controls_enabled = !self.is_busy;
                            let mut next_mode = self.connection_mode;
                            picker(ui, "connection_picker", language.text(next_mode.label()), 176.0, controls_enabled, None, |ui| {
                                for mode in ConnectionMode::ALL {
                                    if picker_option(ui, language.text(mode.label()), next_mode == mode) {
                                        next_mode = mode;
                                    }
                                }
                            });
                            if next_mode != self.connection_mode {
                                if let Some(flag) = self.scan_stop_flag.take() { flag.store(true, Ordering::Relaxed); }
                                self.scanning_syslog = false;
                                self.auto_scan_for = None;
                                self.connection_mode = next_mode;
                                self.add_log(format!("Transport mode changed to {}.", self.connection_mode.label()));
                                self.status_msg = format!("{}: {}", language.text("Transport mode"), language.text(self.connection_mode.label()));
                            }
                            let mut next_udid = self.selected_udid.clone();
                            let mut archived_udids: Vec<String> = self.card_records.iter()
                                .filter(|record| !self.devices.iter().any(|device| device.udid == record.udid))
                                .map(|record| record.udid.clone())
                                .collect();
                            archived_udids.extend(self.phone_records.iter()
                                .filter(|phone| !self.devices.iter().any(|device| device.udid == phone.udid))
                                .map(|phone| phone.udid.clone()));
                            archived_udids.sort();
                            archived_udids.dedup();
                            let selected_label = self.devices.iter()
                                .find(|device| Some(&device.udid) == self.selected_udid.as_ref())
                                .map(|device| format!("{}  ·  {}", device.name, device.transport_summary()))
                                .or_else(|| self.selected_udid.as_ref().filter(|udid| archived_udids.contains(udid)).map(|udid| {
                                    let name = self.phone_records.iter().find(|phone| phone.udid == *udid)
                                        .filter(|phone| !phone.name.is_empty()).map(|phone| phone.name.as_str())
                                        .unwrap_or(language.text("Archived iPhone"));
                                    format!("{}  ·  {}", name, &udid[udid.len().saturating_sub(6)..])
                                }))
                                .unwrap_or_else(|| language.text("No device").to_string());
                            picker(ui, "device_picker", &selected_label, 224.0, controls_enabled && (!self.devices.is_empty() || !archived_udids.is_empty()),
                                Some(if self.selected_transport_available() { md3::SUCCESS } else { md3::ERROR }), |ui| {
                                for device in &self.devices {
                                    let label = format!("{}  ·  {}", device.name, device.transport_summary());
                                    if picker_option(ui, &label, Some(&device.udid) == next_udid.as_ref()) {
                                        next_udid = Some(device.udid.clone());
                                    }
                                }
                                for udid in &archived_udids {
                                    let name = self.phone_records.iter().find(|phone| phone.udid == *udid)
                                        .filter(|phone| !phone.name.is_empty()).map(|phone| phone.name.as_str())
                                        .unwrap_or(language.text("Archived iPhone"));
                                    let label = format!("{}  ·  {}", name, &udid[udid.len().saturating_sub(6)..]);
                                    if picker_option(ui, &label, Some(udid) == next_udid.as_ref()) {
                                        next_udid = Some(udid.clone());
                                    }
                                }
                            });
                            if next_udid != self.selected_udid {
                                if let Some(flag) = self.scan_stop_flag.take() { flag.store(true, Ordering::Relaxed); }
                                self.scanning_syslog = false;
                                self.auto_scan_for = None;
                                self.selected_udid = next_udid;
                                self.card_hash.clear();
                                self.scanned_name.clear();
                                self.selected_artwork = None;
                                if let Some(selected) = self.selected_udid.clone() {
                                    self.add_log(format!("Selected device: {}", selected));
                                }
                            }
                    });
                });
            });

        if self.page == Page::Artwork && !self.is_busy && !self.scanning_syslog && self.selected_transport_available() {
            let selected = self.selected_udid.clone().unwrap_or_default();
            if self.auto_scan_for.as_deref() != Some(selected.as_str()) {
                self.auto_scan_for = Some(selected);
                self.toggle_syslog_scan();
            }
        }

        // Status bar
        egui::TopBottomPanel::bottom("status_bar")
            .frame(
                egui::Frame::new()
                    .fill(md3::PRIMARY)
                    .inner_margin(egui::Margin::symmetric(24, 9)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let dot_col = if self.is_busy || self.scanning_syslog {
                        md3::PRIMARY
                    } else if self.status_msg.starts_with("Error") || self.status_msg.starts_with("Failed") {
                        md3::ERROR
                    } else {
                        md3::SUCCESS
                    };
                    draw_status_dot(ui, dot_col);
                    if self.is_busy || self.scanning_syslog { ui.spinner(); }
                    ui.label(egui::RichText::new(&self.status_msg).size(11.0).color(md3::ON_PRIMARY));

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let btn_text = if self.show_logs_window {
                            language.text("Logs [x]")
                        } else {
                            language.text("Logs")
                        };
                        let btn = egui::Button::new(
                            egui::RichText::new(btn_text).size(11.0).color(
                                if self.show_logs_window { md3::ON_PRIMARY_CONTAINER } else { md3::ON_PRIMARY }
                            ),
                        )
                        .fill(if self.show_logs_window { md3::PRIMARY_CONTAINER } else { egui::Color32::TRANSPARENT })
                        .corner_radius(2)
                        .stroke(egui::Stroke::new(1.0_f32, if self.show_logs_window { md3::SIGNAL } else { md3::OUTLINE }));
                        if ui.add(btn).clicked() {
                            self.show_logs_window = !self.show_logs_window;
                        }
                    });
                });
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(md3::SURFACE)
                    .inner_margin(egui::Margin::same(22)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let content_width = ui.available_width().min(1180.0);
                    ui.horizontal_top(|ui| {
                        ui.add_space((ui.available_width() - content_width).max(0.0) * 0.5);
                        ui.allocate_ui_with_layout(egui::vec2(content_width, 0.0), egui::Layout::top_down(egui::Align::Min), |content| {
                            match self.page {
                                Page::Cards => self.show_cards_page(ctx, content),
                                Page::Artwork => self.show_wallet_tab(ctx, content),
                            }
                        });
                    });
                });
            });

        let mut show_logs = self.show_logs_window;
        let mut file_saved_msg: Option<String> = None;
        if show_logs {
            egui::Window::new(language.text("Logs"))
                .open(&mut show_logs)
                .default_size([540.0, 300.0])
                .min_size([360.0, 180.0])
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        if m3_button_tonal(ui, language.text("Copy Logs")) {
                            ctx.copy_text(self.logs.join("\n"));
                        }
                        if m3_button_outlined(ui, language.text("Save to File...")) {
                            if let Some(path) = rfd::FileDialog::new()
                                .set_file_name("aircard-diagnostics.log")
                                .add_filter("Log files", &["log", "txt"])
                                .save_file()
                            {
                                let content = self.logs.join("\r\n");
                                let _ = std::fs::write(&path, content);
                                file_saved_msg = Some(format!("Saved log file to {}", path.display()));
                            }
                        }
                        if m3_button_outlined(ui, language.text("Clear")) {
                            self.logs.clear();
                        }
                        ui.label(
                            egui::RichText::new(format!(
                                "{} {}",
                                self.logs.len(),
                                language.text("entries")
                            ))
                                .size(11.0)
                                .color(md3::ON_SURFACE_VARIANT),
                        );
                    });
                    ui.add_space(8.0);
                    egui::Frame::new()
                        .fill(md3::SURFACE_CONTAINER_HIGH)
                        .corner_radius(12)
                        .inner_margin(egui::Margin::same(10))
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .stick_to_bottom(true)
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    if self.logs.is_empty() {
                                        ui.label(egui::RichText::new(language.text("No events logged yet.")).size(11.0).color(md3::ON_SURFACE_VARIANT));
                                    } else {
                                        for line in &self.logs {
                                            ui.label(
                                                egui::RichText::new(line)
                                                    .size(10.5)
                                                    .monospace()
                                                    .color(md3::ON_SURFACE),
                                            );
                                        }
                                    }
                                });
                        });
                });
            self.show_logs_window = show_logs;
            if let Some(msg) = file_saved_msg {
                self.add_log(msg);
            }
        }
    }
}

impl AirCardApp {
    fn reload_local_records(&mut self) {
        self.card_records = card_history::load();
        self.phone_records = card_history::phones();
        self.record_preview_key.clear();
        self.history_textures.clear();
    }

    fn history_texture(&mut self, ctx: &egui::Context, path: &std::path::Path) -> Option<egui::TextureHandle> {
        if !self.history_textures.contains_key(path) {
            let texture = load_record_texture(ctx, &format!("history:{}", path.display()), path)?;
            self.history_textures.insert(path.to_path_buf(), texture);
        }
        self.history_textures.get(path).cloned()
    }

    fn show_cards_page(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let content_width = ui.available_width().min(1180.0);
        ui.set_max_width(content_width);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("CARD ARCHIVE").monospace().size(11.0).color(md3::PRIMARY));
                ui.label(egui::RichText::new("Cards").strong().size(32.0).color(md3::ON_SURFACE));
                ui.label(egui::RichText::new("Phones, cards and every successful artwork write · stored beside this EXE")
                    .size(12.0).color(md3::ON_SURFACE_VARIANT));
            });
        });
        ui.add_space(18.0);

        let mut known_phones: Vec<(String, String, bool)> = self.devices.iter()
            .map(|device| (device.udid.clone(), device.name.clone(), true)).collect();
        for phone in &self.phone_records {
            if !known_phones.iter().any(|item| item.0 == phone.udid) {
                let name = if phone.name.is_empty() { format!("iPhone · {}", &phone.udid[phone.udid.len().saturating_sub(6)..]) } else { phone.name.clone() };
                known_phones.push((phone.udid.clone(), name, false));
            }
        }
        let selected_record = self.selected_udid.as_deref().and_then(|udid|
            self.card_records.iter().find(|record| record.udid == udid && record.hash == self.card_hash.trim())
        ).cloned();

        ui.horizontal_top(|ui| {
            let side_width = 310.0;
            let detail_width = (ui.available_width() - side_width - 16.0).max(440.0);
            ui.allocate_ui_with_layout(egui::vec2(side_width, 0.0), egui::Layout::top_down(egui::Align::Min), |side| {
                m3_card(side, |ui| {
                    ui.label(egui::RichText::new("01 / PHONES").monospace().size(10.0).color(md3::PRIMARY));
                    ui.add_space(8.0);
                    if known_phones.is_empty() {
                        ui.label("No phone records yet. Connect an iPhone or add one by UDID.");
                    }
                    for (udid, name, online) in &known_phones {
                        let selected = self.selected_udid.as_deref() == Some(udid.as_str());
                        let label = format!("{}  {}", name, if *online { "●" } else { "○" });
                        let button = egui::Button::new(label)
                            .fill(if selected { md3::PRIMARY_CONTAINER } else { md3::SURFACE_CONTAINER_HIGH })
                            .corner_radius(2)
                            .min_size(egui::vec2(ui.available_width(), 38.0));
                        let response = ui.add(button);
                        if response.clicked() {
                            self.selected_udid = Some(udid.clone());
                            self.card_hash.clear();
                            self.selected_artwork = None;
                            self.pending_delete_card = None;
                        }
                        if self.phone_records.iter().any(|phone| phone.udid == *udid) {
                            response.context_menu(|ui| {
                                if ui.button("Delete local phone record…").clicked() {
                                    self.pending_delete_phone = Some(udid.clone());
                                    ui.close();
                                }
                            });
                        }
                    }
                    ui.add_space(12.0);
                    if m3_button_outlined(ui, if self.adding_phone { "Cancel" } else { "+ Add phone" }) {
                        self.adding_phone = !self.adding_phone;
                    }
                    if self.adding_phone {
                        ui.add_space(8.0);
                        ui.label("Phone UDID");
                        ui.text_edit_singleline(&mut self.new_phone_udid);
                        ui.label("Name (optional)");
                        ui.text_edit_singleline(&mut self.new_phone_name);
                            if m3_button_filled(ui, "Save phone") && !self.is_busy && !self.scanning_syslog {
                            match card_history::add_phone(&self.new_phone_udid, &self.new_phone_name) {
                                Ok(()) => {
                                    self.selected_udid = Some(self.new_phone_udid.trim().to_string());
                                    self.new_phone_udid.clear();
                                    self.new_phone_name.clear();
                                    self.adding_phone = false;
                                    self.reload_local_records();
                                }
                                Err(err) => self.status_msg = format!("Add phone failed: {err:#}"),
                            }
                        }
                    }
                });
                side.add_space(14.0);
                m3_card(side, |ui| {
                    ui.label(egui::RichText::new("02 / CARDS").monospace().size(10.0).color(md3::PRIMARY));
                    ui.add_space(8.0);
                    if let Some(udid) = self.selected_udid.clone() {
                        let cards: Vec<CardRecord> = self.card_records.iter().filter(|record| record.udid == udid).cloned().collect();
                        for card in cards {
                            let key = (card.udid.clone(), card.hash.clone());
                            if self.renaming_card.as_ref() == Some(&key) {
                                ui.horizontal(|ui| {
                                    let response = ui.add(egui::TextEdit::singleline(&mut self.rename_text)
                                        .desired_width((ui.available_width() - 80.0).max(100.0)));
                                    let save = ui.button("✓").clicked() || (response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)));
                                    if save {
                                        match card_history::rename_card(&card.udid, &card.hash, &self.rename_text) {
                                            Ok(()) => { self.scanned_name = self.rename_text.trim().into(); self.renaming_card = None; self.reload_local_records(); }
                                            Err(err) => self.status_msg = format!("Rename card failed: {err:#}"),
                                        }
                                    }
                                    if ui.button("×").clicked() { self.renaming_card = None; }
                                });
                                continue;
                            }
                            let selected = self.card_hash.trim() == card.hash;
                            let button = egui::Button::new(format!("{}  ·  {}…", card.name, &card.hash[..8.min(card.hash.len())]))
                                .fill(if selected { md3::PRIMARY_CONTAINER } else { md3::SURFACE_CONTAINER_HIGH })
                                .corner_radius(2).min_size(egui::vec2(ui.available_width(), 38.0));
                            let response = ui.add(button).on_hover_text("Double-click to rename");
                            if response.double_clicked() {
                                self.rename_text = card.name.clone();
                                self.renaming_card = Some(key);
                            } else if response.clicked() {
                                self.card_hash = card.hash.clone();
                                self.scanned_name = card.name;
                                self.selected_artwork = None;
                                self.pending_delete_card = None;
                            }
                        }
                        ui.add_space(10.0);
                        if m3_button_outlined(ui, if self.adding_card { "Cancel" } else { "+ Add card" }) {
                            self.adding_card = !self.adding_card;
                        }
                        if self.adding_card {
                            ui.add_space(8.0);
                            ui.label("Card hash");
                            ui.text_edit_singleline(&mut self.new_card_hash);
                            ui.label("Card name (optional)");
                            ui.text_edit_singleline(&mut self.new_card_name);
                            if m3_button_filled(ui, "Save card") && !self.is_busy && !self.scanning_syslog {
                                match card_history::add_card(&udid, self.new_card_hash.trim(), &self.new_card_name) {
                                    Ok(()) => {
                                        self.card_hash = self.new_card_hash.trim().to_string();
                                        self.scanned_name = self.new_card_name.trim().to_string();
                                        self.new_card_hash.clear(); self.new_card_name.clear(); self.adding_card = false;
                                        self.reload_local_records();
                                    }
                                    Err(err) => self.status_msg = format!("Add card failed: {err:#}"),
                                }
                            }
                        }
                    } else { ui.label("Select a phone first."); }
                });
            });

            ui.add_space(16.0);
            ui.allocate_ui_with_layout(egui::vec2(detail_width, 0.0), egui::Layout::top_down(egui::Align::Min), |detail| {
                m3_card(detail, |ui| {
                    ui.label(egui::RichText::new("03 / ARTWORK HISTORY").monospace().size(10.0).color(md3::PRIMARY));
                    ui.add_space(6.0);
                    if let Some(record) = selected_record.clone() {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&record.name).strong().size(24.0).color(md3::ON_SURFACE));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if m3_button_filled(ui, "Edit artwork →") { self.page = Page::Artwork; self.auto_scan_for = None; }
                            });
                        });
                        ui.label(egui::RichText::new(&record.hash).monospace().size(11.0).color(md3::ON_SURFACE_VARIANT));
                        ui.add_space(14.0);
                        let mut tiles: Vec<(SelectedArtwork, String, Option<PathBuf>)> = Vec::new();
                        let baseline_label = if record.baseline_from_prior_change {
                            "00 / ORIGIN · PREVIOUSLY EDITED"
                        } else { "00 / ORIGIN" };
                        tiles.push((SelectedArtwork::Origin, baseline_label.to_string(), if record.original_backed_up {
                            original_preview_path(&record.udid, &record.hash)
                        } else { None }));
                        if record.versions.is_empty() && record.changed_at > 0 {
                            tiles.push((SelectedArtwork::Legacy, "01 / WRITE".to_string(), Some(changed_preview_path(&record.udid, &record.hash))));
                        }
                        for (index, version) in record.versions.iter().enumerate() {
                            tiles.push((SelectedArtwork::Version(version.id.clone()), format!("{:02} / WRITE", index + 1), card_history::version_image_path(&record.udid, &record.hash, version)));
                        }
                        egui::ScrollArea::horizontal().id_salt("art-history-strip").show(ui, |ui| {
                            ui.horizontal_top(|ui| {
                                for (selection, label, path) in tiles {
                                    let active = self.selected_artwork.as_ref() == Some(&selection);
                                    let response = egui::Frame::new()
                                        .fill(if active { md3::PRIMARY_CONTAINER } else { md3::SURFACE_CONTAINER_HIGH })
                                        .stroke(egui::Stroke::new(if active { 2.0_f32 } else { 1.0_f32 }, if active { md3::SIGNAL } else { md3::OUTLINE_VARIANT }))
                                        .inner_margin(egui::Margin::same(10)).corner_radius(2)
                                        .show(ui, |ui| {
                                            ui.set_width(196.0);
                                            ui.label(egui::RichText::new(label).monospace().size(10.0).color(md3::PRIMARY));
                                            ui.add_space(7.0);
                                            let texture = path.as_deref().and_then(|path| self.history_texture(ctx, path));
                                            if let Some(texture) = texture {
                                                ui.image((texture.id(), egui::vec2(176.0, 111.0)));
                                            } else {
                                                ui.allocate_ui(egui::vec2(176.0, 111.0), |ui| {
                                                    ui.centered_and_justified(|ui| { ui.label("No captured image"); });
                                                });
                                            }
                                        }).response.interact(egui::Sense::click());
                                    if response.clicked() {
                                        self.selected_artwork = if active { None } else { Some(selection) };
                                        self.pending_delete_card = None;
                                        self.pending_delete_version = None;
                                    }
                                }
                            });
                        });
                        ui.add_space(14.0);
                        ui.label(egui::RichText::new(if record.baseline_from_prior_change {
                            "First capture happened after an earlier edit. It is a restorable baseline, not proof of the factory artwork."
                        } else if record.original_backed_up {
                            "The first readable face is locked and subsequent writes cannot overwrite it."
                        } else {
                            "No first-captured face yet. The next write will try to capture it before changing the card."
                        }).size(11.0).color(md3::ON_SURFACE_VARIANT));
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            if !record.original_backed_up && self.selected_transport_available() && !self.is_busy {
                                if m3_button_tonal(ui, "Capture current face") { self.capture_first_face(); }
                            }
                            let selected_path_exists = match self.selected_artwork.as_ref() {
                                Some(SelectedArtwork::Origin) => record.original_backed_up && backup_exists(&record.udid, &record.hash),
                                Some(SelectedArtwork::Legacy) => changed_preview_path(&record.udid, &record.hash).is_file(),
                                Some(SelectedArtwork::Version(id)) => record.versions.iter().find(|version| &version.id == id)
                                    .and_then(|version| card_history::version_image_path(&record.udid, &record.hash, version))
                                    .is_some_and(|path| path.is_file()),
                                None => false,
                            };
                            let restore_enabled = selected_path_exists && self.selected_transport_available() && !self.is_busy;
                            if ui.add_enabled(restore_enabled, egui::Button::new("Restore selected").fill(md3::SECONDARY_CONTAINER)).clicked() {
                                self.apply_selected_artwork();
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let delete_enabled = self.selected_artwork != Some(SelectedArtwork::Origin) && !self.is_busy && !self.scanning_syslog;
                                let delete = egui::Button::new(egui::RichText::new("Delete").color(md3::ERROR))
                                    .fill(md3::ERROR_CONTAINER).stroke(egui::Stroke::new(1.0_f32, md3::ERROR));
                                if ui.add_enabled(delete_enabled, delete).clicked() {
                                    match self.selected_artwork.clone() {
                                        Some(selection) => self.pending_delete_version = Some((record.udid.clone(), record.hash.clone(), selection)),
                                        None => self.pending_delete_card = Some((record.udid.clone(), record.hash.clone())),
                                    }
                                }
                            });
                        });
                    } else {
                        ui.label(egui::RichText::new("Select a phone and card").strong().size(24.0).color(md3::ON_SURFACE));
                        ui.label("Cards that are only scanned are not saved automatically. Add a card manually, or save after a successful artwork write.");
                    }
                });
            });
        });
        if let Some(udid) = self.pending_delete_phone.clone() {
            egui::Window::new("Delete phone record?").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO).show(ctx, |ui| {
                ui.label("This removes the phone and all its local card records and images. The iPhone is not changed.");
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() { self.pending_delete_phone = None; }
                    if ui.add(egui::Button::new("Delete phone and local cards").fill(md3::ERROR_CONTAINER)).clicked() && !self.is_busy {
                        match card_history::delete_phone(&udid) {
                            Ok(()) => {
                                if self.selected_udid.as_deref() == Some(&udid) { self.selected_udid = None; self.card_hash.clear(); }
                                self.selected_artwork = None;
                                self.pending_delete_phone = None;
                                self.reload_local_records();
                            }
                            Err(err) => self.status_msg = format!("Delete phone failed: {err:#}"),
                        }
                    }
                });
            });
        }
        if let Some((udid, hash)) = self.pending_delete_card.clone() {
            egui::Window::new("Delete card record?").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO).show(ctx, |ui| {
                ui.label("This removes all local history for this card, including ORIGIN. The iPhone is not changed.");
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() { self.pending_delete_card = None; }
                    if ui.add(egui::Button::new("Delete entire card record").fill(md3::ERROR_CONTAINER)).clicked() && !self.is_busy {
                        match card_history::delete_card(&udid, &hash) {
                            Ok(()) => { self.card_hash.clear(); self.selected_artwork = None; self.pending_delete_card = None; self.reload_local_records(); }
                            Err(err) => self.status_msg = format!("Delete card failed: {err:#}"),
                        }
                    }
                });
            });
        }
        if let Some((udid, hash, selection)) = self.pending_delete_version.clone() {
            egui::Window::new("Delete artwork record?").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO).show(ctx, |ui| {
                ui.label("This removes only the selected local artwork record. ORIGIN is protected; the iPhone is not changed.");
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() { self.pending_delete_version = None; }
                    if ui.add(egui::Button::new("Delete selected record").fill(md3::ERROR_CONTAINER)).clicked() && !self.is_busy {
                        let result = match selection {
                            SelectedArtwork::Origin => Err(anyhow::anyhow!("ORIGIN cannot be deleted")),
                            SelectedArtwork::Legacy => card_history::delete_legacy_version(&udid, &hash),
                            SelectedArtwork::Version(id) => card_history::delete_version(&udid, &hash, &id),
                        };
                        match result {
                            Ok(()) => { self.selected_artwork = None; self.pending_delete_version = None; self.reload_local_records(); }
                            Err(err) => self.status_msg = format!("Delete artwork failed: {err:#}"),
                        }
                    }
                });
            });
        }
    }

    fn show_wallet_tab(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let language = self.language;
        if self.scanning_syslog {
            egui::Frame::new()
                .fill(md3::TERTIARY_CONTAINER)
                .corner_radius(16)
                .inner_margin(egui::Margin::same(16))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new(language.text("Scanning syslog...")).strong().size(13.0).color(md3::ON_TERTIARY_CONTAINER));
                            ui.label(egui::RichText::new(language.text("Open Wallet on iPhone and tap your card")).size(11.5).color(md3::ON_TERTIARY_CONTAINER));
                        });
                    });
                });
            ui.add_space(8.0);
        }

        ui.horizontal_top(|ui| {
            let total_width = ui.available_width();
            let left_width = (total_width * 0.37).clamp(340.0, 520.0);
            let right_width = (total_width - left_width - 18.0).max(320.0);
            ui.allocate_ui_with_layout(egui::vec2(left_width, 0.0), egui::Layout::top_down(egui::Align::Min), |left| {
            m3_card(left, |ui| {
                ui.label(egui::RichText::new("ARTWORK / EDITOR").monospace().size(10.0).color(md3::ON_SURFACE_VARIANT));
                ui.add_space(3.0);
                ui.label(egui::RichText::new(language.text("Artwork")).strong().size(24.0).color(md3::ON_SURFACE));
                ui.painter().rect_filled(egui::Rect::from_min_size(ui.cursor().min, egui::vec2(46.0, 3.0)), 0.0, md3::SIGNAL);
                ui.add_space(7.0);
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Choose a card on Cards, then prepare its new artwork.").size(12.0).color(md3::ON_SURFACE_VARIANT));
                ui.add_space(16.0);

                // Target Card Hash
                ui.label(egui::RichText::new(language.text("Target Card Hash")).monospace().strong().size(11.0).color(md3::ON_SURFACE));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.card_hash)
                        .hint_text(language.text("Base64 pass hash..."))
                        .background_color(md3::SURFACE_CONTAINER_HIGH)
                        .desired_width(ui.available_width()));
                });

                ui.add_space(16.0);

                // Card Skin
                ui.label(egui::RichText::new(language.text("Artwork")).monospace().strong().size(11.0).color(md3::ON_SURFACE));
                ui.label(egui::RichText::new(language.text("PNG, JPG, WebP - auto-scaled to 1536x969")).size(12.0).color(md3::ON_SURFACE_VARIANT));
                ui.label(egui::RichText::new(language.text("Drag inside the preview to reposition the crop.")).size(12.0).color(md3::ON_SURFACE_VARIANT));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if m3_button_filled(ui, language.text("Choose Image...")) { self.select_skin(ctx); }
                    if self.skin.is_some() {
                        if m3_button_tonal(ui, language.text("Export PNG")) { self.save_prepared_png(); }
                    }
                });

                if let Some(skin) = &self.skin {
                    ui.add_space(4.0);
                    let fname = self.source_path.as_ref()
                        .and_then(|p| p.file_name()).and_then(|n| n.to_str()).unwrap_or("image");
                    ui.label(egui::RichText::new(format!("{} - 1536x969 - {:.0} KB", fname, skin.png.len() as f32 / 1024.0)).size(11.0).color(md3::PRIMARY));
                }

                ui.add_space(16.0);

                // Apply
                ui.label(egui::RichText::new(language.text("Write to iPhone")).monospace().strong().size(11.0).color(md3::ON_SURFACE));
                ui.add_space(4.0);

                let can_flash = !self.is_busy
                    && self.selected_transport_available()
                    && !self.card_hash.trim().is_empty()
                    && self.skin.is_some();
                let flash_btn = egui::Button::new(
                    egui::RichText::new(language.text("Apply Card Skin")).strong().size(14.0)
                        .color(if can_flash { md3::ON_PRIMARY } else { md3::ON_SURFACE_VARIANT }),
                )
                .fill(if can_flash { md3::PRIMARY } else { md3::SURFACE_CONTAINER_HIGH })
                .corner_radius(2).stroke(egui::Stroke::NONE)
                .min_size(egui::vec2(ui.available_width(), 40.0));

                let resp = ui.add_enabled(can_flash, flash_btn);
                if can_flash {
                    ui.painter().text(resp.rect.right_center() - egui::vec2(20.0, 0.0), egui::Align2::CENTER_CENTER,
                        "→", egui::FontId::proportional(19.0), md3::SIGNAL);
                }
                if resp.clicked() { self.flash_card(); }
                if !can_flash {
                    let mut r = Vec::new();
                    if self.selected_udid.is_none() { r.push(language.text("connect iPhone")); }
                    else if !self.selected_transport_available() { r.push(language.text("choose available transport")); }
                    if self.card_hash.trim().is_empty() { r.push(language.text("enter card hash")); }
                    if self.skin.is_none() { r.push(language.text("choose image")); }
                    if !r.is_empty() { resp.on_disabled_hover_text(format!("{}{}", language.text("Need: "), r.join(", "))); }
                }

                if self.is_busy {
                    ui.add_space(8.0);
                    if self.progress_total > 0 {
                        ui.add(egui::ProgressBar::new(self.progress_step as f32 / self.progress_total as f32).animate(true));
                    }
                    ui.label(egui::RichText::new(&self.progress_msg).size(11.0).color(md3::PRIMARY));
                }
            });
            });

            // Right: preview
            ui.add_space(10.0);
            ui.allocate_ui_with_layout(egui::vec2(right_width, 0.0), egui::Layout::top_down(egui::Align::Min), |right| {
            m3_card(right, |ui| {
                ui.label(egui::RichText::new("03  /  ARTWORK MONITOR").monospace().size(10.0).color(md3::ON_SURFACE_VARIANT));
                ui.add_space(3.0);
                ui.label(egui::RichText::new(language.text("Preview")).strong().size(24.0).color(md3::ON_SURFACE));
                ui.painter().rect_filled(egui::Rect::from_min_size(ui.cursor().min, egui::vec2(46.0, 3.0)), 0.0, md3::SIGNAL);
                ui.add_space(7.0);
                ui.add_space(4.0);
                ui.label(egui::RichText::new(language.text("1536 x 969 px pass canvas")).size(12.0).color(md3::ON_SURFACE_VARIANT));
                ui.add_space(12.0);

                let pass_w = (ui.available_width() - 8.0).clamp(250.0, 600.0);
                let pass_h = pass_w * (969.0 / 1536.0);
                ui.vertical_centered(|ui| {
                    let (rect, response) =
                        ui.allocate_exact_size(egui::vec2(pass_w, pass_h), egui::Sense::drag());
                    let source_dimensions = self
                        .source_image
                        .as_ref()
                        .map(|image| (image.width(), image.height()));
                    if response.dragged() {
                        if let Some((source_width, source_height)) = source_dimensions {
                            let crop_uv = crop_uv_for_card(
                                source_width,
                                source_height,
                                self.crop_focus[0],
                                self.crop_focus[1],
                            );
                            let visible_width = crop_uv[2] - crop_uv[0];
                            let visible_height = crop_uv[3] - crop_uv[1];
                            if rect.width() > 0.0 {
                                self.crop_focus[0] = (self.crop_focus[0]
                                    - response.drag_motion().x / rect.width()
                                        * (1.0 - visible_width))
                                    .clamp(0.0, 1.0);
                            }
                            if rect.height() > 0.0 {
                                self.crop_focus[1] = (self.crop_focus[1]
                                    - response.drag_motion().y / rect.height()
                                        * (1.0 - visible_height))
                                    .clamp(0.0, 1.0);
                            }
                            self.crop_dirty = true;
                            ctx.request_repaint();
                        }
                    }
                    if response.drag_stopped() && self.crop_dirty {
                        self.rebuild_skin_from_source();
                    }

                    let painter = ui.painter();
                    if let (Some(tex), Some((source_width, source_height))) =
                        (self.source_texture.as_ref(), source_dimensions)
                    {
                        let crop_uv = crop_uv_for_card(
                            source_width,
                            source_height,
                            self.crop_focus[0],
                            self.crop_focus[1],
                        );
                        painter.image(tex.id(), rect,
                            egui::Rect::from_min_max(
                                egui::pos2(crop_uv[0], crop_uv[1]),
                                egui::pos2(crop_uv[2], crop_uv[3]),
                            ),
                            egui::Color32::WHITE);
                        painter.rect_stroke(rect, 2.0,
                            egui::Stroke::new(2.0_f32, md3::SIGNAL),
                            egui::StrokeKind::Inside);
                    } else {
                        painter.rect_filled(rect, 2.0, md3::PRIMARY);
                        let grid = egui::Stroke::new(0.5_f32, egui::Color32::from_rgba_unmultiplied(171, 172, 148, 28));
                        let mut x = rect.left() + 36.0;
                        while x < rect.right() {
                            painter.vline(x, rect.y_range(), grid);
                            x += 36.0;
                        }
                        let mut y = rect.top() + 36.0;
                        while y < rect.bottom() {
                            painter.hline(rect.x_range(), y, grid);
                            y += 36.0;
                        }
                        for radius in [52.0_f32, 78.0_f32, 104.0_f32] {
                            painter.circle_stroke(rect.center(), radius, egui::Stroke::new(1.0_f32, egui::Color32::from_rgba_unmultiplied(231, 165, 57, 44)));
                        }
                        painter.line_segment(
                            [rect.center() - egui::vec2(118.0, 0.0), rect.center() + egui::vec2(118.0, 0.0)],
                            egui::Stroke::new(1.0_f32, egui::Color32::from_rgba_unmultiplied(231, 165, 57, 72)),
                        );
                        painter.text(rect.left_top() + egui::vec2(16.0, 16.0), egui::Align2::LEFT_TOP,
                            "AC  /  IMAGE CHANNEL", egui::FontId::monospace(10.0), md3::SIGNAL);
                        painter.text(rect.center(), egui::Align2::CENTER_CENTER,
                            language.text("No artwork loaded"), egui::FontId::proportional(14.0), md3::ON_PRIMARY);
                        painter.rect_stroke(rect, 2.0, egui::Stroke::new(1.0_f32, md3::OUTLINE), egui::StrokeKind::Inside);
                    }
                });

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("1536x969").size(11.0).color(md3::ON_SURFACE_VARIANT));
                    ui.label(egui::RichText::new("|").size(11.0).color(md3::OUTLINE_VARIANT));
                    ui.label(egui::RichText::new("1.585 ratio").size(11.0).color(md3::ON_SURFACE_VARIANT));
                    ui.label(egui::RichText::new("|").size(11.0).color(md3::OUTLINE_VARIANT));
                    if self.skin.is_some() {
                        ui.label(egui::RichText::new(language.text("Ready")).size(11.0).color(md3::SUCCESS));
                    } else {
                        ui.label(egui::RichText::new(language.text("No image")).size(11.0).color(md3::ON_SURFACE_VARIANT));
                    }
                });
                ui.add_space(8.0);
                ui.label(egui::RichText::new(language.text("After applying, force close Apple Wallet and reopen it.")).size(11.0).color(md3::ON_SURFACE_VARIANT));
            });
            });
        });
    }

}
