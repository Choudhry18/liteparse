//! Layout probe — the vendored (fontdb + skrifa) twin of spike 2's harness.
//!
//! Taps `liteparse_ooxml::render::resolve_and_layout` and emits the same JSON
//! Lines shape as `bench/docx_layout_spike` (which runs upstream dxpdf with
//! Skia), so the two joins directly: same document in, page counts and
//! command censuses out, one row per file. Divergence between the two is the
//! cost of the measurer swap plus resolution differences — spike 6/8 predict
//! ~0 where the host has the fonts.
//!
//! ```text
//! cargo run --release -p liteparse-ooxml --example layout_probe -- <file.docx>...
//! ```

use std::time::Instant;

use liteparse_ooxml::render::layout::draw_command::DrawCommand;
use liteparse_ooxml::render::resolve_and_layout;
use serde_json::json;

/// Running min/max over text positions, so we can check the coordinate
/// convention (origin corner, y direction) rather than assume it.
#[derive(Default)]
struct Extent {
    min_x: Option<f32>,
    max_x: Option<f32>,
    min_y: Option<f32>,
    max_y: Option<f32>,
}

impl Extent {
    fn add(&mut self, x: f32, y: f32) {
        self.min_x = Some(self.min_x.map_or(x, |v: f32| v.min(x)));
        self.max_x = Some(self.max_x.map_or(x, |v: f32| v.max(x)));
        self.min_y = Some(self.min_y.map_or(y, |v: f32| v.min(y)));
        self.max_y = Some(self.max_y.map_or(y, |v: f32| v.max(y)));
    }
}

/// Per-document command census. Counting every variant (not just `Text`) is
/// the point: `LinkAnnotation` and `Underline` are facts the PDF path has to
/// infer geometrically today, so their presence is part of what we are
/// grading.
#[derive(Default)]
struct Census {
    text: u64,
    chars: u64,
    underline: u64,
    line: u64,
    image: u64,
    emoji: u64,
    rect: u64,
    link: u64,
    internal_link: u64,
    named_dest: u64,
    outline: u64,
    transform: u64,
    path: u64,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: layout_probe <file.docx>...");
        std::process::exit(2);
    }

    for path in &args {
        println!("{}", serde_json::to_string(&run_one(path)).unwrap());
    }
}

fn run_one(path: &str) -> serde_json::Value {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            return json!({ "path": path, "ok": false, "stage": "read", "error": e.to_string() });
        }
    };

    let t0 = Instant::now();
    // The vendored parser is fail-open (unknown elements, attribute values
    // and namespace collisions degrade instead of aborting), so parse
    // failures here are unexpected — report them loudly.
    let doc = match liteparse_ooxml::docx::parse(&data) {
        Ok(d) => d,
        Err(e) => {
            return json!({ "path": path, "ok": false, "stage": "parse", "error": e.to_string() });
        }
    };
    let parse_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // `resolve_and_layout` panics rather than erroring on a font-less system
    // (it `expect`s the system FontMgr to expose a typeface), and layout is
    // young code we have never run at corpus scale. Catch panics so one bad
    // document yields a row instead of killing the whole run.
    let t1 = Instant::now();
    let laid = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| resolve_and_layout(doc)));
    let layout_ms = t1.elapsed().as_secs_f64() * 1000.0;

    let (_resolved, pages) = match laid {
        Ok(v) => v,
        Err(_) => {
            return json!({ "path": path, "ok": false, "stage": "layout", "error": "panic" });
        }
    };

    let mut census = Census::default();
    let mut extent = Extent::default();
    let mut page_rows = Vec::new();

    for page in &pages {
        let mut page_text = 0u64;
        let mut page_chars = 0u64;

        for cmd in &page.commands {
            match cmd {
                DrawCommand::Text { position, text, .. } => {
                    census.text += 1;
                    page_text += 1;
                    let n = text.chars().count() as u64;
                    census.chars += n;
                    page_chars += n;
                    extent.add(position.x.raw(), position.y.raw());
                }
                DrawCommand::Underline { .. } => census.underline += 1,
                DrawCommand::Line { .. } => census.line += 1,
                DrawCommand::Image { .. } => census.image += 1,
                DrawCommand::EmojiCluster { .. } => census.emoji += 1,
                DrawCommand::Rect { .. } => census.rect += 1,
                DrawCommand::LinkAnnotation { .. } => census.link += 1,
                DrawCommand::InternalLink { .. } => census.internal_link += 1,
                DrawCommand::NamedDestination { .. } => census.named_dest += 1,
                DrawCommand::Outline(_) => census.outline += 1,
                // PPTX-only placement bracket; a DOCX layout emits none, so a
                // non-zero count here means this probe was pointed at a slide.
                DrawCommand::Transform(_) => census.transform += 1,
                // No catch-all: the compiler confirming this match is
                // exhaustive is how we know the census can't silently miss a
                // command class.
                DrawCommand::Path { .. } => census.path += 1,
            }
        }

        page_rows.push(json!({
            "w": page.page_size.width.raw(),
            "h": page.page_size.height.raw(),
            "text_cmds": page_text,
            "chars": page_chars,
        }));
    }

    json!({
        "path": path,
        "ok": true,
        "pages": pages.len(),
        "parse_ms": parse_ms,
        "layout_ms": layout_ms,
        "page_rows": page_rows,
        "commands": {
            "text": census.text,
            "chars": census.chars,
            "underline": census.underline,
            "line": census.line,
            "image": census.image,
            "emoji": census.emoji,
            "rect": census.rect,
            "link": census.link,
            "internal_link": census.internal_link,
            "named_dest": census.named_dest,
            "outline": census.outline,
            "transform": census.transform,
            "path": census.path,

        },
        "text_extent": {
            "min_x": extent.min_x, "max_x": extent.max_x,
            "min_y": extent.min_y, "max_y": extent.max_y,
        },
    })
}
