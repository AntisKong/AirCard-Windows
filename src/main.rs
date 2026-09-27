#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod afc;
mod airlift;
mod airlift_read;
mod airtraffic;
mod app;
mod apple;
mod card_history;
mod corefp;
mod device;
mod flasher;
mod icon;
mod image_skin;
mod i18n;
mod portable_data;
mod scanner;
mod wallet_backup;

fn main() -> eframe::Result<()> {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--verify-portable")) {
        let expected = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|p| p.join("AppleSupport")));
        if apple::locate_support_dir() != expected {
            eprintln!("AppleSupport beside this EXE is incomplete; no system-directory fallback is accepted by this check");
            std::process::exit(2);
        }
        match apple::verify_support() {
            Ok(message) => {
                println!("{message}");
                match device::list_connected_devices() {
                    Ok(devices) => println!("Devices visible through Apple Devices: {}", devices.len()),
                    Err(err) => { eprintln!("Device enumeration unavailable: {err:#}"); std::process::exit(3); }
                }
                return Ok(());
            }
            Err(err) => { eprintln!("{err:#}"); std::process::exit(1); }
        }
    }
    match corefp::maybe_run_helper() {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(err) => {
            eprintln!("CoreFP helper failed: {err:#}");
            return Ok(());
        }
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1240.0, 800.0])
            .with_min_inner_size([960.0, 640.0])
            .with_title("AirCard")
            .with_icon(egui::IconData { rgba: icon::rgba(64), width: 64, height: 64 }),
        ..Default::default()
    };

    eframe::run_native(
        "AirCard",
        options,
        Box::new(|cc| Ok(Box::new(app::AirCardApp::new(cc)))),
    )
}
