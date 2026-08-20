use std::path::Path;

fn main() {
    glib_build_tools::compile_resources(
        &["ui", "resources"],
        "ui/ui.gresource.xml",
        "scrolex-ui.gresource",
    );
    embed_windows_manifest();
}

// A rust binary carries no resource section, so Windows finds no dpi declaration and treats
// the app as unaware. The msvc linker embeds the manifest without an extra build tool.
fn embed_windows_manifest() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let root = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets the manifest dir");
    let manifest = Path::new(&root).join("resources/windows/scrolex.manifest");
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.display()
    );
}
