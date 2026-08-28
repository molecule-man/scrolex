use futures::channel::oneshot;
use gtk::prelude::FileExt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::mupdf_render::{content_x_span, CONTENT_SCAN_VERSION};

const MAGIC: &[u8; 4] = b"SXCB";
const HEADER_LEN: usize = 32;
const SPAN_LEN: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    size: u64,
    mtime_seconds: i64,
    mtime_nanoseconds: u32,
}

impl Identity {
    fn read(uri: &str) -> Option<Self> {
        let path = gtk::gio::File::for_uri(uri).path()?;
        let metadata = fs::metadata(path).ok()?;
        let (mtime_seconds, mtime_nanoseconds) = timestamp(metadata.modified().ok()?)?;
        Some(Self {
            size: metadata.len(),
            mtime_seconds,
            mtime_nanoseconds,
        })
    }
}

fn timestamp(time: SystemTime) -> Option<(i64, u32)> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => Some((
            i64::try_from(duration.as_secs()).ok()?,
            duration.subsec_nanos(),
        )),
        Err(error) => {
            let duration = error.duration();
            let seconds = i64::try_from(duration.as_secs()).ok()?;
            if duration.subsec_nanos() == 0 {
                Some((seconds.checked_neg()?, 0))
            } else {
                Some((
                    seconds.checked_add(1)?.checked_neg()?,
                    1_000_000_000 - duration.subsec_nanos(),
                ))
            }
        }
    }
}

pub(crate) fn path(uri: &str) -> Option<PathBuf> {
    gtk::gio::File::for_uri(uri).path()?;
    let mut path = crate::viewport::state_dir();
    path.push("pdf-viewer");
    path.push(crate::viewport::uri_components(uri));
    let file_name = path.file_name()?.to_os_string();
    let mut cache_name = file_name;
    cache_name.push(".crop");
    path.set_file_name(cache_name);
    Some(path)
}

pub(crate) fn read(uri: &str, cache_path: &Path, n_pages: i32) -> Option<Vec<Option<(f64, f64)>>> {
    let identity = Identity::read(uri)?;
    let page_count = usize::try_from(n_pages).ok()?;
    let expected_len = page_count.checked_mul(SPAN_LEN)?.checked_add(HEADER_LEN)?;
    let bytes = fs::read(cache_path).ok()?;
    if bytes.len() != expected_len || bytes.get(0..4)? != MAGIC {
        return None;
    }
    if u16::from_le_bytes(bytes.get(4..6)?.try_into().ok()?) != CONTENT_SCAN_VERSION
        || u16::from_le_bytes(bytes.get(6..8)?.try_into().ok()?) != 0
        || u64::from_le_bytes(bytes.get(8..16)?.try_into().ok()?) != identity.size
        || i64::from_le_bytes(bytes.get(16..24)?.try_into().ok()?) != identity.mtime_seconds
        || u32::from_le_bytes(bytes.get(24..28)?.try_into().ok()?) != identity.mtime_nanoseconds
        || u32::from_le_bytes(bytes.get(28..32)?.try_into().ok()?) != 0
    {
        return None;
    }

    let spans = bytes[HEADER_LEN..]
        .as_chunks::<SPAN_LEN>()
        .0
        .iter()
        .map(|span| {
            let x1 = u16::from_le_bytes(span[0..2].try_into().ok()?);
            let x2 = u16::from_le_bytes(span[2..4].try_into().ok()?);
            Some((x2 != 0).then_some((f64::from(x1), f64::from(x2))))
        })
        .collect::<Option<Vec<_>>>()?;
    log::info!(
        "Loaded crop cache {}: {page_count} pages",
        cache_path.display()
    );
    Some(spans)
}

pub(crate) fn spawn_sweep(
    uri: String,
    n_pages: i32,
    cache_path: PathBuf,
) -> oneshot::Receiver<Vec<Option<(f64, f64)>>> {
    let (sender, receiver) = oneshot::channel();
    std::thread::spawn(move || {
        let Ok(page_count) = usize::try_from(n_pages) else {
            return;
        };
        let Some(before) = Identity::read(&uri) else {
            return;
        };
        if crate::mupdf_render::with_doc(&uri, |_| Some(())).is_none() {
            return;
        }
        let spans: Vec<_> = (0..page_count)
            .map(|page| {
                i32::try_from(page)
                    .ok()
                    .and_then(|page| content_x_span(&uri, page))
            })
            .collect();
        if Identity::read(&uri) != Some(before) {
            return;
        }
        match write(&cache_path, before, &spans) {
            Ok(()) => log::info!(
                "Wrote crop cache {}: {page_count} pages",
                cache_path.display()
            ),
            Err(error) => log::error!(
                "Crop cache write failed for {}: {error}",
                cache_path.display()
            ),
        }
        let _ = sender.send(spans);
    });
    receiver
}

fn write(cache_path: &Path, identity: Identity, spans: &[Option<(f64, f64)>]) -> io::Result<()> {
    let capacity = spans
        .len()
        .checked_mul(SPAN_LEN)
        .and_then(|length| length.checked_add(HEADER_LEN))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "crop cache is too large"))?;
    let mut bytes = Vec::with_capacity(capacity);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&CONTENT_SCAN_VERSION.to_le_bytes());
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes.extend_from_slice(&identity.size.to_le_bytes());
    bytes.extend_from_slice(&identity.mtime_seconds.to_le_bytes());
    bytes.extend_from_slice(&identity.mtime_nanoseconds.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    for span in spans {
        let (x1, x2) = span.and_then(stored_span).unwrap_or((0, 0));
        bytes.extend_from_slice(&x1.to_le_bytes());
        bytes.extend_from_slice(&x2.to_le_bytes());
    }

    let parent = cache_path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "crop cache has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut output = tempfile::NamedTempFile::new_in(parent)?;
    output.write_all(&bytes)?;
    output.flush()?;
    output.persist(cache_path).map_err(|error| error.error)?;
    Ok(())
}

fn stored_span((x1, x2): (f64, f64)) -> Option<(u16, u16)> {
    let x1 = x1.floor();
    let x2 = x2.ceil();
    if !x1.is_finite()
        || !x2.is_finite()
        || x1 < 0.0
        || x2 <= x1
        || x1 > f64::from(u16::MAX)
        || x2 > f64::from(u16::MAX)
    {
        return None;
    }
    Some((x1 as u16, x2 as u16))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_uri(name: &str) -> String {
        gtk::gio::File::for_path(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .uri()
        .to_string()
    }

    #[test]
    fn spans_round_trip_with_outward_rounding() {
        let uri = fixture_uri("outline.pdf");
        let dir = tempfile::tempdir().expect("cache directory");
        let cache_path = dir.path().join("outline.pdf.crop");
        let spans = vec![None, Some((1.2, 8.1)), Some((0.0, 70_000.0))];

        write(
            &cache_path,
            Identity::read(&uri).expect("source identity"),
            &spans,
        )
        .expect("write cache");

        assert_eq!(
            read(&uri, &cache_path, 3),
            Some(vec![None, Some((1.0, 9.0)), None])
        );
    }

    #[test]
    fn corrupt_or_wrong_length_cache_is_rejected() {
        let uri = fixture_uri("outline.pdf");
        let dir = tempfile::tempdir().expect("cache directory");
        let cache_path = dir.path().join("outline.pdf.crop");
        write(
            &cache_path,
            Identity::read(&uri).expect("source identity"),
            &[None; 3],
        )
        .expect("write cache");
        assert_eq!(read(&uri, &cache_path, 4), None);

        fs::write(&cache_path, [0_u8; HEADER_LEN]).expect("write corrupt cache");

        assert_eq!(read(&uri, &cache_path, 0), None);
    }

    #[test]
    fn identity_and_version_mismatches_are_rejected() {
        let uri = fixture_uri("outline.pdf");
        let dir = tempfile::tempdir().expect("cache directory");
        let cache_path = dir.path().join("outline.pdf.crop");
        let identity = Identity::read(&uri).expect("source identity");
        write(&cache_path, identity, &[None; 3]).expect("write cache");

        let mut bytes = fs::read(&cache_path).expect("read cache");
        bytes[4..6].copy_from_slice(&(CONTENT_SCAN_VERSION + 1).to_le_bytes());
        fs::write(&cache_path, &bytes).expect("write version mismatch");
        assert_eq!(read(&uri, &cache_path, 3), None);

        bytes[4..6].copy_from_slice(&CONTENT_SCAN_VERSION.to_le_bytes());
        bytes[8..16].copy_from_slice(&(identity.size + 1).to_le_bytes());
        fs::write(&cache_path, bytes).expect("write identity mismatch");
        assert_eq!(read(&uri, &cache_path, 3), None);
    }

    #[test]
    fn sweep_file_matches_direct_scans() {
        let uri = fixture_uri("outline.pdf");
        let dir = tempfile::tempdir().expect("cache directory");
        let cache_path = dir.path().join("outline.pdf.crop");
        let direct: Vec<_> = (0..3).map(|page| content_x_span(&uri, page)).collect();

        let swept = futures::executor::block_on(spawn_sweep(uri.clone(), 3, cache_path.clone()))
            .expect("sweep result");

        assert_eq!(swept, direct);
        assert_eq!(read(&uri, &cache_path, 3), Some(direct));
    }

    #[test]
    fn worker_uses_the_resolved_scratch_path() {
        crate::viewport::use_scratch_state_dir();
        let scratch = crate::viewport::state_dir();
        let uri = fixture_uri("no_outline.pdf");
        let cache_path = path(&uri).expect("local cache path");
        assert!(cache_path.starts_with(&scratch));

        futures::executor::block_on(spawn_sweep(uri, 1, cache_path.clone())).expect("sweep result");

        assert!(cache_path.exists());
    }
}
