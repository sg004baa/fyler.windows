use embed_manifest::manifest::Setting;
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_manifest(new_manifest("fyler").long_path_aware(Setting::Enabled))
            .expect("Windowsアプリマニフェストを埋め込めません");
    }

    println!("cargo:rerun-if-changed=build.rs");
}
