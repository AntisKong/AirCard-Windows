use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::thread;

use eframe::egui;
use image::DynamicImage;

use crate::apple;
use crate::corefp::SessionGuard;
use crate::card_history::{self, CardRecord, PhoneRecord};
use crate::device::{ConnectionMode, DeviceInfo, DeviceTransport, list_connected_devices, query_usbmux_devices};
use crate::flasher::{flash_wallet_skin, restore_wallet_original, WALLET_WRITE_PHASES};
use crate::image_skin::{PreparedSkin, crop_uv_for_card, png_to_pdf};
use crate::i18n::Language;
use crate::scanner::scan_syslog_for_cards;
use crate::wallet_backup::{backup_exists, capture_original_card, changed_preview_path, original_preview_path, read_current_card_face};

enum BackgroundTaskMessage {
    Progress { step: usize, total: usize, message: String },
    Log(String),
    Done(Result<String, String>),
}

enum ScanMessage {
    Log(String),
    CardFound { hash: String, name: String },
}

enum LiveFaceMessage {
    Log(String),
    Preview(egui::ColorImage),
    Done(Result<bool, String>),
}

struct LoadedSkin {
    path: PathBuf,
    image: DynamicImage,
    skin: PreparedSkin,
    preview: egui::ColorImage,
}

enum ImageLoadMessage {
    Selected(PathBuf),
    Done(Result<Option<LoadedSkin>, String>),
}

struct PendingFlash {
    udid: String,
    hash: String,
    name: String,
    mode: ConnectionMode,
    skin: PreparedSkin,
}

#[derive(Clone)]
enum RenameTarget { Card(String, String), Phone(String) }

fn local_phone_name<'a>(phones: &'a [PhoneRecord], udid: &str, fallback: &'a str) -> &'a str {
    let phone = phones.iter().find(|phone| phone.udid.eq_ignore_ascii_case(udid));
    if let Some(alias) = phone.map(|phone| phone.name.trim()).filter(|name| !name.is_empty()) { return alias; }
    if !fallback.trim().is_empty() { return fallback; }
    phone.map(|phone| phone.device_name.trim()).unwrap_or("")
}

fn archived_phone_name(phones: &[PhoneRecord], udid: &str) -> String {
    let name = local_phone_name(phones, udid, "");
    if name.is_empty() { format!("iPhone · {}", &udid[udid.len().saturating_sub(6)..]) } else { name.to_string() }
}

fn refresh_rotation(progress: f32) -> f32 {
    let t = progress.clamp(0.0, 1.0);
    let eased = t * t * t * (t * (t * 6.0 - 15.0) + 10.0);
    -std::f32::consts::TAU * eased
}

fn apply_available(operation_busy: bool, preview_busy: bool, queued: bool) -> bool {
    (!operation_busy || preview_busy) && !queued
}

fn scan_follows_target(page: Page, operation_busy: bool, preview_busy: bool, queued: bool) -> bool {
    page == Page::Artwork && (!operation_busy || preview_busy) && !queued
}

fn smooth_progress(current: f32, target: f32, dt: f32) -> f32 {
    let target = target.clamp(0.0, 1.0);
    if target < current { return target; }
    let next = current + (target - current) * (1.0 - (-12.0 * dt.clamp(0.0, 0.1)).exp());
    if target - next < 0.0001 { target } else { next }
}

fn history_tile_frame(active: bool) -> egui::Frame {
    egui::Frame::new()
        .fill(if active { md3::PRIMARY_CONTAINER } else { md3::SURFACE_CONTAINER_HIGH })
        .stroke(egui::Stroke::new(2.0_f32, if active { md3::SIGNAL } else { md3::OUTLINE_VARIANT }))
        .inner_margin(egui::Margin::same(10)).corner_radius(2)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DialogAction { Pending, Cancel, Confirm }

struct DialogMotion {
    key: &'static str,
    started: f64,
    closing: Option<(f64, f32, DialogAction)>,
}

fn dialog_ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn dialog_button(ui: &mut egui::Ui, label: &str, primary: bool, destructive: bool, enabled: bool) -> bool {
    let fill = if destructive { md3::ERROR } else if primary { md3::PRIMARY } else { egui::Color32::TRANSPARENT };
    let text = if primary || destructive { md3::ON_PRIMARY } else { md3::ON_SURFACE };
    ui.add_enabled(enabled, egui::Button::new(egui::RichText::new(label).size(13.0).color(text))
        .min_size(egui::vec2(112.0, 40.0)).corner_radius(2).fill(fill)
        .stroke(egui::Stroke::new(1.0_f32, if primary || destructive { fill } else { md3::OUTLINE_VARIANT }))).clicked()
}

fn archive_dialog(
    ctx: &egui::Context,
    motion: &mut Option<DialogMotion>,
    key: &'static str,
    title: &str,
    destructive: bool,
    content: impl FnOnce(&mut egui::Ui, bool) -> DialogAction,
) -> DialogAction {
    let now = ctx.input(|input| input.time);
    let first = motion.as_ref().is_none_or(|state| state.key != key);
    if first { *motion = Some(DialogMotion { key, started: now, closing: None }); }
    let state = motion.as_mut().unwrap();
    let opening = dialog_ease(((now - state.started) / 0.3) as f32);
    let opacity = state.closing.map_or(opening, |(at, from, _)| from * (1.0 - dialog_ease(((now - at) / 0.2) as f32)));
    let offset = state.closing.map_or(12.0 * (1.0 - opening), |(at, from, _)| {
        let start = 12.0 * (1.0 - from);
        start + (8.0 - start) * dialog_ease(((now - at) / 0.2) as f32)
    });
    let closing = state.closing.is_some();
    let accent = if destructive { md3::ERROR } else { md3::SIGNAL };
    let frame = egui::Frame::new()
        .fill(egui::Color32::from_rgba_unmultiplied(251, 250, 246, (236.0 * opacity) as u8))
        .stroke(egui::Stroke::new(1.0_f32, egui::Color32::from_white_alpha((190.0 * opacity) as u8)))
        .corner_radius(4).inner_margin(egui::Margin::same(30))
        .shadow(egui::epaint::Shadow { offset: [0, 12], blur: 40, spread: 0,
            color: egui::Color32::from_black_alpha((32.0 * opacity) as u8) });
    let response = egui::Modal::new(egui::Id::new(key))
        .area(egui::Modal::default_area(egui::Id::new(key)).fade_in(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, offset)))
        .backdrop_color(egui::Color32::from_rgba_unmultiplied(31, 37, 37, (55.0 * opacity) as u8))
        .frame(frame).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 100.0).clamp(240.0, 480.0));
            ui.set_min_height(220.0);
            ui.multiply_opacity(opacity);
            if closing { ui.disable(); }
            let mut action = DialogAction::Pending;
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("ARCHIVE / CONTROL").monospace().size(10.0).color(md3::ON_SURFACE_VARIANT));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (rect, response) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
                    let inset = rect.shrink(6.0);
                    let stroke = egui::Stroke::new(1.2_f32, md3::ON_SURFACE_VARIANT);
                    ui.painter().line_segment([inset.left_top(), inset.right_bottom()], stroke);
                    ui.painter().line_segment([inset.right_top(), inset.left_bottom()], stroke);
                    if response.clicked() { action = DialogAction::Cancel; }
                });
            });
            ui.add_space(12.0);
            ui.label(egui::RichText::new(title).size(28.0).color(md3::ON_SURFACE));
            ui.add_space(8.0);
            let (line, _) = ui.allocate_exact_size(egui::vec2(40.0, 2.0), egui::Sense::hover());
            ui.painter().rect_filled(line, 0.0, accent);
            ui.add_space(20.0);
            let inner_action = content(ui, first);
            if action == DialogAction::Pending { action = inner_action; }
            action
        });
    if !closing {
        let action = if response.should_close() { DialogAction::Cancel } else { response.inner };
        if action != DialogAction::Pending { state.closing = Some((now, opacity, action)); }
    } else {
        ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
    }
    if let Some((at, _, action)) = state.closing {
        if now - at >= 0.2 { *motion = None; ctx.request_repaint(); return action; }
    }
    if opacity < 1.0 || state.closing.is_some() { ctx.request_repaint(); }
    DialogAction::Pending
}

fn card_list_row(ui: &mut egui::Ui, name: &str, hash: &str, selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 38.0), egui::Sense::click());
    let fill = if selected { md3::PRIMARY_CONTAINER } else { md3::SURFACE_CONTAINER_HIGH };
    ui.painter().rect_filled(rect, 2.0, fill);
    if response.hovered() {
        ui.painter().rect_stroke(rect, 2.0, egui::Stroke::new(1.0_f32, md3::OUTLINE), egui::StrokeKind::Inside);
    }
    let short_hash = format!("{}…", hash.chars().take(8).collect::<String>());
    let hash_galley = ui.painter().layout_no_wrap(short_hash, egui::FontId::monospace(11.0), md3::ON_SURFACE_VARIANT);
    let hash_pos = egui::pos2(rect.right() - 12.0 - hash_galley.size().x, rect.center().y - hash_galley.size().y * 0.5);
    let name_width = (hash_pos.x - rect.left() - 28.0).max(0.0);
    let name_galley = egui::WidgetText::from(egui::RichText::new(name).size(14.0).color(md3::ON_SURFACE))
        .into_galley(ui, Some(egui::TextWrapMode::Truncate), name_width, egui::FontId::proportional(14.0));
    ui.painter().galley(egui::pos2(rect.left() + 12.0, rect.center().y - name_galley.size().y * 0.5), name_galley, md3::ON_SURFACE);
    ui.painter().galley(hash_pos, hash_galley, md3::ON_SURFACE_VARIANT);
    response.on_hover_text(format!("{name}\n{hash}\nDouble-click to rename"))
}

fn prepare_loaded_skin(path: PathBuf, image: DynamicImage) -> anyhow::Result<LoadedSkin> {
    let skin = PreparedSkin::from_image_with_focus(image.clone(), 0.5, 0.5)?;
    let rgba = image.thumbnail(image.width().min(2048), image.height().min(2048)).to_rgba8();
    let preview = egui::ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize], rgba.as_raw());
    Ok(LoadedSkin { path, image, skin, preview })
}

fn face_cache_key(target: &(String, String)) -> (String, String) {
    (target.0.to_ascii_lowercase(), target.1.trim().trim_end_matches('=').replace('-', "+").replace('_', "/"))
}

fn same_card_hash(left: &str, right: &str) -> bool {
    let canonical = |byte| match byte { b'-' => b'+', b'_' => b'/', other => other };
    left.trim().trim_end_matches('=').bytes().map(canonical)
        .eq(right.trim().trim_end_matches('=').bytes().map(canonical))
}

fn known_card_name<'a>(records: &'a [CardRecord], udid: Option<&str>, hash: &str) -> Option<&'a str> {
    let udid = udid?;
    if hash.trim().is_empty() { return None; }
    records.iter().find(|record| record.udid.eq_ignore_ascii_case(udid)
        && (same_card_hash(&record.hash, hash) || (!record.device_hash.is_empty() && same_card_hash(&record.device_hash, hash))))
        .map(|record| record.name.trim()).filter(|name| !name.is_empty())
}

fn update_scanned_target(target: &mut String, target_name: &mut String, hash: String, name: String) -> bool {
    let changed = !same_card_hash(target, &hash);
    *target = hash;
    *target_name = name;
    changed
}

#[cfg(test)]
mod target_tests {
    use super::*;

    fn card(udid: &str, hash: &str, name: &str) -> CardRecord {
        CardRecord { udid: udid.into(), hash: hash.into(), device_hash: String::new(),
            name: name.into(), changed_at: 0, original_backed_up: false,
            baseline_from_prior_change: false, versions: Vec::new() }
    }

    #[test]
    fn scanning_replaces_a_nonempty_target() {
        let mut hash = "old=".to_string();
        let mut name = "Old card".to_string();
        assert!(update_scanned_target(&mut hash, &mut name, "new=".into(), "New card".into()));
        assert_eq!(hash, "new=");
        assert_eq!(name, "New card");
    }

    #[test]
    fn continuous_discovery_preserves_history_and_fixed_write_targets() {
        assert!(scan_follows_target(Page::Artwork, false, false, false));
        assert!(scan_follows_target(Page::Artwork, true, true, false));
        assert!(!scan_follows_target(Page::Cards, false, false, false));
        assert!(!scan_follows_target(Page::Artwork, true, false, false));
        assert!(!scan_follows_target(Page::Artwork, true, true, true));
    }

    #[test]
    fn duplicate_scans_do_not_restart_a_read() {
        let mut hash = "abc-=".to_string();
        let mut name = String::new();
        assert!(!update_scanned_target(&mut hash, &mut name, "abc+".into(), "Card".into()));
        assert_eq!(name, "Card");
    }

    #[test]
    fn card_names_are_scoped_to_the_selected_phone() {
        let records = vec![card("phone-a", "abc-=", "深圳通"), card("phone-b", "abc-=", "Other")];
        assert_eq!(known_card_name(&records, Some("phone-a"), "abc+"), Some("深圳通"));
        assert_eq!(known_card_name(&records, Some("phone-b"), "abc-="), Some("Other"));
        assert_eq!(known_card_name(&records, Some("phone-a"), "unknown"), None);
        assert_eq!(known_card_name(&records, None, "abc-="), None);
    }

    #[test]
    fn phone_alias_is_bound_to_udid_not_system_name_or_transport() {
        let phones = vec![PhoneRecord { udid: "PHONE-A".into(), name: " My phone ".into(), device_name: "Vina".into() },
            PhoneRecord { udid: "phone-b".into(), name: String::new(), device_name: "Vina".into() }];
        assert_eq!(local_phone_name(&phones, "phone-a", "Vina"), "My phone");
        assert_eq!(local_phone_name(&phones, "phone-a", "Renamed by iPhone"), "My phone");
        assert_eq!(local_phone_name(&phones, "phone-b", "Vina"), "Vina");
        assert_eq!(local_phone_name(&phones, "phone-c", "Vina"), "Vina");
        assert_eq!(archived_phone_name(&phones, "phone-a"), "My phone");
        assert_eq!(archived_phone_name(&phones, "PHONE-B"), "Vina");
        assert_eq!(local_phone_name(&phones, "phone-b", "New name"), "New name");
    }

    #[test]
    fn language_menu_text_is_centered_in_equal_height_rows() {
        let context = egui::Context::default();
        let output = context.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.set_width(94.0);
                picker_option_aligned(ui, "English", true, true);
                picker_option_aligned(ui, "Chinese", false, true);
            });
        });
        let rows: Vec<_> = output.shapes.iter().filter_map(|shape| match &shape.shape {
            egui::Shape::Rect(rect) if rect.rect.height() == 34.0 => Some(rect.rect),
            _ => None,
        }).collect();
        let labels: Vec<_> = output.shapes.iter().filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) => Some(egui::Rect::from_min_size(text.pos, text.galley.size())),
            _ => None,
        }).collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(labels.len(), 2);
        for (row, text) in rows.iter().zip(&labels) {
            assert!((row.center() - text.center()).length() < 0.01);
        }
    }

    #[test]
    fn history_delete_requires_a_selected_non_origin_record() {
        assert!(!can_delete_artwork(None, false, false));
        assert!(!can_delete_artwork(Some(&SelectedArtwork::Origin), false, false));
        assert!(can_delete_artwork(Some(&SelectedArtwork::Legacy), false, false));
        let version = SelectedArtwork::Version("version-id".into());
        assert!(can_delete_artwork(Some(&version), false, false));
        assert!(!can_delete_artwork(Some(&version), true, false));
        assert!(can_delete_artwork(Some(&version), false, true));
    }

    #[test]
    fn device_hash_alias_resolves_the_saved_card_name() {
        let mut record = card("phone", "stored", "Transit");
        record.device_hash = "device=".into();
        assert_eq!(known_card_name(&[record], Some("phone"), "device"), Some("Transit"));
    }

    #[test]
    fn memory_preview_keys_normalize_transport_independent_card_identity() {
        assert_eq!(face_cache_key(&("PHONE".into(), " ab-_== ".into())),
            face_cache_key(&("phone".into(), "ab+/".into())));
        assert_ne!(face_cache_key(&("phone-a".into(), "ab=".into())),
            face_cache_key(&("phone-b".into(), "ab=".into())));
    }

    #[test]
    fn image_preparation_runs_without_ui_or_file_output() {
        let result = thread::spawn(|| prepare_loaded_skin(PathBuf::from("image.png"),
            DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(32, 20, image::Rgba([80, 90, 100, 255])))))
            .join().unwrap().unwrap();
        assert_eq!(result.image.width(), 32);
        assert_eq!(result.preview.size, [32, 20]);
        assert_eq!(result.skin.source_width, 32);
        let prepared = image::load_from_memory(&result.skin.png).unwrap();
        assert_eq!((prepared.width(), prepared.height()), (1536, 969));
        assert!(result.skin.pdf.starts_with(b"%PDF"));
    }

    #[test]
    fn apply_is_available_during_preview_but_not_during_write_or_after_queueing() {
        assert!(apply_available(true, true, false));
        assert!(apply_available(false, false, false));
        assert!(!apply_available(true, false, false));
        assert!(!apply_available(true, true, true));
    }

    #[test]
    fn refresh_animation_completes_one_counterclockwise_turn_with_easing() {
        assert_eq!(refresh_rotation(0.0), 0.0);
        assert_eq!(refresh_rotation(1.0), -std::f32::consts::TAU);
        assert_eq!(refresh_rotation(2.0), -std::f32::consts::TAU);
        assert!(refresh_rotation(0.25) < 0.0);
        assert!(refresh_rotation(0.25).abs() < std::f32::consts::TAU * 0.25);
        assert!((refresh_rotation(0.5) + std::f32::consts::PI).abs() < 0.00001);
    }

    #[test]
    fn progress_eases_without_exceeding_completed_work() {
        let first = smooth_progress(0.0, 0.6, 1.0 / 60.0);
        let second = smooth_progress(first, 0.6, 1.0 / 60.0);
        assert!(first > 0.0 && second > first && second < 0.6);
        assert!(second - first < first);
        let mut current = second;
        for _ in 0..120 { current = smooth_progress(current, 0.6, 1.0 / 60.0); }
        assert_eq!(current, 0.6);
        assert_eq!(smooth_progress(current, 0.0, 0.016), 0.0);
        assert_eq!(smooth_progress(0.2, 0.8, 0.0), 0.2);
    }

    #[test]
    fn selecting_history_tile_does_not_change_geometry() {
        let context = egui::Context::default();
        let mut sizes = Vec::new();
        let _ = context.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                for active in [false, true] {
                    sizes.push(history_tile_frame(active).show(ui, |ui| {
                        ui.set_width(196.0);
                        ui.label("00 / ORIGIN");
                        ui.add_space(7.0);
                        ui.allocate_exact_size(egui::vec2(176.0, 111.0), egui::Sense::hover());
                    }).response.rect.size());
                }
            });
        });
        assert_eq!(sizes[0], sizes[1]);
    }

    #[test]
    fn card_hashes_align_right_regardless_of_name_length() {
        let context = egui::Context::default();
        let mut sizes = Vec::new();
        let output = context.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.set_width(310.0);
                for name in ["Card", "A much longer card name", "A card name that is too long to fit the space available in one row"] {
                    sizes.push(card_list_row(ui, name, "12345678abcdefghijkl", false).rect.size());
                }
            });
        });
        let right_edges: Vec<f32> = output.shapes.iter().filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == "12345678…" => Some(text.pos.x + text.galley.size().x),
            _ => None,
        }).collect();
        assert_eq!(right_edges.len(), 3);
        assert!(right_edges.iter().all(|right| (*right - right_edges[0]).abs() < 0.001));
        assert!(sizes.iter().all(|size| *size == sizes[0] && size.y == 38.0));
    }

    #[test]
    fn modal_escape_cancels_after_exit_animation() {
        let context = egui::Context::default();
        let mut motion = None;
        let mut action = DialogAction::Pending;
        for (time, escape) in [(0.0, false), (0.35, false), (0.4, true), (0.5, false), (0.65, false)] {
            let mut input = egui::RawInput { time: Some(time), ..Default::default() };
            if escape {
                input.events.push(egui::Event::Key { key: egui::Key::Escape, physical_key: None,
                    pressed: true, repeat: false, modifiers: egui::Modifiers::NONE });
            }
            let _ = context.run(input, |ctx| {
                action = archive_dialog(ctx, &mut motion, "escape-test", "Rename card", false,
                    |ui, _| { ui.label("Test content"); DialogAction::Pending });
            });
            if time < 0.6 { assert_eq!(action, DialogAction::Pending); }
        }
        assert_eq!(action, DialogAction::Cancel);
        assert!(motion.is_none());
    }

    #[test]
    fn dialog_easing_stays_bounded_and_soft_at_endpoints() {
        assert_eq!(dialog_ease(-1.0), 0.0);
        assert_eq!(dialog_ease(2.0), 1.0);
        assert_eq!(dialog_ease(0.5), 0.5);
        assert!(dialog_ease(0.1) < 0.1);
        assert!(dialog_ease(0.9) > 0.9);
    }
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
    corefp_session: Option<SessionGuard>,
    corefp_rx: Option<Receiver<Result<SessionGuard, String>>>,
    corefp_attempted: bool,
    live_face_rx: Option<Receiver<LiveFaceMessage>>,
    live_face_stop: Option<Arc<AtomicBool>>,
    pending_flash: Option<PendingFlash>,
    refresh_started_at: Option<f64>,
    refresh_rx: Option<Receiver<Result<Vec<DeviceInfo>, String>>>,
    live_face_target: Option<(String, String)>,
    live_face_texture: Option<egui::TextureHandle>,
    live_face_cache: VecDeque<((String, String), egui::TextureHandle)>,
    image_load_rx: Option<Receiver<ImageLoadMessage>>,
    show_live_face: bool,
    card_changed_at: std::time::Instant,
    device_poll_at: std::time::Instant,

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
    rename_target: Option<RenameTarget>,
    rename_text: String,
    dialog_motion: Option<DialogMotion>,
    auto_scan_for: Option<(String, ConnectionMode)>,
    scan_rx: Option<Receiver<ScanMessage>>,
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
    progress_display: f32,
    progress_total: usize,
    progress_msg: String,
    status_msg: String,
    task_rx: Option<Receiver<BackgroundTaskMessage>>,
    logs: Vec<String>,
    show_logs_window: bool,
    logs_copied_at: Option<f64>,
    logs_follow: bool,
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
            corefp_session: None,
            corefp_rx: None,
            corefp_attempted: false,
            live_face_rx: None,
            live_face_stop: None,
            pending_flash: None,
            refresh_started_at: None,
            refresh_rx: None,
            live_face_target: None,
            live_face_texture: None,
            live_face_cache: VecDeque::new(),
            image_load_rx: None,
            show_live_face: true,
            card_changed_at: std::time::Instant::now(),
            device_poll_at: std::time::Instant::now(),

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
            rename_target: None,
            rename_text: String::new(),
            dialog_motion: None,
            auto_scan_for: None,
            scan_rx: None,
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
            progress_display: 0.0,
            progress_total: 0,
            progress_msg: String::new(),
            status_msg: language
                .text("Ready. Connect iPhone via USB or paired WiFi and unlock it.")
                .to_string(),
            task_rx: None,
            logs: Vec::new(),
            show_logs_window: false,
            logs_copied_at: None,
            logs_follow: true,
        };

        app.add_log("AirCard initialized");
        app.add_log(format!("Apple Support Runtime: {apple_status}"));
        app.add_log(format!("Loaded {} changed card(s) from portable history", app.card_records.len()));

        if app.apple_ready {
            app.refresh_devices();
        }

        app
    }

    fn ensure_corefp_session(&mut self) {
        if self.devices.is_empty() || self.corefp_session.is_some() || self.corefp_rx.is_some() || self.corefp_attempted {
            return;
        }
        self.corefp_attempted = true;
        let (tx, rx) = channel();
        self.corefp_rx = Some(rx);
        self.add_log("Preparing CoreFP for this AirCard session...");
        thread::spawn(move || {
            let result = SessionGuard::begin().map_err(|err| format!("{err:#}"));
            let _ = tx.send(result);
        });
    }

    fn poll_device_changes(&mut self) {
        if !self.apple_ready || self.is_busy || self.refresh_rx.is_some() || self.device_poll_at.elapsed().as_secs() < 3 { return; }
        self.device_poll_at = std::time::Instant::now();
        if let Ok(entries) = query_usbmux_devices() {
            let mut current: Vec<_> = entries.iter().map(|entry| format!("{}:{}", entry.udid, entry.transport.label())).collect();
            let mut previous: Vec<_> = self.devices.iter().flat_map(|device| device.transports.iter()
                .map(|transport| format!("{}:{}", device.udid, transport.label()))).collect();
            current.sort();
            previous.sort();
            if current != previous { self.refresh_devices(); }
        }
    }

    fn current_face_target(&self) -> Option<(String, String)> {
        let udid = self.selected_udid.as_ref()?;
        let hash = self.card_hash.trim();
        if !crate::scanner::is_valid_card_hash(hash) { return None; }
        Some((udid.clone(), hash.to_string()))
    }

    fn handle_session_messages(&mut self, ctx: &egui::Context) {
        let setup = self.corefp_rx.as_ref().and_then(|rx| match rx.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("CoreFP setup worker stopped unexpectedly".to_string())),
        });
        if let Some(result) = setup {
            self.corefp_rx = None;
            match result {
                Ok(guard) => {
                    self.add_log(if guard.is_temporary() {
                        "Temporary CoreFP LibraryPath is ready; it will be removed when AirCard exits."
                    } else {
                        "Existing CoreFP LibraryPath will be left unchanged."
                    });
                    self.corefp_session = Some(guard);
                }
                Err(err) => {
                    self.add_log(format!("CoreFP session setup failed: {err}"));
                    self.status_msg = format!("CoreFP: {err}");
                }
            }
        }
        let mut messages = Vec::new();
        if let Some(rx) = &self.live_face_rx {
            loop {
                match rx.try_recv() {
                    Ok(message) => {
                        let finished = matches!(message, LiveFaceMessage::Done(_));
                        messages.push(message);
                        if finished { break; }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        messages.push(LiveFaceMessage::Done(Err(
                            "Card read worker stopped unexpectedly; keep Data/recovery for device recovery".to_string(),
                        )));
                        break;
                    }
                }
            }
        }
        for message in messages {
            match message {
                LiveFaceMessage::Log(line) => self.add_log(line),
                LiveFaceMessage::Preview(image) => {
                    if self.current_face_target() == self.live_face_target {
                        self.live_face_texture = Some(ctx.load_texture("current-device-card", image, egui::TextureOptions::LINEAR));
                        self.status_msg = self.language.text("Card preview ready; restoring device file...").to_string();
                        self.progress_msg = self.status_msg.clone();
                        self.add_log("Card preview ready; device write-back and cleanup are still running.");
                    }
                }
                LiveFaceMessage::Done(result) => {
                    let safe_to_write = result.is_ok();
                    self.live_face_rx = None;
                    self.live_face_stop = None;
                    self.is_busy = false;
                    let still_selected = self.current_face_target() == self.live_face_target;
                    match result {
                        Ok(true) if still_selected => {
                            if let (Some(target), Some(texture)) = (&self.live_face_target, &self.live_face_texture) {
                                let key = face_cache_key(target);
                                self.live_face_cache.retain(|(cached, _)| cached != &key);
                                self.live_face_cache.push_back((key, texture.clone()));
                                while self.live_face_cache.len() > 8 { self.live_face_cache.pop_front(); }
                            }
                            self.add_log("Current card face loaded into memory; no card history was saved.");
                            self.status_msg = self.language.text("Current card face loaded.").to_string();
                        }
                        Ok(false) if still_selected && self.pending_flash.is_none() => {
                            self.live_face_texture = None;
                            self.status_msg = self.language.text("Current card artwork is unavailable.").to_string();
                            self.add_log("No readable current card artwork was found.");
                        }
                        Err(err) => {
                            if still_selected { self.live_face_texture = None; }
                            self.add_log(format!("Current card read failed: {err}"));
                            self.status_msg = format!("Current card read failed: {err}");
                        }
                        _ => {}
                    }
                    if let Some(request) = self.pending_flash.take() {
                        if safe_to_write {
                            self.start_flash(request);
                        } else {
                            self.add_log("Pending Apply cancelled because the preview read did not finish safely. Resolve the recovery error before writing.");
                        }
                    }
                }
            }
        }
    }

    fn maybe_read_current_face(&mut self) {
        if self.page != Page::Artwork || self.is_busy || self.corefp_session.is_none()
            || !self.selected_transport_available() || self.card_changed_at.elapsed().as_millis() < 400 {
            return;
        }
        let Some(target) = self.current_face_target() else { return };
        if self.live_face_target.as_ref() == Some(&target) { return; }
        self.live_face_target = Some(target.clone());
        let key = face_cache_key(&target);
        self.live_face_texture = self.live_face_cache.iter().find(|(cached, _)| cached == &key).map(|(_, texture)| texture.clone());
        self.is_busy = true;
        self.progress_total = 0;
        self.progress_msg = self.language.text("Reading current card artwork...").to_string();
        self.status_msg = self.progress_msg.clone();
        if self.live_face_texture.is_some() {
            self.status_msg = self.language.text("Cached preview; refreshing from iPhone...").to_string();
        }
        let (tx, rx) = channel();
        self.live_face_rx = Some(rx);
        let cancel = Arc::new(AtomicBool::new(false));
        self.live_face_stop = Some(cancel.clone());
        let mode = self.connection_mode;
        thread::spawn(move || {
            let result = read_current_card_face(&target.0, mode, &target.1,
                |line| { let _ = tx.send(LiveFaceMessage::Log(line.to_string())); },
                |bytes| {
                    if let Ok(image) = image::load_from_memory(bytes) {
                        let rgba = image.to_rgba8();
                        let image = egui::ColorImage::from_rgba_unmultiplied(
                            [rgba.width() as usize, rgba.height() as usize], rgba.as_raw());
                        let _ = tx.send(LiveFaceMessage::Preview(image));
                    }
                }, &cancel)
                .map(|bytes| bytes.is_some()).map_err(|err| format!("{err:#}"));
            let _ = tx.send(LiveFaceMessage::Done(result));
        });
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
        self.accept_devices(list_connected_devices().map_err(|err| format!("{err:#}")));
    }

    fn request_refresh(&mut self, ctx: &egui::Context) {
        if self.is_busy || self.refresh_rx.is_some() { return; }
        let (tx, rx) = channel();
        self.refresh_rx = Some(rx);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = tx.send(list_connected_devices().map_err(|err| format!("{err:#}")));
            ctx.request_repaint();
        });
    }

    fn handle_refresh_messages(&mut self) {
        let result = self.refresh_rx.as_ref().and_then(|rx| match rx.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("Device refresh worker stopped unexpectedly".into())),
        });
        if let Some(result) = result {
            self.refresh_rx = None;
            self.accept_devices(result);
        }
    }

    fn accept_devices(&mut self, result: Result<Vec<DeviceInfo>, String>) {
        if self.corefp_session.is_none() && self.corefp_rx.is_none() { self.corefp_attempted = false; }
        self.add_log("Scanning for connected iOS devices via usbmuxd...");
        match result {
            Ok(devs) => {
                self.devices = devs;
                self.remember_connected_phone_names();
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
                    self.ensure_corefp_session();
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
        if self.live_face_rx.is_some() {
            self.status_msg = self.language.text("Wait for the current card read to finish.").to_string();
            return false;
        }
        if operation != "Syslog scan" && self.corefp_session.is_none() {
            self.status_msg = self.language.text("CoreFP is not ready. Refresh devices and approve the permission prompt.").to_string();
            return false;
        }
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
        if self.image_load_rx.is_some() { return; }
        let (tx, rx) = channel();
        self.image_load_rx = Some(rx);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let result = (|| -> anyhow::Result<Option<LoadedSkin>> {
                let Some(path) = rfd::FileDialog::new()
                    .add_filter("Images", &["png", "jpg", "jpeg", "webp"]).pick_file() else {
                        return Ok(None);
                    };
                let _ = tx.send(ImageLoadMessage::Selected(path.clone()));
                ctx.request_repaint();
                let image = image::open(&path)?;
                prepare_loaded_skin(path, image).map(Some)
            })().map_err(|err| format!("{err:#}"));
            let _ = tx.send(ImageLoadMessage::Done(result));
            ctx.request_repaint();
        });
    }

    fn handle_image_load_messages(&mut self, ctx: &egui::Context) {
        let mut messages = Vec::new();
        if let Some(rx) = &self.image_load_rx {
            loop {
                match rx.try_recv() {
                    Ok(message) => {
                        let finished = matches!(message, ImageLoadMessage::Done(_));
                        messages.push(message);
                        if finished { break; }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        messages.push(ImageLoadMessage::Done(Err("Image worker stopped unexpectedly".to_string())));
                        break;
                    }
                }
            }
        }
        for message in messages {
            match message {
                ImageLoadMessage::Selected(path) => {
                    self.add_log(format!("Opening skin image: {}", path.display()));
                    self.status_msg = self.language.text("Preparing image...").to_string();
                }
                ImageLoadMessage::Done(result) => {
                    self.image_load_rx = None;
                    match result {
                        Ok(Some(LoadedSkin { path, image, skin, preview })) => {
                            self.source_texture = Some(ctx.load_texture("card-skin-source-preview", preview, egui::TextureOptions::LINEAR));
                            self.status_msg = format!("Prepared {} ({}x{} -> 1536x969 PNG, {:.1} KB)",
                                path.file_name().and_then(|n| n.to_str()).unwrap_or("image"),
                                skin.source_width, skin.source_height, skin.png.len() as f32 / 1024.0);
                            self.add_log(self.status_msg.clone());
                            self.source_path = Some(path);
                            self.source_image = Some(image);
                            self.show_live_face = false;
                            self.crop_focus = [0.5, 0.5];
                            self.crop_dirty = false;
                            self.skin = Some(skin);
                        }
                        Ok(None) => {}
                        Err(err) => {
                            self.add_log(format!("Image preparation failed: {err}"));
                            self.status_msg = format!("Could not prepare image: {err}");
                        }
                    }
                }
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

    fn maintain_syslog_scan(&mut self) {
        let target = self.selected_udid.clone().filter(|_| self.selected_transport_available())
            .map(|udid| (udid, self.connection_mode));
        if self.auto_scan_for == target { return; }
        if let Some(flag) = self.scan_stop_flag.take() { flag.store(true, Ordering::Relaxed); }
        self.scan_rx = None;
        self.scanning_syslog = false;
        self.auto_scan_for = target.clone();
        let Some((udid, connection_mode)) = target else { return; };
        let stop_flag = Arc::new(AtomicBool::new(false));
        self.scan_stop_flag = Some(Arc::clone(&stop_flag));
        self.scanning_syslog = true;
        self.add_log("Initiating syslog monitor session...");
        let (tx, rx) = channel();
        self.scan_rx = Some(rx);
        thread::spawn(move || {
            let mut retry_seconds = 1;
            while !stop_flag.load(Ordering::Relaxed) {
                let started = std::time::Instant::now();
                let tx_card = tx.clone();
                let tx_log = tx.clone();
                let result = scan_syslog_for_cards(Some(&udid), connection_mode, Arc::clone(&stop_flag),
                    move |hash, name| { let _ = tx_card.send(ScanMessage::CardFound { hash, name }); },
                    move |msg| { let _ = tx_log.send(ScanMessage::Log(msg)); });
                if stop_flag.load(Ordering::Relaxed) { break; }
                if started.elapsed().as_secs() >= 10 { retry_seconds = 1; }
                let reason = result.err().map(|err| format!("{err:#}")).unwrap_or_else(|| "device closed the relay".into());
                if tx.send(ScanMessage::Log(format!("Syslog connection ended ({reason}); reconnecting in {retry_seconds}s..."))).is_err() { break; }
                for _ in 0..retry_seconds * 10 {
                    if stop_flag.load(Ordering::Relaxed) { break; }
                    thread::sleep(std::time::Duration::from_millis(100));
                }
                retry_seconds = (retry_seconds * 2).min(8);
            }
        });
    }

    fn handle_scan_messages(&mut self) {
        let messages: Vec<_> = self.scan_rx.as_ref().map(|rx| rx.try_iter().collect()).unwrap_or_default();
        for message in messages {
            match message {
                ScanMessage::Log(message) => self.add_log(message),
                ScanMessage::CardFound { hash, name } => {
                    let display_name = known_card_name(&self.card_records, self.selected_udid.as_deref(), &hash)
                        .unwrap_or(&name).to_string();
                    let message = format!("{}: {} ({})", self.language.text("Card captured"), display_name, hash);
                    self.add_log(&message);
                    // Discovery never changes the historical selection or an in-flight write target.
                    if !scan_follows_target(self.page, self.is_busy, self.live_face_rx.is_some(), self.pending_flash.is_some()) { continue; }
                    if update_scanned_target(&mut self.card_hash, &mut self.scanned_name, hash, name) {
                        self.selected_artwork = None;
                        self.live_face_texture = None;
                        if self.live_face_rx.is_none() { self.live_face_target = None; }
                        self.show_live_face = true;
                        self.card_changed_at = std::time::Instant::now();
                    }
                    if !self.is_busy { self.status_msg = message; }
                }
            }
        }
    }

    fn flash_card(&mut self) {
        if !apply_available(self.is_busy, self.live_face_rx.is_some(), self.pending_flash.is_some()) {
            return;
        }
        if self.live_face_rx.is_none() && !self.validate_selected_transport("Card flash") {
            return;
        }
        if self.corefp_session.is_none() || !self.selected_transport_available() || self.image_load_rx.is_some() {
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

        let request = PendingFlash { udid, hash, mode: self.connection_mode, skin: skin.clone(),
            name: known_card_name(&self.card_records, self.selected_udid.as_deref(), &self.card_hash)
                .unwrap_or(&self.scanned_name).to_string() };
        if self.live_face_rx.is_some() {
            self.pending_flash = Some(request);
            if let Some(flag) = &self.live_face_stop { flag.store(true, Ordering::Relaxed); }
            self.status_msg = self.language.text("Apply accepted; finishing the current read safely...").to_string();
            self.progress_msg = self.status_msg.clone();
            self.add_log("Apply accepted with a fixed card and image; preview cancellation requested at the next safe boundary.");
            return;
        }
        self.start_flash(request);
    }

    fn start_flash(&mut self, request: PendingFlash) {
        let PendingFlash { udid, hash, name: card_name, mode: connection_mode, skin } = request;
        let png_bytes = skin.png;
        let pdf_bytes = skin.pdf;
        self.is_busy = true;
        self.progress_step = 0;
        self.progress_display = 0.0;
        self.progress_total = WALLET_WRITE_PHASES;
        self.progress_msg = "Initiating card flash...".to_string();
        self.status_msg = self.language.text("Writing card skin to iPhone...").to_string();
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
        let Some(udid) = self.selected_udid.clone() else { return };
        let hash = self.card_hash.trim().to_string();
        if !self.card_records.iter().any(|record| record.udid == udid && record.hash == hash) {
            self.status_msg = "Add this card record before capturing its face.".into();
            return;
        }
        let mode = self.connection_mode;
        self.is_busy = true;
        self.status_msg = "Capturing first readable card face; keep iPhone unlocked...".into();
        self.progress_total = 0;
        self.progress_msg = self.status_msg.clone();
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

        self.is_busy = true;
        self.progress_step = 0;
        self.progress_display = 0.0;
        self.progress_total = WALLET_WRITE_PHASES;
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
                    let msg_str = format!("[Phase {}/{}] {}", (step + 1).min(total), total, localized_message);
                    self.add_log(&msg_str);
                    self.status_msg = msg_str;
                }
                BackgroundTaskMessage::Log(log_line) => {
                    self.add_log(log_line);
                }
                BackgroundTaskMessage::Done(res) => {
                    self.is_busy = self.live_face_rx.is_some();
                    self.card_records = card_history::load();
                    self.remember_connected_phone_names();
                    self.record_preview_key.clear();
                    self.history_textures.clear();
                    finished = true;
                    match res {
                        Ok(ok_msg) => {
                            if self.progress_total == WALLET_WRITE_PHASES {
                                self.progress_step = WALLET_WRITE_PHASES;
                                self.add_log("[5/5] Device operations and local finalization completed.");
                            }
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

fn can_delete_artwork(selection: Option<&SelectedArtwork>, busy: bool, _scanning: bool) -> bool {
    matches!(selection, Some(SelectedArtwork::Legacy | SelectedArtwork::Version(_))) && !busy
}

fn protected_artwork_cursor(ctx: &egui::Context, message: &str) {
    let Some(pointer) = ctx.pointer_hover_pos() else { return; };
    ctx.set_cursor_icon(egui::CursorIcon::None);
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("protected-artwork-cursor")));
    painter.circle_filled(pointer, 9.0, md3::ERROR);
    painter.line_segment([pointer + egui::vec2(0.0, -4.5), pointer + egui::vec2(0.0, 1.0)],
        egui::Stroke::new(1.8_f32, egui::Color32::WHITE));
    painter.circle_filled(pointer + egui::vec2(0.0, 4.0), 1.1, egui::Color32::WHITE);
    let galley = painter.layout_no_wrap(message.to_string(), egui::FontId::proportional(13.0), md3::ERROR);
    let bounds = ctx.content_rect().shrink(8.0);
    let left = (pointer.x - galley.size().x * 0.5).clamp(bounds.left(), (bounds.right() - galley.size().x).max(bounds.left()));
    let top = (pointer.y - 18.0 - galley.size().y).max(bounds.top());
    painter.galley(egui::pos2(left, top), galley, md3::ERROR);
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
) -> bool {
    let button = egui::Button::new(
        egui::RichText::new("")
            .size(12.0)
            .color(if enabled { md3::ON_SURFACE } else { md3::ON_SURFACE_VARIANT }),
    )
    .fill(md3::SURFACE_CONTAINER_HIGH)
    .stroke(egui::Stroke::new(1.0_f32, md3::OUTLINE_VARIANT))
    .corner_radius(2)
    .min_size(egui::vec2(width, 36.0));
    let response = ui.add_enabled(enabled, button).on_hover_text(label);
    if let Some(color) = indicator {
        let pulse = if color == md3::SUCCESS { 0.7 * (ui.input(|input| input.time) as f32 * 3.0).sin() } else { 0.0 };
        ui.painter().circle_filled(response.rect.left_center() + egui::vec2(15.0, 0.0), 3.2 + pulse, color);
        if color == md3::SUCCESS { ui.ctx().request_repaint_after(std::time::Duration::from_millis(60)); }
    }
    let text_color = if enabled { md3::ON_SURFACE } else { md3::ON_SURFACE_VARIANT };
    let galley = egui::WidgetText::from(egui::RichText::new(label).size(12.0).color(text_color))
        .into_galley(ui, Some(egui::TextWrapMode::Truncate), (response.rect.width() - 60.0).max(0.0), egui::FontId::proportional(12.0));
    let text_x = if indicator.is_some() { response.rect.left() + 27.0 }
        else { response.rect.center().x - galley.size().x * 0.5 };
    ui.painter().galley(egui::pos2(text_x, response.rect.center().y - galley.size().y * 0.5), galley, text_color);
    let x = response.rect.right() - 17.0;
    let y = response.rect.center().y;
    ui.painter().add(egui::Shape::convex_polygon(
        vec![egui::pos2(x - 4.0, y - 2.0), egui::pos2(x + 4.0, y - 2.0), egui::pos2(x, y + 3.0)],
        if enabled { md3::SIGNAL } else { md3::OUTLINE },
        egui::Stroke::NONE,
    ));
    let double_clicked = response.double_clicked();
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
        .show(|ui| {
            ui.set_width((response.rect.width() - 18.0).max(0.0));
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), contents);
        });
    double_clicked
}

fn refresh_icon_button(ui: &mut egui::Ui, tooltip: &str, started_at: &mut Option<f64>) -> bool {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(34.0, 34.0), egui::Sense::click());
    let now = ui.input(|input| input.time);
    if response.clicked() { *started_at = Some(now); }
    let rotation = if let Some(start) = *started_at {
        let progress = ((now - start) / 0.72) as f32;
        if progress < 1.0 { ui.ctx().request_repaint(); }
        else { *started_at = None; }
        refresh_rotation(progress)
    } else { 0.0 };
    let rotate = |vector: egui::Vec2| egui::vec2(
        vector.x * rotation.cos() - vector.y * rotation.sin(),
        vector.x * rotation.sin() + vector.y * rotation.cos());
    let color = if response.hovered() { md3::SIGNAL } else { md3::PRIMARY };
    let center = rect.center();
    let radius = 8.0;
    let points: Vec<egui::Pos2> = (0..=24).map(|step| {
        let angle = (-145.0_f32 + step as f32 * 11.5).to_radians();
        center + rotate(egui::vec2(angle.cos() * radius, angle.sin() * radius))
    }).collect();
    ui.painter().add(egui::Shape::line(points, egui::Stroke::new(1.8_f32, color)));
    let tip = center + rotate(egui::vec2((-145.0_f32).to_radians().cos() * radius, (-145.0_f32).to_radians().sin() * radius));
    ui.painter().line_segment([tip, tip + rotate(egui::vec2(0.0, -5.0))], egui::Stroke::new(1.8_f32, color));
    ui.painter().line_segment([tip, tip + rotate(egui::vec2(5.0, 0.0))], egui::Stroke::new(1.8_f32, color));
    response.on_hover_text(tooltip).clicked()
}

fn picker_option(ui: &mut egui::Ui, label: &str, selected: bool) -> bool {
    picker_option_aligned(ui, label, selected, false)
}

fn picker_option_aligned(ui: &mut egui::Ui, label: &str, selected: bool, centered: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 34.0), egui::Sense::click());
    let fill = if selected { md3::PRIMARY_CONTAINER } else if response.hovered() { md3::SURFACE_CONTAINER_HIGHEST } else { md3::SURFACE_CONTAINER_HIGH };
    let color = if selected { md3::ON_PRIMARY_CONTAINER } else { md3::ON_SURFACE };
    ui.painter().rect_filled(rect, 2.0, fill);
    let galley = egui::WidgetText::from(egui::RichText::new(label).size(12.0).color(color))
        .into_galley(ui, Some(egui::TextWrapMode::Truncate), (rect.width() - 24.0).max(0.0), egui::FontId::proportional(12.0));
    let x = if centered { rect.center().x - galley.size().x * 0.5 } else { rect.left() + 12.0 };
    ui.painter().galley(egui::pos2(x, rect.center().y - galley.size().y * 0.5), galley, color);
    let clicked = response.clicked();
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
        self.is_busy = true;
        self.progress_step = 0;
        self.progress_display = 0.0;
        self.progress_total = WALLET_WRITE_PHASES;
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

impl Drop for AirCardApp {
    fn drop(&mut self) {
        if let Some(flag) = self.scan_stop_flag.take() { flag.store(true, Ordering::Relaxed); }
        if let Some(guard) = self.corefp_session.take() {
            if let Err(err) = guard.finish() { eprintln!("CoreFP session cleanup failed: {err:#}"); }
        }
    }
}

impl eframe::App for AirCardApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_messages();
        self.handle_session_messages(ctx);
        self.handle_image_load_messages(ctx);
        self.handle_refresh_messages();
        self.poll_device_changes();
        self.maintain_syslog_scan();
        self.handle_scan_messages();
        self.ensure_corefp_session();
        self.maybe_read_current_face();
        self.refresh_record_previews(ctx);
        if self.is_busy && ctx.input(|input| input.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.status_msg = self.language.text("Wait for the device operation to finish before closing AirCard.").to_string();
        }
        let language = self.language;
        ctx.request_repaint_after(std::time::Duration::from_secs(3));

        if self.is_busy || self.scanning_syslog || self.corefp_rx.is_some() || self.image_load_rx.is_some() || self.refresh_rx.is_some() {
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
                                if picker_option_aligned(ui, language.option_label(option), next_language == option, true) {
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
                            self.page = page;
                        }
                    }
                });
                ui.add_space(16.0);
                ui.painter().hline(ui.available_rect_before_wrap().x_range(), ui.cursor().top(), egui::Stroke::new(1.0_f32, md3::OUTLINE_VARIANT));
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if refresh_icon_button(ui, language.text("Refresh"), &mut self.refresh_started_at) {
                                    self.device_poll_at = std::time::Instant::now();
                                    self.request_refresh(ui.ctx());
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
                                .map(|device| format!("{}  ·  {}", local_phone_name(&self.phone_records, &device.udid, &device.name), device.transport_summary()))
                                .or_else(|| self.selected_udid.as_ref().filter(|udid| archived_udids.contains(udid)).map(|udid| {
                                    format!("{}  ·  {}", archived_phone_name(&self.phone_records, udid), language.text("Offline"))
                                }))
                                .unwrap_or_else(|| language.text("No device").to_string());
                            let rename_device = picker(ui, "device_picker", &selected_label, 224.0, controls_enabled && (!self.devices.is_empty() || !archived_udids.is_empty()),
                                Some(if self.selected_transport_available() { md3::SUCCESS } else { md3::ERROR }), |ui| {
                                for device in &self.devices {
                                    let label = format!("{}  ·  {}", local_phone_name(&self.phone_records, &device.udid, &device.name), device.transport_summary());
                                    if picker_option(ui, &label, Some(&device.udid) == next_udid.as_ref()) {
                                        next_udid = Some(device.udid.clone());
                                    }
                                }
                                for udid in &archived_udids {
                                    let label = format!("{}  ·  {}", archived_phone_name(&self.phone_records, udid), language.text("Offline"));
                                    if picker_option(ui, &label, Some(udid) == next_udid.as_ref()) {
                                        next_udid = Some(udid.clone());
                                    }
                                }
                            });
                            if rename_device {
                                if let Some(udid) = self.selected_udid.clone() {
                                    let fallback = self.devices.iter().find(|device| device.udid == udid).map(|device| device.name.as_str()).unwrap_or("");
                                    self.rename_text = local_phone_name(&self.phone_records, &udid, fallback).to_string();
                                    self.rename_target = Some(RenameTarget::Phone(udid));
                                    egui::Popup::close_all(ctx);
                                }
                            }
                            if next_udid != self.selected_udid {
                                self.selected_udid = next_udid;
                                self.card_hash.clear();
                                self.live_face_texture = None;
                                self.live_face_target = None;
                                self.show_live_face = true;
                                self.scanned_name.clear();
                                self.selected_artwork = None;
                                if let Some(selected) = self.selected_udid.clone() {
                                    self.add_log(format!("Selected device: {}", selected));
                                }
                            }
                            if self.scanning_syslog {
                                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                    ui.add(egui::Spinner::new().size(12.0));
                                    ui.label(egui::RichText::new(language.text("Open Wallet · tap a card"))
                                        .size(11.0).color(md3::ON_SURFACE_VARIANT));
                                });
                            }
                    });
                });
            });

        self.maintain_syslog_scan();

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

        self.show_diagnostics(ctx);
        self.show_archive_dialogs(ctx);
    }
}

impl AirCardApp {
    fn show_diagnostics(&mut self, ctx: &egui::Context) {
        if !self.show_logs_window { return; }
        let language = self.language;
        let now = ctx.input(|input| input.time);
        let copied = self.logs_copied_at.is_some_and(|time| now - time < 2.0);
        if copied { ctx.request_repaint_after(std::time::Duration::from_millis(50)); }
        let mut close = ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        egui::Window::new("aircard_diagnostics")
            .id(egui::Id::new("diagnostics_panel_v2"))
            .title_bar(false)
            .default_pos(ctx.content_rect().center() - egui::vec2(380.0, 230.0))
            .default_size([760.0, 460.0])
            .min_size([540.0, 320.0])
            .frame(egui::Frame::new()
                .fill(egui::Color32::from_rgba_unmultiplied(250, 249, 245, 246))
                .stroke(egui::Stroke::new(1.0_f32, md3::OUTLINE_VARIANT))
                .corner_radius(4).inner_margin(egui::Margin::same(22))
                .shadow(egui::Shadow { offset: [0, 12], blur: 32, spread: 0, color: egui::Color32::from_black_alpha(40) }))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new("AC / DIAGNOSTICS").monospace().size(10.0).color(md3::ON_SURFACE_VARIANT));
                        ui.label(egui::RichText::new(language.text("Logs")).size(25.0).color(md3::ON_SURFACE));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add_sized([34.0, 34.0], egui::Button::new(egui::RichText::new("×").size(22.0))
                            .fill(md3::SURFACE_CONTAINER_HIGH).stroke(egui::Stroke::NONE)).on_hover_text(language.text("Close")).clicked() { close = true; }
                    });
                });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let copy = egui::Button::new(egui::RichText::new(language.text(if copied { "Copied" } else { "Copy Logs" }))
                        .size(12.0).color(if copied { md3::SUCCESS } else { md3::ON_PRIMARY }))
                        .fill(if copied { md3::SURFACE_CONTAINER_HIGH } else { md3::PRIMARY })
                        .stroke(egui::Stroke::new(1.0_f32, if copied { md3::SUCCESS } else { md3::PRIMARY })).corner_radius(2);
                    if ui.add_enabled(!self.logs.is_empty(), copy.min_size(egui::vec2(112.0, 34.0))).clicked() {
                        ctx.copy_text(self.logs.join("\n"));
                        self.logs_copied_at = Some(now);
                        ctx.request_repaint();
                    }
                    if ui.add_sized([132.0, 34.0], egui::Button::new(language.text("Save to File..."))
                        .fill(md3::SURFACE_CONTAINER_HIGH).stroke(egui::Stroke::NONE)).clicked() {
                        if let Some(path) = rfd::FileDialog::new().set_file_name("aircard-diagnostics.log")
                            .add_filter("Log files", &["log", "txt"]).save_file() {
                            match std::fs::write(&path, self.logs.join("\r\n")) {
                                Ok(()) => self.add_log(format!("Saved log file to {}", path.display())),
                                Err(err) => self.add_log(format!("Could not save log file: {err}")),
                            }
                        }
                    }
                    if ui.add_sized([72.0, 34.0], egui::Button::new(language.text("Clear"))
                        .fill(egui::Color32::TRANSPARENT).stroke(egui::Stroke::new(1.0_f32, md3::OUTLINE_VARIANT))).clicked() {
                        self.logs.clear(); self.logs_copied_at = None;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.checkbox(&mut self.logs_follow, language.text("Follow latest"));
                    });
                });
                ui.add_space(12.0);
                let log_height = (ui.available_height() - 64.0).max(140.0);
                egui::Frame::new().fill(md3::SURFACE_CONTAINER_HIGH)
                    .stroke(egui::Stroke::new(1.0_f32, md3::OUTLINE_VARIANT))
                    .corner_radius(2).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        egui::ScrollArea::both().id_salt("diagnostics_lines")
                            .max_height(log_height).stick_to_bottom(self.logs_follow).auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 5.0;
                                if self.logs.is_empty() {
                                    ui.label(egui::RichText::new(language.text("No events logged yet.")).color(md3::ON_SURFACE_VARIANT));
                                }
                                for line in &self.logs {
                                    let lower = line.to_ascii_lowercase();
                                    let color = if lower.contains("failed") || lower.contains("error") { md3::ERROR }
                                        else if lower.contains("successfully") || lower.contains("completed") { md3::SUCCESS }
                                        else { md3::ON_SURFACE };
                                    ui.add(egui::Label::new(egui::RichText::new(line).monospace().size(11.0).color(color))
                                        .wrap_mode(egui::TextWrapMode::Extend).selectable(true));
                                }
                            });
                    });
                ui.add_space(8.0);
                ui.label(egui::RichText::new(format!("{} {}", self.logs.len(), language.text("entries")))
                    .monospace().size(10.0).color(md3::ON_SURFACE_VARIANT));
            });
        if close { self.show_logs_window = false; }
    }

    fn remember_connected_phone_names(&mut self) {
        let names = self.devices.iter().map(|device| (device.udid.clone(), device.name.clone())).collect::<Vec<_>>();
        if let Err(err) = card_history::remember_device_names(&names) {
            self.add_log(format!("Could not remember device names: {err:#}"));
        }
        self.phone_records = card_history::phones();
    }

    fn reload_local_records(&mut self) {
        self.card_records = card_history::load();
        self.remember_connected_phone_names();
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
        let language = self.language;
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
            .map(|device| (device.udid.clone(), local_phone_name(&self.phone_records, &device.udid, &device.name).to_string(), true)).collect();
        for phone in &self.phone_records {
            if !known_phones.iter().any(|item| item.0 == phone.udid) {
                let name = archived_phone_name(&self.phone_records, &phone.udid);
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
                        let button = egui::Button::new(label).truncate()
                            .fill(if selected { md3::PRIMARY_CONTAINER } else { md3::SURFACE_CONTAINER_HIGH })
                            .corner_radius(2)
                            .min_size(egui::vec2(ui.available_width(), 38.0));
                        let response = ui.add(button);
                        if response.double_clicked() {
                            self.rename_text = name.clone();
                            self.rename_target = Some(RenameTarget::Phone(udid.clone()));
                        } else if response.clicked() {
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
                            if m3_button_filled(ui, "Save phone") && !self.is_busy {
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
                            let selected = self.card_hash.trim() == card.hash;
                            let response = card_list_row(ui, &card.name, &card.hash, selected);
                            if response.double_clicked() {
                                self.rename_text = card.name.clone();
                                self.rename_target = Some(RenameTarget::Card(key.0, key.1));
                            } else if response.clicked() {
                                self.card_hash = card.hash.clone();
                                self.live_face_texture = None;
                                if self.live_face_rx.is_none() { self.live_face_target = None; }
                                self.show_live_face = true;
                                self.card_changed_at = std::time::Instant::now();
                                self.scanned_name = card.name;
                                self.selected_artwork = None;
                                self.pending_delete_card = None;
                            }
                            response.context_menu(|ui| {
                                if ui.add(egui::Button::new(egui::RichText::new(language.text("Delete local card record…")).color(md3::ERROR))).clicked() {
                                    self.pending_delete_card = Some((card.udid.clone(), card.hash.clone()));
                                    self.pending_delete_version = None;
                                    self.pending_delete_phone = None;
                                    ui.close();
                                }
                            });
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
                            if m3_button_filled(ui, "Save card") && !self.is_busy {
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
                                if m3_button_filled(ui, "Edit artwork →") { self.page = Page::Artwork; }
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
                                    let response = history_tile_frame(active)
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
                                let delete_enabled = can_delete_artwork(self.selected_artwork.as_ref(), self.is_busy, self.scanning_syslog);
                                let color = if delete_enabled { md3::ERROR } else { md3::ON_SURFACE_VARIANT };
                                let delete = egui::Button::new(egui::RichText::new(language.text("Delete")).color(color))
                                    .fill(if delete_enabled { md3::ERROR_CONTAINER } else { md3::SURFACE_CONTAINER_HIGH })
                                    .stroke(egui::Stroke::new(1.0_f32, if delete_enabled { md3::ERROR } else { md3::OUTLINE_VARIANT }));
                                let response = ui.add_enabled(delete_enabled, delete);
                                if self.selected_artwork == Some(SelectedArtwork::Origin) && response.contains_pointer() {
                                    protected_artwork_cursor(ctx, language.text("Original artwork cannot be deleted"));
                                }
                                if response.clicked() {
                                    if let Some(selection) = self.selected_artwork.clone() {
                                        self.pending_delete_version = Some((record.udid.clone(), record.hash.clone(), selection));
                                    }
                                }
                            });
                        });
                        self.show_operation_progress(ui);
                    } else {
                        ui.label(egui::RichText::new("Select a phone and card").strong().size(24.0).color(md3::ON_SURFACE));
                        ui.label("Cards that are only scanned are not saved automatically. Add a card manually, or save after a successful artwork write.");
                    }
                });
            });
        });
    }

    fn show_archive_dialogs(&mut self, ctx: &egui::Context) {
        let language = self.language;
        if let Some(target) = self.rename_target.clone() {
            let phone = matches!(target, RenameTarget::Phone(_));
            let (key, title, description, field_label) = if phone {
                ("rename-phone", "Rename device", "This name is only used in AirCard. The iPhone name and its UDID stay unchanged.", "Device name")
            } else {
                ("rename-card", "Rename card", "Give this card a name. Its artwork and history stay unchanged.", "Card name")
            };
            let action = archive_dialog(ctx, &mut self.dialog_motion, key, language.text(title), false, |ui, first| {
                ui.label(egui::RichText::new(language.text(description))
                    .size(13.0).color(md3::ON_SURFACE_VARIANT));
                ui.add_space(20.0);
                ui.label(egui::RichText::new(language.text(field_label)).monospace().size(11.0).color(md3::ON_SURFACE_VARIANT));
                ui.add_space(8.0);
                egui::Frame::new().fill(egui::Color32::from_white_alpha(160))
                    .stroke(egui::Stroke::new(1.0_f32, md3::OUTLINE_VARIANT))
                    .inner_margin(egui::Margin::symmetric(12, 10)).corner_radius(2).show(ui, |ui| {
                        let response = ui.add(egui::TextEdit::singleline(&mut self.rename_text).frame(false)
                            .font(egui::FontId::proportional(18.0)).desired_width(ui.available_width()));
                        if first { response.request_focus(); }
                    });
                ui.add_space(8.0);
                let valid = !self.rename_text.trim().is_empty() && self.rename_text.trim().chars().count() <= 80;
                ui.label(egui::RichText::new(format!("{} / 80", self.rename_text.trim().chars().count()))
                    .monospace().size(10.0).color(if valid { md3::ON_SURFACE_VARIANT } else { md3::ERROR }));
                ui.add_space(24.0);
                let mut action = DialogAction::Pending;
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("ESC / ENTER").monospace().size(10.0).color(md3::ON_SURFACE_VARIANT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if dialog_button(ui, language.text("Save"), true, false, valid && !self.is_busy) { action = DialogAction::Confirm; }
                        if dialog_button(ui, language.text("Cancel"), false, false, true) { action = DialogAction::Cancel; }
                    });
                });
                if valid && !self.is_busy && ui.is_enabled() && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
                    action = DialogAction::Confirm;
                }
                action
            });
            if action != DialogAction::Pending {
                self.rename_target = None;
                if action == DialogAction::Confirm && !self.is_busy {
                    let result = match &target {
                        RenameTarget::Card(udid, hash) => card_history::rename_card(udid, hash, &self.rename_text),
                        RenameTarget::Phone(udid) => card_history::add_phone(udid, &self.rename_text),
                    };
                    match result {
                        Ok(()) => {
                            if let RenameTarget::Card(udid, hash) = &target {
                                if self.selected_udid.as_deref() == Some(udid) && same_card_hash(&self.card_hash, hash) {
                                    self.scanned_name = self.rename_text.trim().into();
                                }
                            }
                            self.reload_local_records();
                        }
                        Err(err) => self.status_msg = format!("Rename failed: {err:#}"),
                    }
                }
            }
            return;
        }

        let (key, title, description, target) = if let Some(udid) = self.pending_delete_phone.as_ref() {
            ("delete-phone", "Delete phone record?", "All local cards, ORIGIN images and artwork history on this phone will be removed. This cannot be undone. The iPhone is not changed.",
                self.phone_records.iter().find(|phone| &phone.udid == udid).map(|phone| phone.name.clone()).unwrap_or_else(|| udid.clone()))
        } else if let Some((udid, hash)) = self.pending_delete_card.as_ref() {
            ("delete-card", "Delete card record?", "All local history for this card, including ORIGIN, will be removed. This cannot be undone. The iPhone is not changed.",
                known_card_name(&self.card_records, Some(udid), hash).unwrap_or(hash).to_string())
        } else if let Some((udid, hash, _)) = self.pending_delete_version.as_ref() {
            ("delete-artwork", "Delete artwork record?", "Only the selected local artwork record will be removed. ORIGIN stays protected. The iPhone is not changed.",
                known_card_name(&self.card_records, Some(udid), hash).unwrap_or(hash).to_string())
        } else { return; };
        let action = archive_dialog(ctx, &mut self.dialog_motion, key, language.text(title), true, |ui, _| {
            ui.label(egui::RichText::new(target).size(18.0).color(md3::ON_SURFACE));
            ui.add_space(12.0);
            ui.label(egui::RichText::new(language.text(description)).size(13.0).color(md3::ON_SURFACE_VARIANT));
            ui.add_space(30.0);
            let mut action = DialogAction::Pending;
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("ESC / CANCEL").monospace().size(10.0).color(md3::ON_SURFACE_VARIANT));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if dialog_button(ui, language.text("Delete"), true, true, !self.is_busy) { action = DialogAction::Confirm; }
                    if dialog_button(ui, language.text("Cancel"), false, false, true) { action = DialogAction::Cancel; }
                });
            });
            action
        });
        if action == DialogAction::Pending { return; }
        let phone = self.pending_delete_phone.take();
        let card = self.pending_delete_card.take();
        let version = self.pending_delete_version.take();
        if action == DialogAction::Cancel || self.is_busy { return; }
        let result = if let Some(udid) = phone {
            card_history::delete_phone(&udid).map(|_| {
                if self.selected_udid.as_deref() == Some(&udid) { self.selected_udid = None; self.card_hash.clear(); }
            })
        } else if let Some((udid, hash)) = card {
            card_history::delete_card(&udid, &hash).map(|_| { self.card_hash.clear(); })
        } else if let Some((udid, hash, selection)) = version {
            match selection {
                SelectedArtwork::Origin => Err(anyhow::anyhow!("ORIGIN cannot be deleted")),
                SelectedArtwork::Legacy => card_history::delete_legacy_version(&udid, &hash),
                SelectedArtwork::Version(id) => card_history::delete_version(&udid, &hash, &id),
            }
        } else { return; };
        match result {
            Ok(()) => { self.selected_artwork = None; self.reload_local_records(); }
            Err(err) => self.status_msg = format!("Delete local record failed: {err:#}"),
        }
    }

    fn show_operation_progress(&mut self, ui: &mut egui::Ui) {
        if !self.is_busy { return; }
        ui.add_space(10.0);
        if self.progress_total > 0 {
            let target = self.progress_step as f32 / self.progress_total as f32;
            self.progress_display = smooth_progress(self.progress_display, target, ui.input(|input| input.stable_dt));
            let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 4.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 2.0, md3::SURFACE_CONTAINER_HIGH);
            let fill = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * self.progress_display, rect.height()));
            ui.painter().rect_filled(fill, 2.0, md3::SIGNAL);
            if self.progress_display < target { ui.ctx().request_repaint(); }
            ui.add_space(5.0);
            ui.label(egui::RichText::new(format!("{:02} / {:02}  {}",
                (self.progress_step + 1).min(self.progress_total), self.progress_total,
                self.language.text(&self.progress_msg))).size(11.0).color(md3::PRIMARY));
            ui.label(egui::RichText::new(self.language.text("Keep iPhone connected until completion."))
                .size(10.0).color(md3::ON_SURFACE_VARIANT));
        } else {
            ui.label(egui::RichText::new(self.language.text(&self.progress_msg)).size(11.0).color(md3::PRIMARY));
        }
    }

    fn show_wallet_tab(&mut self, ctx: &egui::Context, ui: &mut egui::Ui) {
        let language = self.language;

        ui.horizontal_top(|ui| {
            let total_width = ui.available_width();
            let left_width = (total_width * 0.37).clamp(340.0, 520.0);
            let right_width = (total_width - left_width - 18.0).max(320.0);
            ui.allocate_ui_with_layout(egui::vec2(left_width, 0.0), egui::Layout::top_down(egui::Align::Min), |left| {
            m3_card(left, |ui| {
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(egui::vec2(110.0, 46.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.label(egui::RichText::new("ARTWORK / EDITOR").monospace().size(10.0).color(md3::ON_SURFACE_VARIANT));
                        ui.add_space(3.0);
                        ui.label(egui::RichText::new(language.text("Artwork")).strong().size(24.0).color(md3::ON_SURFACE));
                    });
                });
                ui.painter().rect_filled(egui::Rect::from_min_size(ui.cursor().min, egui::vec2(46.0, 3.0)), 0.0, md3::SIGNAL);
                ui.add_space(7.0);
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Choose a card on Cards, then prepare its new artwork.").size(12.0).color(md3::ON_SURFACE_VARIANT));
                ui.add_space(16.0);

                ui.label(egui::RichText::new(language.text("Target Card")).monospace().strong().size(11.0).color(md3::ON_SURFACE));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if let Some(name) = known_card_name(&self.card_records, self.selected_udid.as_deref(), &self.card_hash) {
                        egui::Frame::new().fill(md3::SURFACE_CONTAINER_HIGH)
                            .inner_margin(egui::Margin::symmetric(8, 5)).show(ui, |ui| {
                                ui.set_min_width((ui.available_width() - 16.0).max(0.0));
                                ui.label(name).on_hover_text(self.card_hash.trim());
                            });
                    } else {
                        let response = ui.add(egui::TextEdit::singleline(&mut self.card_hash)
                            .hint_text(language.text("Base64 pass hash..."))
                            .background_color(md3::SURFACE_CONTAINER_HIGH)
                            .desired_width(ui.available_width()));
                        if response.changed() {
                            self.live_face_texture = None;
                            if self.live_face_rx.is_none() { self.live_face_target = None; }
                            self.show_live_face = true;
                            self.card_changed_at = std::time::Instant::now();
                        }
                    }
                });

                ui.add_space(16.0);

                // Card Skin
                ui.label(egui::RichText::new(language.text("Artwork")).monospace().strong().size(11.0).color(md3::ON_SURFACE));
                ui.label(egui::RichText::new(language.text("PNG, JPG, WebP - auto-scaled to 1536x969")).size(12.0).color(md3::ON_SURFACE_VARIANT));
                ui.label(egui::RichText::new(language.text("Drag inside the preview to reposition the crop.")).size(12.0).color(md3::ON_SURFACE_VARIANT));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(self.image_load_rx.is_none(), |ui| {
                        if m3_button_filled(ui, language.text("Choose Image...")) { self.select_skin(ctx); }
                    });
                    if self.image_load_rx.is_some() { ui.spinner(); }
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

                let can_flash = apply_available(self.is_busy, self.live_face_rx.is_some(), self.pending_flash.is_some())
                    && self.corefp_session.is_some()
                    && self.image_load_rx.is_none()
                    && self.selected_transport_available()
                    && !self.card_hash.trim().is_empty()
                    && self.skin.is_some();
                let flash_btn = egui::Button::new(
                    egui::RichText::new(language.text(if self.pending_flash.is_some() { "Apply queued" } else { "Apply Card Skin" })).strong().size(14.0)
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

                self.show_operation_progress(ui);
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
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(language.text(if self.show_live_face { "Current card · device read" } else { "New artwork" }))
                        .size(12.0).color(md3::ON_SURFACE_VARIANT));
                    if self.source_texture.is_some() && self.live_face_texture.is_some() {
                        if m3_button_tonal(ui, language.text(if self.show_live_face { "Show new artwork" } else { "Show current card" })) {
                            self.show_live_face = !self.show_live_face;
                        }
                    }
                });
                ui.add_space(12.0);

                let pass_w = (ui.available_width() - 8.0).clamp(250.0, 600.0);
                let pass_h = pass_w * (969.0 / 1536.0);
                ui.vertical_centered(|ui| {
                    let (rect, response) =
                        ui.allocate_exact_size(egui::vec2(pass_w, pass_h), egui::Sense::drag());
                    let source_dimensions = if self.show_live_face { None } else {
                        self.source_image.as_ref().map(|image| (image.width(), image.height()))
                    };
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
                    if let Some(tex) = self.live_face_texture.as_ref().filter(|_| self.show_live_face) {
                        painter.image(tex.id(), rect, egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
                        painter.rect_stroke(rect, 2.0, egui::Stroke::new(1.0_f32, md3::OUTLINE), egui::StrokeKind::Inside);
                    } else if let (Some(tex), Some((source_width, source_height))) =
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
                            language.text(if self.live_face_rx.is_some() { "Reading current card artwork..." } else { "No artwork loaded" }),
                            egui::FontId::proportional(14.0), md3::ON_PRIMARY);
                        painter.rect_stroke(rect, 2.0, egui::Stroke::new(1.0_f32, md3::OUTLINE), egui::StrokeKind::Inside);
                    }
                });

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let size = self.live_face_texture.as_ref().filter(|_| self.show_live_face)
                        .map(|texture| texture.size()).unwrap_or([1536, 969]);
                    ui.label(egui::RichText::new(format!("{}x{}", size[0], size[1])).size(11.0).color(md3::ON_SURFACE_VARIANT));
                    ui.label(egui::RichText::new("|").size(11.0).color(md3::OUTLINE_VARIANT));
                    ui.label(egui::RichText::new(format!("{:.3} ratio", size[0] as f32 / size[1].max(1) as f32)).size(11.0).color(md3::ON_SURFACE_VARIANT));
                    ui.label(egui::RichText::new("|").size(11.0).color(md3::OUTLINE_VARIANT));
                    if self.show_live_face && self.live_face_rx.is_some() {
                        ui.label(egui::RichText::new(language.text("Refreshing...")).size(11.0).color(md3::ON_SURFACE_VARIANT));
                    } else if (self.show_live_face && self.live_face_texture.is_some()) || (!self.show_live_face && self.skin.is_some()) {
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
