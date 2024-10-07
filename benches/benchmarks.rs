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

criterion_group!(benches, bench_links_lookup);
criterion_main!(benches);
