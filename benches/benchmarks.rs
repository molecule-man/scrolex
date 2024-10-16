use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::hint::black_box;

use scrolex::poppler::{Link, LinkMappingExt};

fn link_lookup_naive(page: &poppler::Page, x: f64, y: f64) -> bool {
    let raw_links = page.link_mapping();

    for raw_link in raw_links {
        let Link(_, area) = raw_link.to_link();

        if area.x1() <= x && x <= area.x2() && area.y1() <= y && y <= area.y2() {
            return true;
        }
    }
    false
}

fn link_lookup_cached(
    lookup: &mut scrolex::links::Links,
    page: &poppler::Page,
    x: f64,
    y: f64,
) -> bool {
    lookup.get_link(page, x, y).is_some()
}

pub fn bench_links_lookup(c: &mut Criterion) {
    let pdf_path = std::env::var("PDF_PATH").expect("Environment variable PDF_PATH is not set");

    let page_number: i32 = std::env::var("PAGE_NUMBER")
        .expect("Environment variable PAGE_NUMBER is not set")
        .parse()
        .expect("PAGE_NUMBER must be a valid integer");

    let doc = poppler::Document::from_file(&format!("file://{pdf_path}"), None).unwrap();
    let page = doc.page(page_number).unwrap();
    let lookup = &mut scrolex::links::Links::default();

    let mut group = c.benchmark_group("links_lookup");
    group.throughput(Throughput::Elements(1));

    group.bench_function(format!("naive {pdf_path} page {page_number} 300"), |b| {
        b.iter(|| link_lookup_naive(&page, black_box(300.0), black_box(300.0)))
    });

    group.bench_function(format!("cached {pdf_path} page {page_number} 300"), |b| {
        b.iter(|| link_lookup_cached(lookup, &page, black_box(300.0), black_box(300.0)))
    });

    group.finish();
}

fn draw_half_page(page: &poppler::Page) -> gtk::cairo::ImageSurface {
    let (width, height) = page.size();
    let height = height / 2.0;

    let surface =
        gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, width as i32, height as i32)
            .expect("Couldn't create a surface!");

    let cr = gtk::cairo::Context::new(&surface).expect("Couldn't create a context!");
    cr.rectangle(0.0, 0.0, width, height);
    cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
    cr.fill().expect("Failed to fill");
    let mut old_rect = poppler::Rectangle::new();
    let mut rect = poppler::Rectangle::new();
    rect.set_x1(0.0);
    rect.set_y1(0.0);
    rect.set_x2(width);
    rect.set_y2(height);
    page.render_selection(
        &cr,
        &mut rect,
        &mut old_rect,
        poppler::SelectionStyle::Glyph,
        &mut poppler::Color::new(),
        &mut poppler::Color::new(),
    );

    surface
}

pub fn bench_render_surface(c: &mut Criterion) {
    let pdf_path = std::env::var("PDF_PATH").expect("Environment variable PDF_PATH is not set");

    let page_number: i32 = std::env::var("PAGE_NUMBER")
        .expect("Environment variable PAGE_NUMBER is not set")
        .parse()
        .expect("PAGE_NUMBER must be a valid integer");

    let doc = poppler::Document::from_file(&format!("file://{pdf_path}"), None).unwrap();
    let page = doc.page(page_number).unwrap();
    let (width, height) = page.size();

    let mut group = c.benchmark_group("render_surface");
    group.throughput(Throughput::Elements(1));

    group.bench_function(format!("half-page {pdf_path} page {page_number}"), |b| {
        b.iter(|| draw_half_page(&page))
    });

    group.bench_function(
        format!("full ({width}x{height}) {pdf_path} page {page_number}"),
        |b| b.iter(|| scrolex::page::render_surface(&page, 1.0)),
    );

    group.bench_function(
        format!("downscaled 1/4 {pdf_path} page {page_number}"),
        |b| b.iter(|| scrolex::page::render_surface(&page, 0.25)),
    );

    group.bench_function(format!("upscaled x4 {pdf_path} page {page_number}"), |b| {
        b.iter(|| scrolex::page::render_surface(&page, 4.0))
    });

    let surface = scrolex::page::render_surface(&page, 1.0);
    let cr = gtk::cairo::Context::new(&surface).unwrap();
    let bbox = scrolex::page::Rectangle::from(cr.clip_extents().unwrap());
    cr.set_source_rgb(1.0, 1.0, 1.0);

    group.bench_function(
        format!("draw pre-rendered {pdf_path} page {page_number}"),
        |b| {
            b.iter(|| {
                scrolex::page::draw_surface(&cr, &surface, &bbox, 1.0, 1.0);
            })
        },
    );

    group.finish();
}

criterion_group!(benches, bench_links_lookup, bench_render_surface);
criterion_main!(benches);
