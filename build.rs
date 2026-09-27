#[path = "src/icon.rs"]
mod icon;

fn main() {
    println!("cargo:rerun-if-changed=src/icon.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("gnu")
    {
        return;
    }
    if let Err(error) = embed_windows_icon() {
        panic!("Could not embed Air icon in executable: {error}");
    }
}

fn embed_windows_icon() -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};
    use std::path::PathBuf;
    use std::process::Command;

    let out = PathBuf::from(std::env::var_os("OUT_DIR").ok_or_else(|| Error::new(ErrorKind::NotFound, "OUT_DIR"))?);
    let ico = out.join("air.ico");
    let rc = out.join("air.rc");
    let obj = out.join("air-icon.o");
    let size = 64u32;
    let pixels = icon::rgba(size);
    let mask_stride = ((size + 31) / 32 * 4) as usize;
    let pixel_bytes = (size * size * 4) as usize;
    let image_bytes = 40 + pixel_bytes + mask_stride * size as usize;
    let mut bytes = Vec::with_capacity(22 + image_bytes);
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&[size as u8, size as u8, 0, 0]);
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&32u16.to_le_bytes());
    bytes.extend_from_slice(&(image_bytes as u32).to_le_bytes());
    bytes.extend_from_slice(&22u32.to_le_bytes());
    bytes.extend_from_slice(&40u32.to_le_bytes());
    bytes.extend_from_slice(&(size as i32).to_le_bytes());
    bytes.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&32u16.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&(pixel_bytes as u32).to_le_bytes());
    bytes.extend_from_slice(&[0u8; 16]);
    for y in (0..size as usize).rev() {
        for x in 0..size as usize {
            let index = (y * size as usize + x) * 4;
            bytes.extend_from_slice(&[pixels[index + 2], pixels[index + 1], pixels[index], pixels[index + 3]]);
        }
    }
    for y in (0..size as usize).rev() {
        let mut row = vec![0u8; mask_stride];
        for x in 0..size as usize {
            if pixels[(y * size as usize + x) * 4 + 3] == 0 {
                row[x / 8] |= 1 << (7 - x % 8);
            }
        }
        bytes.extend_from_slice(&row);
    }
    std::fs::write(&ico, bytes)?;
    std::fs::write(&rc, format!("1 ICON \"{}\"\n", ico.display().to_string().replace('\\', "/")))?;
    let status = Command::new("windres")
        .args(["-i", rc.to_str().unwrap(), "-o", obj.to_str().unwrap(), "-O", "coff"])
        .status()?;
    if !status.success() {
        return Err(Error::other(format!("windres exited with {status}")));
    }
    println!("cargo:rustc-link-arg={}", obj.display());
    Ok(())
}
