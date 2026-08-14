# Attribution

This crate is a vendored subset of **[dxpdf](https://github.com/nerdy-pro/dxpdf)
v0.4.0** by nerdy.pro, used under the MIT License (see `LICENSE`).

Upstream is a DOCX→PDF engine (`parse → resolve → layout → subset → paint`)
built on Skia. We vendor `parse → resolve → layout`, with layout's one
production Skia dependency (the `TextMeasurer`) rewritten over fontdb + skrifa,
so the result is pure Rust and builds on musl (rust-skia ships no musl
prebuilts, and `skia-safe` on musl falls back to a ~1hr from-source build).
`subset` and `paint` are PDF-emission concerns and stay out — layout ends at
`LayoutedPage` (draw commands), not a PDF.

## What was copied verbatim

```
src/docx/                 src/model/
src/field/                src/render/resolve/
src/error.rs
src/render/{dimension,geometry,error}.rs
src/render/layout/        (everything except measurer.rs and mod-decl edits;
                           the whole build/fragment/paragraph/section/table
                           pagination engine — vendored 2026-08-13)
src/render/emoji/cluster.rs
src/render/emoji/resolve.rs
render/mod.rs sections: estimate_cursor_y, resolve_and_layout,
                        layout_document, measure_header_footer_clearance,
                        measure_header_bottom, measure_footer_extent
```

## What was changed

1. `src/render/emf.rs` — **dropped**. Converts Windows metafiles for the
   painter, unreachable from the structure and layout paths.
2. `src/render/fonts.rs` — **rewritten over fontdb** (was a Skia
   `FontRegistry`; briefly a shim between the structure vendor and the layout
   vendor). Same consumed surface (`build`/`resolve`/`resolve_exact`/
   `resolve_system_only`/`preload`, `TypefaceEntry`, `FontStyle`), with the
   resolution rules proven in spikes 6–8: embedded-first, exact, face-alias
   index (PostScript names + weight-qualified face names, with upstream's
   ambiguity rule and `merged_alias_weight`), style-suffix strip, the
   metric/visual substitution table, generic, and an explicit `LAST_RESORT`
   chain (`fontdb::Family::SansSerif` is not a reliable floor). Every
   resolution records a `ResolveRule`; only embedded/exact/alias/suffix/
   `Substitution(Metric)` claim cross-host reproducibility. Geometry is
   deliberately host-dependent beyond that (decision 2026-08-13: no bundled
   fonts, no checksum pinning).
3. `src/render/layout/measurer.rs` — **rewritten over skrifa.** Measurement
   arithmetic copied verbatim (cmap advance sum, §17.3.2.45 text scale,
   §17.3.2.35 char spacing); unmapped codepoints take .notdef's advance
   (zero-contribution was tried and disproved, spike 6). Underline offsets are
   negative-below — skrifa's `post` value passes through untouched; upstream's
   comment at its `measurer.rs:156` states the opposite of what its code does.
   Emoji cluster advances are cmap-only (upstream's own fallback path) until a
   harfrust shaper is wired; `emoji/{shape,raster}.rs` are not vendored.
4. `src/render/emoji/resolve.rs` — one import swapped (`skia_safe::FontStyle`
   → `crate::render::fonts::FontStyle`); its test helper builds a
   `TypefaceEntry` from the fontdb registry instead of a Skia `FontMgr`.
5. `src/lib.rs`, `src/render/mod.rs`, `src/render/emoji/mod.rs` —
   **rewritten** to declare only the vendored modules;
   `src/render/layout/mod.rs` is now upstream's verbatim.
6. Edition-2024 pattern fixes (`ref mut` in an implicit borrow) in
   `render/resolve/properties.rs`, `docx/parse/rel_rewrite.rs`,
   `render/layout/fragment/collect.rs`, `render/layout/paragraph/mod.rs` —
   upstream pins an older toolchain.
7. Test-only constructor edits: `FontRegistry::new(skia_safe::FontMgr::new())`
   → `FontRegistry::new()` across layout test modules.
8. Unknown-element tolerance — see below.
9. **Block→page instrumentation (liteparse-only, no upstream equivalent).**
   `LayoutedPage.block_starts` records, per page, the flattened body-block
   indices (over the concatenation of every section's `blocks`) whose first
   content command landed on that page — this feeds liteparse's per-page
   markdown split. Supporting plumbing: `BuiltSection.source_indices`
   (`layout/build/mod.rs`), `SectionStart.block_sources` +
   `PageLayoutState.pending_block_start`/`note_block_start` with flush sites
   at the four content-append points (`layout/section/layout.rs`), and the
   `body_base` offset accumulation in `render/mod.rs::layout_document`.
   Deliberately a side-channel field rather than a `DrawCommand` variant — the
   command enum is matched exhaustively across the crate by design, and a page
   field inherits checkpoint/replay and `Continuous`-continuation semantics
   for free. Verified behaviour-preserving: `layout_probe` command censuses
   over the 48-doc corpus + long documents are byte-identical before/after.
10. **`src/render/raster.rs` (liteparse-only, no upstream equivalent;
    `raster` cargo feature).** Rasterizes `LayoutedPage` draw commands to
    RGBA over tiny-skia + skrifa outlines — the role upstream's
    `painter.rs`+Skia plays for PDF emission, rebuilt for PNG screenshots
    without Skia. Glyph pen advances re-derive the measurer's arithmetic
    against the same `FontRegistry` faces, so raster and `TextItem` geometry
    share one coordinate space by construction. Paints in four layers
    (shape < shading rect < image < ink), each in stream order:
    `DrawCommand` carries no z-order and §20.4.2.3 `behindDoc` is honored by
    emission position only within a header/footer run, so a body-anchored
    behind-doc shape lands mid-stream after the header's text — upstream's
    sequential painter has the same artifact. The command stream itself is
    untouched (censuses, image-FIFO naming and text-item order all
    unaffected). Optional deps: `tiny-skia` 0.11 (the line resvg already
    pins in the liteparse tree), `image` 0.25 (decode only). The
    `cmd_dump` example prints a page's command stream compactly — it is the
    debug tool for z-order/ordering questions.

## Layout parity (measured 2026-08-13, macOS host)

Against upstream dxpdf 0.4.0 with Skia, same 48-doc corpus + long documents:
48/48 within one page, 41/48 exact; of the exact docs, 37 have identical full
draw-command censuses and 32 identical per-page text/char counts (identical
line breaking). Every page diff is +1 and confined to documents requesting
fonts the host lacks (Calibri/Cambria/Aptos) — the two engines fall back to
different faces (Helvetica 1.00 em vs Arial-metrics 1.15 em line height), which
is a host-font-book fact, not measurer error. `civil_jury.docx`: 616 vs 615
pages (0.16% at 615 pages — drift does not compound).

## Fail-open parsing (the main deliberate divergence)

**Upstream fails closed at every level. We fail open.** A single malformed
value anywhere in a file must never cost the whole document — that is a
standing requirement of this copy, not a one-off fix. Three distinct classes,
all of which upstream treats as fatal:

| class | upstream | here |
|---|---|---|
| **unknown elements** (`commentReference`, …) | aborts document | `#[serde(other)]` catch-all, element skipped |
| **unknown attribute *values*** (`w:jc val="bogus"`) | aborts document | `lenient::` deserializers → unspecified |
| **malformed scalars** (colours, measurements, ids) | aborts document | dropped / spec default |

The mechanism lives in `docx/parse/primitives/lenient.rs`:

- `opt_attr` — `Option<T>` attribute → `None`. ECMA-376 §17.17 says an invalid
  value is treated as absent, and absent means *inherit from the style chain*,
  so this is both the spec-correct and the least-destructive degradation.
- `opt_val_attr` — same for `<w:x w:val="…"/>` elements; collapses the
  `ValAttr<T>` wrapper so the field is plain `Option<T>`.
- `or_default` — required attributes whose type has a *spec* default.
- `nonneg_or_default` — required non-negative measurements → zero, keeping the
  non-negativity guard.

Two rules that are easy to get wrong and were both hit during the port:

1. **Never invent a value to keep an infallible `From`.** Where the model has
   no "unspecified" variant (colours, adjust handles, gradient stops) the
   conversion was made fallible and callers drop the item. Substituting black
   or zero would be silent corruption.
2. **`AttrValueDeserializer` must coerce, not just visit strings.** serde's own
   string deserializer refuses to produce an integer, so routing numeric
   attributes through it would silently turn every *valid* `gridSpan`, `numId`,
   `ilvl` and `outlineLvl` into `None` — corrupting documents rather than
   merely being strict. Its unit tests guard exactly this.

Upstream tests that asserted the strict behaviour were rewritten to assert the
new contract *plus* the guarantee they originally protected (e.g. `"1.0"` is
dropped as a numbering id, but must still never be coerced to `1`).

### Verifying

```
cargo test -p liteparse-docx
cargo build -p liteparse-docx --example parse_probe
python3 bench/docx_native_spike/fuzz_attribute_values.py --mode all
```

The fuzz harness corrupts every attribute value in the `docx_files` corpus
(623k values) and requires all 48 documents to still parse.

Keeping this list current matters: it is what makes a future re-sync against a
newer dxpdf tractable.
