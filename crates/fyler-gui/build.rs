use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let icon_path = manifest_dir.join("../../assets/fyler.ico");

    let file = std::fs::File::open(&icon_path).expect("Failed to open fyler.ico");
    let icon_dir = ico::IconDir::read(file).expect("Failed to parse fyler.ico");
    let entry = icon_dir
        .entries()
        .iter()
        .max_by_key(|entry| entry.width())
        .expect("fyler.ico has no icon entries");
    let image = entry.decode().expect("Failed to decode fyler.ico entry");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out_dir.join("window_icon.rgba"), image.rgba_data())
        .expect("Failed to write window icon RGBA");

    println!("cargo:rustc-env=FYLER_WINDOW_ICON_WIDTH={}", image.width());
    println!(
        "cargo:rustc-env=FYLER_WINDOW_ICON_HEIGHT={}",
        image.height()
    );
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", icon_path.display());
}
