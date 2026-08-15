//! ZIP extraction and OOXML package-path resolution.

use std::collections::HashMap;
use std::io::Read;

use crate::docx::error::{ParseError, Result};
use crate::docx::whitespace_workaround::substitute_whitespace_only_runs;

/// The contents of a DOCX package, extracted from the ZIP archive.
pub struct PackageContents {
    /// All files in the ZIP, keyed by normalized path (no leading slash).
    pub parts: HashMap<String, Vec<u8>>,
}

impl PackageContents {
    /// Extract all parts from a DOCX ZIP archive.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let cursor = std::io::Cursor::new(data);
        let mut archive = zip::ZipArchive::new(cursor)?;
        let mut parts = HashMap::with_capacity(archive.len());

        for i in 0..archive.len() {
            let mut file = archive.by_index(i)?;
            let name = normalize_path(file.name());
            let mut buf = Vec::with_capacity(file.size() as usize);
            file.read_to_end(&mut buf)?;
            // Apply the whitespace workaround only to XML parts. Binary parts
            // (images, fonts, embedded OLE) must not be touched.
            // See `whitespace_workaround` module docs for the rationale.
            if name.ends_with(".xml") || name.ends_with(".rels") {
                buf = substitute_whitespace_only_runs(&buf);
            }
            parts.insert(name, buf);
        }

        Ok(Self { parts })
    }

    /// Get the bytes for a part, case-insensitively.
    pub fn get_part(&self, path: &str) -> Option<&[u8]> {
        let normalized = normalize_path(path);
        self.parts.get(&normalized).map(|v| v.as_slice())
    }

    /// Get part bytes, or return MissingPart error.
    pub fn require_part(&self, path: &str) -> Result<&[u8]> {
        self.get_part(path)
            .ok_or_else(|| ParseError::MissingPart(path.to_string()))
    }

    /// Remove and return the owned bytes for a part. Avoids cloning.
    pub fn take_part(&mut self, path: &str) -> Option<Vec<u8>> {
        let normalized = normalize_path(path);
        self.parts.remove(&normalized)
    }
}

fn normalize_path(path: &str) -> String {
    path.trim_start_matches('/').to_lowercase()
}

/// Resolve a relationship target to an absolute path within the package.
/// base_dir is the directory containing the source part (e.g., "word" for "word/document.xml").
pub fn resolve_target(base_dir: &str, target: &str) -> String {
    if target.starts_with('/') {
        // Absolute path within the package
        normalize_path(target)
    } else {
        // Relative to base_dir
        let mut path = if base_dir.is_empty() {
            target.to_string()
        } else {
            format!("{}/{}", base_dir, target)
        };
        // Drop a leading "./" and collapse interior "/./" no-op segments so a
        // target like "./media/x.png" resolves the same as "media/x.png".
        if let Some(stripped) = path.strip_prefix("./") {
            path = stripped.to_string();
        }
        while let Some(pos) = path.find("/./") {
            path = format!("{}{}", &path[..pos], &path[pos + 2..]);
        }
        // Simplify "../" sequences
        while let Some(pos) = path.find("/../") {
            if let Some(parent_start) = path[..pos].rfind('/') {
                path = format!("{}{}", &path[..parent_start], &path[pos + 3..]);
            } else {
                path = path[pos + 4..].to_string();
            }
        }
        normalize_path(&path)
    }
}

/// Get the .rels path for a given part path.
/// e.g., "word/document.xml" → "word/_rels/document.xml.rels"
pub fn rels_path_for(part_path: &str) -> String {
    let normalized = normalize_path(part_path);
    if let Some(slash_pos) = normalized.rfind('/') {
        format!(
            "{}/_rels/{}.rels",
            &normalized[..slash_pos],
            &normalized[slash_pos + 1..]
        )
    } else {
        format!("_rels/{}.rels", normalized)
    }
}

/// Get the directory portion of a part path.
pub fn part_directory(part_path: &str) -> &str {
    match part_path.rfind('/') {
        Some(pos) => &part_path[..pos],
        None => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_leading_slash_and_lowercases() {
        assert_eq!(normalize_path("/Word/Document.XML"), "word/document.xml");
        assert_eq!(
            normalize_path("word/media/Image1.PNG"),
            "word/media/image1.png"
        );
    }

    #[test]
    fn resolve_relative_target_joins_base_dir() {
        assert_eq!(
            resolve_target("word", "media/image1.png"),
            "word/media/image1.png"
        );
    }

    #[test]
    fn resolve_absolute_target_ignores_base_dir() {
        assert_eq!(
            resolve_target("word", "/word/media/image1.png"),
            "word/media/image1.png"
        );
    }

    #[test]
    fn resolve_empty_base_dir_uses_target_as_is() {
        assert_eq!(resolve_target("", "document.xml"), "document.xml");
    }

    #[test]
    fn resolve_collapses_parent_sequences() {
        // A theme rel from word/ pointing at word/theme/theme1.xml via "../".
        assert_eq!(
            resolve_target("word/theme", "../media/image1.png"),
            "word/media/image1.png"
        );
        // Multiple "../" segments.
        assert_eq!(resolve_target("a/b/c", "../../x.xml"), "a/x.xml");
    }

    #[test]
    fn resolve_parent_past_root_leaves_residual_dotdot() {
        // Traversing above the package root is malformed. One "../" is consumed
        // against "word"; the second has no parent left, so a residual "../"
        // remains — a path that matches no part, which is the safe outcome for
        // malformed input (the image is simply not found).
        assert_eq!(resolve_target("word", "../../x.xml"), "../x.xml");
    }

    #[test]
    fn resolve_collapses_current_dir_segments() {
        // Leading "./" and interior "/./" are no-ops and must normalize away,
        // otherwise the part lookup (an exact map key) would miss.
        assert_eq!(resolve_target("word", "./media/x.png"), "word/media/x.png");
        assert_eq!(resolve_target("", "./document.xml"), "document.xml");
        assert_eq!(resolve_target("word", "media/./x.png"), "word/media/x.png");
    }

    #[test]
    fn rels_path_for_builds_sibling_rels_file() {
        assert_eq!(
            rels_path_for("word/document.xml"),
            "word/_rels/document.xml.rels"
        );
        // A root-level part has no directory prefix.
        assert_eq!(
            rels_path_for("[Content_Types].xml"),
            "_rels/[content_types].xml.rels"
        );
    }

    #[test]
    fn part_directory_returns_dir_or_empty() {
        assert_eq!(part_directory("word/media/image1.png"), "word/media");
        assert_eq!(part_directory("document.xml"), "");
    }

    // KNOWN GAP: percent-encoded relationship targets (e.g. spaces as "%20")
    // are not decoded, so a target like "media/my%20image.png" resolves to a
    // key that won't match the ZIP part "media/my image.png". Word does not
    // percent-encode internal media names in practice, so this is left as a
    // documented limitation rather than pulling in a percent-decoder.
}
