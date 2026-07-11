use embed_manifest::manifest::Setting;
use embed_manifest::{embed_manifest, new_manifest};
use std::path::PathBuf;

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();

    if target_os == "windows" {
        embed_manifest(new_manifest("fyler").long_path_aware(Setting::Enabled))
            .expect("Windowsアプリマニフェストを埋め込めません");
    }

    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest_dir.join("../../assets/fyler.ico");
    // windows-gnuクロスCIにはwindresがないため、製品情報はMSVCビルドだけに埋め込む。
    if target_os == "windows" && target_env == "msvc" {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon(icon.to_str().expect("アイコンのパスがUTF-8ではありません"))
            .set("ProductName", "fyler")
            .set("FileDescription", "fyler - Neovim-powered file manager")
            .set("OriginalFilename", "fyler.exe")
            .set("LegalCopyright", "Copyright (c) 2026 sg004baa")
            .set("CompanyName", "sg004baa")
            .compile()
            .expect("Windows EXEリソースを埋め込めません");
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", icon.display());
}
