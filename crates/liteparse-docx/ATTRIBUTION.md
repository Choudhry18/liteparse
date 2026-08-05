# Attribution

This crate is a vendored subset of **[dxpdf](https://github.com/nerdy-pro/dxpdf)
v0.4.0** by nerdy.pro, used under the MIT License (see `LICENSE`).

Upstream is a DOCX→PDF engine (`parse → resolve → layout → subset → paint`)
built on Skia. We vendor only the first two stages, which contain no Skia
references, so the result is pure Rust and builds on musl (rust-skia ships no
musl prebuilts, and `skia-safe` on musl falls back to a ~1hr from-source build).

## What was copied verbatim

```
src/docx/                 src/model/
src/field/                src/render/resolve/
src/error.rs
src/render/{dimension,geometry,error}.rs
src/render/layout/draw_command.rs   (referenced by resolve::shape_visuals)
src/render/emoji/cluster.rs         (referenced by draw_command)
```

## What was changed

1. `src/render/emf.rs` — **dropped**. The subset's only genuine `skia_safe`
   import; converts Windows metafiles for the painter, unreachable from the
   structure path.
2. `src/render/fonts.rs` — **replaced with a shim.** Upstream is a Skia
   `FontRegistry`; only `TypefaceEntry` is named here (one field of
   `DrawCommand::Text`). This is the seam for a future harfrust+skrifa port.
3. `src/lib.rs`, `src/render/mod.rs`, `src/render/layout/mod.rs`,
   `src/render/emoji/mod.rs` — **rewritten** to declare only the copied modules.
4. Edition-2024 pattern fixes (`ref mut` in an implicit borrow) in
   `render/resolve/properties.rs` and `docx/parse/rel_rewrite.rs` — upstream
   pins an older toolchain.
5. Unknown-element tolerance — see below.

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
