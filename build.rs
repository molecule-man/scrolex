fn main() {
    glib_build_tools::compile_resources(
        &["ui", "resources"],
        "ui/ui.gresource.xml",
        "scrolex-ui.gresource",
    );
}
