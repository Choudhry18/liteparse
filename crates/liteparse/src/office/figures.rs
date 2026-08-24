//! The figure/image sink shared by the native office paths.
//!
//! Assigns figure ids and accumulates the bytes behind them, document-wide.
//! Grown in `office/pptx.rs` and moved here unchanged when the XLSX path
//! needed the same dedup; the id *scope* is the one generalization — PPTX
//! passes `p{page}` (a slide is a page), XLSX passes `s{sheet}` (a sheet's
//! pictures stay numbered together however its rows paginate).
//!
//! Ids follow the platform extractor's `{scope}_{n}` naming, 1-based, so
//! `img_{id}.{ext}` file names line up with the PDF and DOCX paths'.
//!
//! **`n` counts in reading order, not draw order.** Draw order is available —
//! it is source order — but using it would mean a second walk purely to
//! number things, and reading order is the order the `![](…)` refs appear in
//! the markdown, so a reader scanning down the page sees `_1` before `_2`.
//! The platform's caveat about index drift (its extractor increments even for
//! images it skips) already means these indices are names, not positions.

use std::collections::HashMap;

use liteparse_ooxml::model::ImageFormat;

use crate::types::{ExtractedImage, Rect};

#[derive(Default)]
pub(crate) struct FigureSink {
    pub(crate) images: Vec<ExtractedImage>,
    /// Same source allocation ⇒ same bytes. Free dedup for the repeated-logo
    /// case — on the PPTX corpus, 3,602 placements behind 1,322 distinct
    /// images — because both readers pool media bytes per package path, so a
    /// part referenced twice shares one buffer and hits here rather than in
    /// the hash map below.
    by_ptr: HashMap<usize, usize>,
    /// Distinct rels can still hold identical bytes; hash → candidate
    /// canonical indices, confirmed by full compare like the DOCX path's.
    by_hash: HashMap<u64, Vec<usize>>,
    /// 1-based within the current scope, reset by
    /// [`FigureSink::reset_ordinal`].
    n: u32,
}

impl FigureSink {
    /// Restart `n` — at each slide for PPTX, each sheet for XLSX.
    pub(crate) fn reset_ordinal(&mut self) {
        self.n = 0;
    }

    /// Record one placed picture and return the `(id, extension)` its
    /// `Block::Figure` should carry, or `None` for media we do not surface
    /// bytes for (EMF/WMF/SVG with no raster fallback), which then consumes
    /// no id — a ref numbered around skipped media would name a file nothing
    /// writes.
    pub(crate) fn place(
        &mut self,
        data: &[u8],
        format: ImageFormat,
        scope: &str,
        page: u32,
        bbox: Rect,
    ) -> Option<(String, String)> {
        use std::hash::{Hash, Hasher};

        let ext = super::docx_layout::media_extension(format)?;
        self.n += 1;
        let id = format!("{scope}_{}", self.n);
        let ptr = data.as_ptr() as usize;

        let canonical = self.by_ptr.get(&ptr).copied().or_else(|| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            data.hash(&mut h);
            self.by_hash
                .get(&h.finish())?
                .iter()
                .copied()
                .find(|&i| *self.images[i].bytes == *data)
        });

        let entry = match canonical {
            Some(ci) => {
                let canonical = &self.images[ci];
                ExtractedImage {
                    id: id.clone(),
                    name: format!("img_{id}.{ext}"),
                    path: None,
                    page,
                    bbox,
                    width: canonical.width,
                    height: canonical.height,
                    rotation: 0.0,
                    format: ext.to_string(),
                    duplicate_of: Some(canonical.id.clone()),
                    bytes: std::sync::Arc::clone(&canonical.bytes),
                }
            }
            None => {
                let bytes = std::sync::Arc::new(data.to_vec());
                // The *natural* size of the image, not the placed box —
                // `bbox` already carries the placement. A crop is not applied
                // to either: `src_rect` describes a visible sub-rectangle the
                // bytes still contain, and 44% of PPTX corpus pictures
                // declare one, so re-encoding to honour it would re-encode
                // nearly half the corpus to change a number no consumer
                // reads.
                let (width, height) =
                    image::ImageReader::new(std::io::Cursor::new(bytes.as_slice()))
                        .with_guessed_format()
                        .ok()
                        .and_then(|r| r.into_dimensions().ok())
                        .unwrap_or((0, 0));
                let mut h = std::collections::hash_map::DefaultHasher::new();
                data.hash(&mut h);
                self.by_hash
                    .entry(h.finish())
                    .or_default()
                    .push(self.images.len());
                self.by_ptr.insert(ptr, self.images.len());
                ExtractedImage {
                    id: id.clone(),
                    name: format!("img_{id}.{ext}"),
                    path: None,
                    page,
                    bbox,
                    width,
                    height,
                    rotation: 0.0,
                    format: ext.to_string(),
                    duplicate_of: None,
                    bytes,
                }
            }
        };
        self.images.push(entry);
        Some((id, ext.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect {
            x: 1.0,
            y: 2.0,
            width: 3.0,
            height: 4.0,
        }
    }

    /// A 1x1 PNG, so `image::ImageReader` reports real dimensions.
    fn png() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
    }

    /// Ids are `{scope}_{n}`, 1-based on both halves and restarted per scope,
    /// so they line up with the platform extractor's and the PDF path's.
    #[test]
    fn figure_ids_are_scope_local_and_one_based() {
        let mut sink = FigureSink::default();
        let a = png();
        let b = {
            let mut v = png();
            v.extend_from_slice(&[0u8; 4]);
            v
        };
        sink.reset_ordinal();
        let place = |s: &mut FigureSink, d: &[u8], scope: &str, page| {
            s.place(d, ImageFormat::Png, scope, page, rect())
        };
        assert_eq!(place(&mut sink, &a, "p1", 1).unwrap().0, "p1_1");
        assert_eq!(place(&mut sink, &b, "p1", 1).unwrap().0, "p1_2");
        sink.reset_ordinal();
        assert_eq!(place(&mut sink, &b, "p2", 2).unwrap().0, "p2_1");
    }

    /// The repeated-logo case: every placement keeps its own name, and all
    /// but the first point at one canonical entry's bytes.
    #[test]
    fn a_repeated_picture_collapses_onto_one_canonical_entry() {
        let mut sink = FigureSink::default();
        let logo = png();
        for page in 1..=3u32 {
            sink.reset_ordinal();
            sink.place(&logo, ImageFormat::Png, &format!("p{page}"), page, rect())
                .expect("placed");
        }
        assert_eq!(sink.images.len(), 3, "one entry per placement");
        assert_eq!(sink.images[0].duplicate_of, None);
        assert_eq!(
            sink.images[1].duplicate_of.as_deref(),
            Some("p1_1"),
            "later placements name the canonical id"
        );
        assert_eq!(sink.images[2].duplicate_of.as_deref(), Some("p1_1"));
        // Distinct names, shared bytes — the platform's contract exactly.
        assert_eq!(sink.images[1].name, "img_p2_1.png");
        assert!(std::sync::Arc::ptr_eq(
            &sink.images[0].bytes,
            &sink.images[2].bytes
        ));
    }

    /// Two *different* source buffers can hold identical bytes — a logo
    /// pasted into a slide and into its master. The pointers differ, so only
    /// the content hash catches this one.
    #[test]
    fn identical_bytes_behind_different_buffers_still_dedupe() {
        let mut sink = FigureSink::default();
        let one = png();
        let two = png();
        assert_ne!(one.as_ptr(), two.as_ptr(), "distinct allocations");
        sink.reset_ordinal();
        sink.place(&one, ImageFormat::Png, "p1", 1, rect())
            .expect("placed");
        sink.place(&two, ImageFormat::Png, "p1", 1, rect())
            .expect("placed");
        assert_eq!(sink.images[1].duplicate_of.as_deref(), Some("p1_1"));
    }

    /// Media we do not surface bytes for is not a figure and does not consume
    /// an id — EMF references and SVG-only blips land here, and a ref
    /// numbered around them would name a file nothing writes.
    #[test]
    fn media_we_cannot_surface_takes_no_id() {
        let mut sink = FigureSink::default();
        let bytes = png();
        sink.reset_ordinal();
        assert!(
            sink.place(&bytes, ImageFormat::Emf, "p1", 1, rect())
                .is_none()
        );
        assert!(sink.images.is_empty());
        // The next real picture is still `_1`.
        assert_eq!(
            sink.place(&bytes, ImageFormat::Png, "p1", 1, rect())
                .unwrap()
                .0,
            "p1_1"
        );
    }
}
