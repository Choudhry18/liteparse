//! Image bytes, resolved per part and shared per package path.
//!
//! ## Why this is keyed two ways
//!
//! A blip names its image with an `r:embed`, and an `r:id` is scoped to the
//! part that writes it: a slide's `rId1`, its layout's `rId1` and its master's
//! `rId1` are three unrelated relationships, and on this corpus they routinely
//! name three different images. So the painter needs a **per-part** table
//! ([`PartMedia`]), or an inherited picture silently acquires whatever the
//! slide's `rId1` happens to point at — a real photograph, from the wrong
//! relationship, which no "did it resolve?" gate can see.
//!
//! The *bytes*, on the other hand, are shared. One logo referenced by 60
//! slides is one media part, and [`MediaEntry`]'s `Arc` identity is what the
//! painter's bitmap cache keys on, so each part must become exactly one `Arc`
//! however many relationships point at it. Hence `by_path` under `by_part`:
//! the first deduplicates bytes, the second scopes ids.
//!
//! ## Nothing is read until something asks for it
//!
//! `PackageContents` holds the whole zip, and the corpus's media is 378 MB
//! across 45 decks — of which the deck-wide totals are misleading, because
//! 139 `.svg` and 23 `.wdp` parts are never a blip target at all (Office
//! writes them as alternatives and effect backups). Copying every media part
//! into an `Arc` up front would pay for all of them. Resolution is therefore
//! lazy per path, and a part that no blip reaches is never copied.
//!
//! A **negative** result is cached too, as `None`. A missing or undecodable
//! target is a property of the package, not of the caller asking, so the
//! second of 60 references must not re-walk the zip to rediscover it.

use std::collections::HashMap;
use std::sync::Arc;

use crate::docx::relationships::{RelationshipType, TargetMode};
use crate::model::{ImageFormat, RelId};
use crate::render::resolve::images::{MediaEntry, PartMedia};

use super::package::{Part, PresentationPackage};

/// Per-part `r:embed` tables over a shared pool of image bytes.
#[derive(Default)]
pub struct MediaCache {
    /// Package path → the bytes behind it, `None` when the package has no such
    /// part. One `Arc` per path, which is the identity the painter caches on.
    by_path: HashMap<String, Option<MediaEntry>>,
    /// Part path → that part's own image relationships, resolved.
    by_part: HashMap<String, PartMedia>,
    /// Handed out for a part that does not exist — a slide with no reachable
    /// master, say. A borrowable empty table rather than an `Option` so that
    /// every paint site takes the same code path, and "this rung has no media"
    /// cannot accidentally become "resolve against whatever is in scope".
    empty: PartMedia,
}

impl MediaCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The image relationships `part` itself declares, built once per part.
    ///
    /// Only `RelationshipType::Image` relationships are included. Filtering on
    /// the declared type rather than on "did the target decode?" keeps this
    /// table an answer about the *package* — a slide's layout rel and its
    /// notes rel resolve to real bytes too, and folding them in would make a
    /// missing image indistinguishable from an id that never named one.
    pub fn part_media(&mut self, pkg: &PresentationPackage, part: &Part) -> &PartMedia {
        if !self.by_part.contains_key(&part.path) {
            let mut table = PartMedia::new();
            for rel in part.rels.all() {
                if rel.rel_type != RelationshipType::Image {
                    continue;
                }
                // §15.2.10 external: the bytes live outside the package, so
                // there is nothing to put in the table. Left absent rather
                // than inserted-as-missing; the caller's "not declared" and
                // "declared elsewhere" both correctly paint nothing.
                if rel.target_mode == TargetMode::External {
                    continue;
                }
                let Some(path) = part.resolve_rel(rel.id.as_str()) else {
                    continue;
                };
                if let Some(entry) = Self::load(&mut self.by_path, pkg, &path) {
                    table.insert(rel.id.clone(), entry);
                }
            }
            self.by_part.insert(part.path.clone(), table);
        }
        &self.by_part[&part.path]
    }

    /// One media part's bytes, read and format-detected at most once.
    ///
    /// Format detection goes through [`ImageFormat::detect`], which tries the
    /// extension and *then* the magic bytes. That fallback is load-bearing
    /// here rather than defensive: 21 corpus blips point at `.jfif` parts,
    /// which are byte-for-byte JPEG and which an extension-only reading drops.
    /// Undecodable formats (EMF/WMF/SVG) are still entered — the painter is
    /// the layer that knows what it can rasterize, and a resolver that hid
    /// them here would report them as missing images instead of unsupported
    /// ones.
    fn load(
        by_path: &mut HashMap<String, Option<MediaEntry>>,
        pkg: &PresentationPackage,
        path: &str,
    ) -> Option<MediaEntry> {
        if !by_path.contains_key(path) {
            let entry = pkg.package.get_part(path).map(|bytes| MediaEntry {
                data: Arc::from(bytes),
                format: ImageFormat::detect(path, bytes),
            });
            by_path.insert(path.to_string(), entry);
        }
        by_path[path].clone()
    }

    /// A part's already-built table. Separate from [`Self::part_media`] so a
    /// caller can prime several parts and *then* borrow them all at once,
    /// which `&mut self` on the builder forbids.
    ///
    /// Falls back to an empty table rather than `None`: see the `empty` field.
    pub fn get(&self, part_path: Option<&str>) -> &PartMedia {
        part_path
            .and_then(|p| self.by_part.get(p))
            .unwrap_or(&self.empty)
    }

    /// How many distinct media parts have been read into memory. Reported by
    /// the probe so that "the deck has images" and "the painter reached them"
    /// stay separate claims.
    pub fn parts_loaded(&self) -> usize {
        self.by_path.values().filter(|v| v.is_some()).count()
    }

    /// Package paths a relationship named that the package does not contain.
    pub fn parts_missing(&self) -> usize {
        self.by_path.values().filter(|v| v.is_none()).count()
    }
}

/// Look one `r:embed` up in a part's table. A thin helper so callers do not
/// have to build a [`RelId`] to ask.
pub fn media_entry<'a>(media: &'a PartMedia, embed: &str) -> Option<&'a MediaEntry> {
    media.get(&RelId::new(embed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jfif_extension_falls_through_to_magic_bytes() {
        // The 21-reference case: an extension OOXML §M.1.1 does not list, over
        // bytes that are plainly JPEG. Extension-only detection returns
        // `Unknown` and the painter drops the image.
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F'];
        assert_eq!(
            ImageFormat::detect("ppt/media/image7.jfif", &jpeg),
            ImageFormat::Jpeg
        );
    }
}
