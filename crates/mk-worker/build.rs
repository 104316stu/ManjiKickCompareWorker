//! Windows: gives both programs the app icon. The .ico is assembled here
//! from the PNGs in assets/ (PNG-compressed entries, fine since Vista).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    #[cfg(windows)]
    windows_icon();
}

#[cfg(windows)]
fn windows_icon() {
    use std::path::PathBuf;
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let assets = manifest.join("../../assets");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    let mut images = Vec::new();
    for size in [16u32, 32, 256] {
        let path = assets.join(format!("icon-{size}.png"));
        println!("cargo:rerun-if-changed={}", path.display());
        match std::fs::read(&path) {
            Ok(png) => images.push((size, png)),
            Err(_) => {
                println!("cargo:warning=no {} - building without an app icon", path.display());
                return;
            }
        }
    }
    let mut ico = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]);
    ico.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, png) in &images {
        let dim = if *size >= 256 { 0 } else { *size as u8 };
        ico.extend_from_slice(&[dim, dim, 0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes()); // planes
        ico.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in &images {
        ico.extend_from_slice(png);
    }
    let ico_path = out.join("app.ico");
    std::fs::write(&ico_path, ico).unwrap();
    let rc_path = out.join("app.rc");
    let escaped = ico_path.display().to_string().replace('\\', "\\\\");
    std::fs::write(&rc_path, format!("1 ICON \"{escaped}\"\n")).unwrap();
    // A machine without a resource compiler still builds, just without the icon.
    match embed_resource::compile(&rc_path, embed_resource::NONE) {
        embed_resource::CompilationResult::Ok | embed_resource::CompilationResult::NotWindows => {}
        other => println!("cargo:warning=app icon not embedded: {other}"),
    }
}
