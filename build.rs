fn main() {
    glib_build_tools::compile_resources(&["ui"], "ui/ui.gresource.xml", "scrolex-ui.gresource");

    let mut cc = cc::Build::new();

    let lib = pkg_config::Config::new().probe("poppler-glib").unwrap();
    for path in lib.include_paths {
        cc.include(path);
    }
    cc.cpp(true)
        .file("./cpp/poppler.cc")
        .flag_if_supported("-std=c++20")
        .compile("poppler_wrapper");

    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());

    println!("cargo:rustc-link-lib=dylib=poppler-cpp");
    println!("cargo:rustc-link-lib=dylib=poppler-glib");
    println!("cargo:rustc-link-lib=dylib=cairo");

    println!("cargo:rerun-if-changed=cpp/poppler.cc");
    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=poppler_wrapper");
}
