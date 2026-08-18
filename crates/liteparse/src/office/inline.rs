//! Inline rendering shared by the native office readers.
//!
//! Both DOCX and PPTX arrive at the same place: a paragraph is a sequence of
//! formatting-tagged text runs, and `Block::Paragraph`/`Block::ListItem` want
//! `(text, bold, italic)`. Only the *cascade* that produces the formatting
//! differs between the two — DOCX resolves it through style chains, PPTX
//! through the placeholder text cascade — so the rendering half is shared and
//! the resolution half is not.
//!
//! Keeping this in one place is what makes the same prose look the same
//! whichever reader produced it, which is the whole point of both pipelines
//! converging on `markdown_layout::Block`.

use crate::markdown_layout::{apply_link, escape_inline};

/// Effective character formatting after the style cascade is applied.
#[derive(Default, Clone, Copy, PartialEq)]
pub(crate) struct Fmt {
    pub(crate) bold: bool,
    pub(crate) italic: bool,
    pub(crate) strike: bool,
}

impl Fmt {
    pub(crate) fn is_plain(self) -> bool {
        !self.bold && !self.italic && !self.strike
    }
}

/// A formatting-tagged run of text, plus the external URL of the hyperlink it
/// sits inside, if any.
#[derive(Clone, PartialEq)]
pub(crate) struct Chunk {
    pub(crate) fmt: Fmt,
    pub(crate) link: Option<String>,
    pub(crate) text: String,
}

/// Render formatting-tagged chunks to the `(text, bold, italic)` shape
/// `Block::Paragraph` and `Block::ListItem` expect.
///
/// When every chunk shares one plain-or-emphasised style the block-level flags
/// carry it and the text stays clean; otherwise emphasis is baked inline and
/// the flags are cleared. This mirrors `paragraph_from_accum` on the PDF path,
/// so the same prose looks the same whichever pipeline produced it.
pub(crate) fn render_chunks(chunks: &[Chunk], escape: bool) -> (String, bool, bool) {
    // Coalesce adjacent runs that share formatting and link first. Word splits
    // a run at every property change, including ones that don't survive the
    // cascade (language, spell-check state, rsid), so a single bold phrase
    // routinely arrives as several identically-formatted chunks. Emitting each
    // one separately would produce `**a****b**`, which no markdown parser
    // reads as one bold span.
    let mut merged: Vec<Chunk> = Vec::new();
    for c in chunks {
        match merged.last_mut() {
            Some(prev) if prev.fmt == c.fmt && prev.link == c.link => prev.text.push_str(&c.text),
            _ => merged.push(c.clone()),
        }
    }
    let chunks = &merged[..];

    let uniform = chunks.first().map(|c| c.fmt).filter(|f| {
        // Strike has no block-level flag, so a uniformly-struck paragraph still
        // takes the inline path. Likewise a link: it must wrap exactly its
        // anchor text, so any linked chunk forces the inline path (mirroring
        // the PDF path's rule in `render_line_inline`).
        !f.strike && chunks.iter().all(|c| c.fmt == *f) && chunks.iter().all(|c| c.link.is_none())
    });

    if let Some(fmt) = uniform {
        let raw: String = chunks.iter().map(|c| c.text.as_str()).collect();
        let raw = raw.trim();
        let text = if escape {
            escape_inline(raw)
        } else {
            raw.to_string()
        };
        return (text, fmt.bold, fmt.italic);
    }

    let mut out = String::new();
    for Chunk { fmt, link, text } in chunks {
        // Emphasis markers must hug non-space content to be valid markdown, so
        // surrounding whitespace is hoisted outside the markers.
        let lead: String = text.chars().take_while(|c| c.is_whitespace()).collect();
        let core = text.trim();
        let trail = if core.is_empty() {
            String::new()
        } else {
            text.chars()
                .rev()
                .take_while(|c| c.is_whitespace())
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect()
        };

        out.push_str(&lead);
        if core.is_empty() {
            continue;
        }
        let escaped = if escape {
            escape_inline(core)
        } else {
            core.to_string()
        };
        let mut rendered = if fmt.is_plain() {
            escaped
        } else {
            let (mut open, mut close) = (String::new(), String::new());
            if fmt.bold {
                open.push_str("**");
                close.insert_str(0, "**");
            }
            if fmt.italic {
                open.push('*');
                close.insert(0, '*');
            }
            if fmt.strike {
                open.push_str("~~");
                close.insert_str(0, "~~");
            }
            format!("{open}{escaped}{close}")
        };
        // Link wraps outside emphasis — `[*anchor*](url)` — matching
        // `render_line_inline` on the PDF path.
        if let Some(url) = link {
            rendered = apply_link(&rendered, url);
        }
        out.push_str(&rendered);
        out.push_str(&trail);
    }
    (out.trim().to_string(), false, false)
}
