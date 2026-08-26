#[test]
fn content_bbox_covers_the_emulated_page() {
    std::env::set_var("SCROLEX_EMULATE", "1");
    std::env::set_var("SCROLEX_EMULATE_PAGE_PT", "600x800");

    assert_eq!(
        scrolex::mupdf_render::content_bbox(scrolex::emulate::URI, 0),
        Some((0.0, 0.0, 600.0, 800.0))
    );
}
