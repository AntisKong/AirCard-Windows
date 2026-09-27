//! Application-owned data stays beside the portable executable.

use std::path::PathBuf;

pub fn directory() -> PathBuf {
    std::env::current_exe()
        .expect("AirCard executable path is unavailable")
        .parent()
        .expect("AirCard executable has no parent directory")
        .join("Data")
}
