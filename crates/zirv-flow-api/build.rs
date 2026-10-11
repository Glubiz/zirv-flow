//! Picks the folder the web UI is embedded from: the built app when it exists,
//! else the committed placeholder, so a checkout without Node still builds.

use std::path::Path;

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let manifest = Path::new(&manifest);
    let built = manifest.join("../../web/dist");
    let placeholder = manifest.join("web-placeholder");

    println!("cargo:rerun-if-changed={}", built.display());
    println!("cargo:rerun-if-changed={}", placeholder.display());

    let root = if built.join("index.html").is_file() {
        built
    } else {
        placeholder
    };
    println!("cargo:rustc-env=ZIRV_FLOW_WEB_ROOT={}", root.display());
}
