// Criterion benchmarks for document rendering.
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::hint::black_box;

pub fn bench_render_surface(c: &mut Criterion) {
    let Ok(pdf_path) = std::env::var("PDF_PATH") else {
        return;
    };

    let page_number: i32 = std::env::var("PAGE_NUMBER")
        .expect("Environment variable PAGE_NUMBER is not set")
        .parse()
        .expect("PAGE_NUMBER must be a valid integer");

    let uri = format!("file://{pdf_path}");

    let mut group = c.benchmark_group("render_surface");
    group.throughput(Throughput::Elements(1));

    for (label, scale) in [
        ("full", 1.0),
        ("downscaled 1/4", 0.25),
        ("upscaled x4", 4.0),
    ] {
        group.bench_function(format!("{label} {pdf_path} page {page_number}"), |b| {
            b.iter(|| {
                black_box(scrolex::mupdf_render::render_page_surface(
                    &uri,
                    page_number,
                    scale,
                    1.0,
                    None,
                ))
            })
        });
    }

    group.finish();
}

criterion_group!(benches, bench_render_surface);
criterion_main!(benches);
