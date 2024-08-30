fn main() {
    glib_build_tools::compile_resources(&["ui"], "ui/ui.gresource.xml", "hallyview-ui.gresource");
}
