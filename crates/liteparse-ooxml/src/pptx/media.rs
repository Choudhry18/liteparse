//! Image bytes, resolved per part and shared per package path.
//!
//! ## Why this is keyed two ways
//!
//! A blip names its image with an `r:embed`, and an `r:id` is scoped to the
//! part that writes it: a slide's `rId1`, its layout's `rId1` and its
//! master's `rId1` are unrelated relationships that can each point at a
//! different image. The painter therefore needs a **per-part** table
//! ([`PartMedia`]) — otherwise an inherited picture could silently pick up
//! whatever the slide's `rId1` happens to name instead of its own target.
//!
//! The *bytes*, on the other hand, are shared: one logo referenced from many
//! slides is one media part, and [`MediaEntry`]'s `Arc` identity is what the
//! painter's bitmap cache keys on, so each part must become exactly one
//! `Arc` no matter how many relationships point at it. Hence `by_path`
//! nested under `by_part`: the first deduplicates bytes, the second scopes
//! ids.
//!
//! ## Nothing is read until something asks for it
//!
//! Media parts (fonts, SVG alternatives, effect backups) can be large and
//! most are never a blip target. Resolution is lazy per path — a part that
//! no blip reaches is never copied into memory — and a missing or
//! undecodable target is cached as `None` so repeated references don't
//! re-walk the zip to rediscover it.

use std::collections::HashMap;
use std::sync::Arc;

use crate::docx::relationships::{RelationshipType, TargetMode};
use crate::model::ImageFormat;
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
    /// every paint site takes the same code path, and "no media here" cannot
    /// accidentally fall back to "resolve against whatever is in scope".
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
    /// extension and then falls back to the magic bytes — needed because
    /// some `.jfif` parts are byte-for-byte JPEG and extension-only sniffing
    /// misses them. Undecodable formats (EMF/WMF/SVG) are still entered: the
    /// painter is the layer that knows what it can rasterize, and hiding
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jfif_extension_falls_through_to_magic_bytes() {
        // `.jfif` is an extension OOXML §M.1.1 does not list, over bytes that
        // are plainly JPEG. Extension-only detection would return `Unknown`
        // and the painter would drop the image.
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F'];
        assert_eq!(
            ImageFormat::detect("ppt/media/image7.jfif", &jpeg),
            ImageFormat::Jpeg
        );
    }
}
