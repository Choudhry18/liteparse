//! Line placement, command emission, and related helpers.

use std::rc::Rc;

use super::super::draw_command::{DrawCommand, OutlineMark};
use super::super::fragment::{Fragment, LinkTarget};
use super::types::{LineSpacingRule, MeasureTextFn, ParagraphStyle, TabStopDef};
use super::{LEADER_CHAR_WIDTH_FALLBACK, LineLayoutParams, LinePlacement};
use crate::render::dimension::Pt;
use crate::render::geometry::PtOffset;

/// Fit all fragments into lines, computing per-line float adjustments when
/// active floats are present.
///
/// When `style.page_floats` is non-empty each line is fitted individually so
/// the available width can vary depending on the line's absolute y position.
/// When there are no floats a single call to `fit_lines_with_first` is used.
pub(super) fn compute_line_placements(
    fragments: &[Fragment],
    style: &ParagraphStyle,
    params: &LineLayoutParams,
) -> Vec<LinePlacement> {
    let content_width = params.content_width;
    let first_line_adjustment = params.first_line_adjustment;
    let drop_cap_indent = params.drop_cap_indent;
    let drop_cap_lines = params.drop_cap_lines;
    let default_line_height = params.default_line_height;
    let ptab_geometry = PTabGeometry {
        max_width: params.max_width,
        indent_left: style.indent_left,
        indent_first_line: style.indent_first_line,
        content_width,
        float_left: Pt::ZERO,
        float_right: Pt::ZERO,
    };
    if style.page_floats.is_empty() {
        // No floats — use standard line fitting.
        let first_line_width = (content_width - first_line_adjustment).max(Pt::ZERO);
        let remaining_width = if drop_cap_indent > Pt::ZERO {
            content_width - drop_cap_indent
        } else {
            content_width
        };
        return super::super::line::fit_lines_with_first(
            fragments,
            first_line_width,
            remaining_width,
            ptab_geometry,
        )
        .into_iter()
        .map(|line| LinePlacement {
            line,
            float_left: Pt::ZERO,
            float_right: Pt::ZERO,
        })
        .collect();
    }

    // Active floats — fit one line at a time so available width can change.
    let mut placements = Vec::new();
    let mut frag_idx = 0;
    let mut line_y = style.space_before;

    while frag_idx < fragments.len() {
        let abs_y = style.page_y + line_y;
        let (fl, fr) = super::super::float::float_adjustments_with_height(
            &style.page_floats,
            abs_y,
            default_line_height,
            style.page_x,
            style.page_content_width,
        );
        let float_reduction = fl + fr;
        let available = (content_width - float_reduction).max(Pt::ZERO);

        let is_first = placements.is_empty();
        let dc_adj = if placements.len() < drop_cap_lines {
            drop_cap_indent
        } else {
            Pt::ZERO
        };
        let line_width = if is_first {
            (available - first_line_adjustment).max(Pt::ZERO)
        } else {
            (available - dc_adj).max(Pt::ZERO)
        };

        // Fit one line at this width. Each call fits exactly one line, so the
        // geometry is *this* line's: its own float narrowing, and the
        // first-line indent only when it really is the first.
        let remaining = &fragments[frag_idx..];
        let line_geometry = PTabGeometry {
            indent_first_line: if is_first {
                ptab_geometry.indent_first_line
            } else {
                Pt::ZERO
            },
            float_left: fl,
            float_right: fr,
            ..ptab_geometry
        };
        let fitted = super::super::line::fit_lines_with_first(
            remaining,
            line_width,
            line_width,
            line_geometry,
        );
        let fitted_line = if let Some(first) = fitted.into_iter().next() {
            super::super::line::FittedLine {
                start: first.start + frag_idx,
                end: first.end + frag_idx,
                width: first.width,
                height: first.height,
                text_height: first.text_height,
                ascent: first.ascent,
                has_break: first.has_break,
            }
        } else {
            break;
        };

        let natural = if fitted_line.height > Pt::ZERO {
            fitted_line.height
        } else {
            default_line_height
        };
        let text_h = if fitted_line.text_height > Pt::ZERO {
            fitted_line.text_height
        } else {
            default_line_height
        };
        let lh = resolve_line_height(natural, text_h, &style.line_spacing, style.auto_fit);

        frag_idx = fitted_line.end;
        placements.push(LinePlacement {
            line: fitted_line,
            float_left: fl,
            float_right: fr,
        });
        line_y += lh;
    }
    placements
}

fn stretchable_gap_after(fragments: &[Fragment], frag_idx: usize, line_end: usize) -> bool {
    let Fragment::Text { text, .. } = &fragments[frag_idx] else {
        return false;
    };
    text.ends_with(' ')
        && fragments[frag_idx + 1..line_end]
            .iter()
            .any(|fragment| !matches!(fragment, Fragment::Bookmark { .. }))
}

fn line_has_justification_tab(fragments: &[Fragment], line_start: usize, line_end: usize) -> bool {
    fragments[line_start..line_end].iter().any(|fragment| {
        matches!(
            fragment,
            Fragment::Tab {
                fitting_width: None,
                ..
            } | Fragment::PTab { .. }
        ) || matches!(fragment, Fragment::Text { text, .. } if text.contains('\t'))
    })
}

fn visible_line_width(fragments: &[Fragment], line: &super::super::line::FittedLine) -> Pt {
    let last_visible = (line.start..line.end)
        .rev()
        .find(|&idx| !matches!(fragments[idx], Fragment::Bookmark { .. }));
    (line.start..line.end)
        .map(|idx| {
            if Some(idx) == last_visible {
                fragments[idx].trimmed_width()
            } else {
                fragments[idx].width()
            }
        })
        .sum()
}

fn justification_extra_after(
    fragments: &[Fragment],
    line: &super::super::line::FittedLine,
    line_idx: usize,
    line_count: usize,
    alignment: crate::model::Alignment,
    line_has_tabs: bool,
    remaining: Pt,
) -> Pt {
    if alignment != crate::model::Alignment::Both
        || line.has_break
        || line_idx + 1 == line_count
        || line_has_tabs
        || remaining <= Pt::ZERO
    {
        return Pt::ZERO;
    }
    let gaps = (line.start..line.end)
        .filter(|&idx| stretchable_gap_after(fragments, idx, line.end))
        .count();
    if gaps == 0 {
        Pt::ZERO
    } else {
        remaining / gaps as f32
    }
}

fn distribution_unit_count(fragment: &Fragment, terminal: bool) -> usize {
    match fragment {
        Fragment::Text { text, .. } => {
            if terminal {
                text.trim_end().chars().count()
            } else {
                text.chars().count()
            }
        }
        Fragment::Image { .. } | Fragment::Emoji { .. } => 1,
        _ => 0,
    }
}

fn distribution_gap_count_after(fragments: &[Fragment], frag_idx: usize, line_end: usize) -> usize {
    let has_following_unit = fragments[frag_idx + 1..line_end]
        .iter()
        .any(|fragment| distribution_unit_count(fragment, false) > 0);
    let units = distribution_unit_count(&fragments[frag_idx], !has_following_unit);
    if has_following_unit {
        units
    } else {
        units.saturating_sub(1)
    }
}

fn distribution_extra_per_gap(
    fragments: &[Fragment],
    line: &super::super::line::FittedLine,
    alignment: crate::model::Alignment,
    line_has_tabs: bool,
    remaining: Pt,
) -> Pt {
    if alignment != crate::model::Alignment::Distribute || line_has_tabs || remaining <= Pt::ZERO {
        return Pt::ZERO;
    }
    let gaps = (line.start..line.end)
        .map(|idx| distribution_gap_count_after(fragments, idx, line.end))
        .sum::<usize>();
    if gaps == 0 {
        Pt::ZERO
    } else {
        remaining / gaps as f32
    }
}

/// Emit `DrawCommand`s for all lines in a paragraph, advancing `cursor_y`
/// by the total line height consumed.
///
/// Handles per-fragment shading, run borders, text, hyperlinks, underlines,
/// images, tab stops (with leaders), bookmarks, and drop-cap indent offsets.
#[allow(clippy::too_many_arguments)] // low-level emit primitive; inputs are cohesive
pub(super) fn emit_line_commands(
    commands: &mut Vec<DrawCommand>,
    cursor_y: &mut Pt,
    line_placements: &[LinePlacement],
    line_range: std::ops::Range<usize>,
    fragments: &[Fragment],
    style: &ParagraphStyle,
    params: &LineLayoutParams,
    measure_text: MeasureTextFn<'_>,
    // §17.3.1.14: false for the continuation of a paragraph split across pages.
    // The caller knows; it cannot be inferred from `line_range`, because a
    // continuation is re-fitted against its own page and starts again at 0.
    is_first_segment: bool,
) {
    let content_width = params.content_width;
    let first_line_adjustment = params.first_line_adjustment;
    let drop_cap_indent = params.drop_cap_indent;
    let drop_cap_lines = params.drop_cap_lines;
    let default_line_height = params.default_line_height;
    // §17.18.85: `bar` entries are paragraph decorations, not stops, so they
    // are gathered here rather than reached from the `Fragment::Tab` arm — a
    // bar draws on every line of the paragraph, including lines (and whole
    // paragraphs) holding no tab character at all. Resolved once, outside the
    // loop: the colour is a property of the paragraph and must not vary from
    // line to line.
    let has_bar_stop = style
        .tabs
        .iter()
        .any(|ts| TabStopRole::of(ts.alignment) == TabStopRole::DrawsRule);
    let bar_color = paragraph_bar_color(fragments);
    // `line_idx` stays absolute within the paragraph (not the emitted range) so
    // first-line indent (§17.3.1.12), drop caps (§17.3.1.11), and last-line
    // justification (§17.3.1.13) resolve correctly when a paragraph is split
    // across pages and only a sub-range of its lines is emitted here.
    // §17.3.1.19: bracket this paragraph's commands so the PDF outline can point
    // at it. Emitted here, around the whole emitted run, rather than per line:
    // Skia derives an entry's destination from the union of the marks under its
    // node, so one pair spanning the heading gives the top-left of its first
    // line — exactly the point a reader expects to land on.
    //
    // Only the first segment emits it. `emit_line_commands` runs once per
    // page-slice of a paragraph (§17.3.1.14), so a heading that splits across
    // pages would otherwise open a second entry for its continuation — the
    // entry belongs to the page the heading starts on. This rides the same
    // `is_first_segment` flag that already decides `space_before` and the drop
    // cap, for the same reason.
    let outline = style.outline.as_ref().filter(|_| is_first_segment);
    if let Some(heading) = outline {
        commands.push(DrawCommand::Outline(OutlineMark::Begin(heading.clone())));
    }

    for line_idx in line_range {
        let lp = &line_placements[line_idx];
        let line = &lp.line;

        // Drop cap lines get extra indent; after that, refit remaining lines at full width.
        let dc_offset = if line_idx < drop_cap_lines {
            drop_cap_indent
        } else {
            Pt::ZERO
        };

        // §17.4.58: per-line float left indent.
        let float_offset = lp.float_left;

        let indent = if line_idx == 0 {
            style.indent_left + style.indent_first_line + dc_offset + float_offset
        } else {
            style.indent_left + dc_offset + float_offset
        };

        let natural_height = if line.height > Pt::ZERO {
            line.height
        } else {
            default_line_height
        };
        let text_height = if line.text_height > Pt::ZERO {
            line.text_height
        } else {
            default_line_height
        };
        let line_height = resolve_line_height(
            natural_height,
            text_height,
            &style.line_spacing,
            style.auto_fit,
        );

        // Alignment offset — computed relative to the line's available width.
        let float_reduction = lp.float_left + lp.float_right;
        let line_available = if line_idx == 0 {
            (content_width - float_reduction - first_line_adjustment).max(Pt::ZERO)
        } else {
            (content_width - float_reduction - dc_offset).max(Pt::ZERO)
        };
        let remaining = (line_available - visible_line_width(fragments, line)).max(Pt::ZERO);
        // §17.3.1.37: when a line contains tab characters, tab stops control
        // horizontal positioning — paragraph alignment does not apply. Absolute
        // position tabs (§17.3.1.30) place content explicitly for the same
        // reason, so they suppress paragraph alignment too.
        let line_has_tab_placement = fragments[line.start..line.end].iter().any(is_tab_like);
        let align_offset = if line_has_tab_placement {
            Pt::ZERO
        } else {
            match style.alignment {
                crate::model::Alignment::Center => remaining * 0.5,
                crate::model::Alignment::End => remaining,
                crate::model::Alignment::Both
                    if !line.has_break && line_idx < line_placements.len() - 1 =>
                {
                    Pt::ZERO
                }
                _ => Pt::ZERO,
            }
        };
        let extra_per_gap = justification_extra_after(
            fragments,
            line,
            line_idx,
            line_placements.len(),
            style.alignment,
            line_has_justification_tab(fragments, line.start, line.end),
            remaining,
        );
        let distribution_extra = distribution_extra_per_gap(
            fragments,
            line,
            style.alignment,
            line_has_justification_tab(fragments, line.start, line.end),
            remaining,
        );

        let x_start = indent + align_offset;

        // Before the content, so text wins wherever a rule and a glyph overlap.
        if has_bar_stop {
            emit_bar_rules(commands, &style.tabs, bar_color, *cursor_y, line_height);
        }

        // Emit text commands for this line
        let mut x = x_start;
        for (frag_idx, frag) in (line.start..line.end).zip(&fragments[line.start..line.end]) {
            match frag {
                Fragment::Text {
                    text,
                    font,
                    color,
                    shading,
                    border,
                    width,
                    metrics,
                    hyperlink_url,
                    baseline_offset,
                    text_offset,
                    ..
                } => {
                    let extra_after = if stretchable_gap_after(fragments, frag_idx, line.end) {
                        extra_per_gap
                    } else {
                        Pt::ZERO
                    };
                    let distributed_width = distribution_extra
                        * distribution_gap_count_after(fragments, frag_idx, line.end) as f32;
                    let rendered_width = *width + extra_after + distributed_width;

                    // §17.3.2.32: render run-level shading behind text.
                    // Uses text bounds (ascent+descent), not full line height.
                    if let Some(bg_color) = shading {
                        let text_top = *cursor_y + line.ascent - metrics.ascent;
                        commands.push(DrawCommand::Rect {
                            rect: crate::render::geometry::PtRect::from_xywh(
                                x,
                                text_top,
                                rendered_width,
                                metrics.height(),
                            ),
                            color: *bg_color,
                        });
                    }

                    // §17.3.2.4: render run-level border (box around text).
                    // Uses text bounds, not full line height.
                    if let Some(bdr) = border {
                        let text_top = *cursor_y + line.ascent - metrics.ascent;
                        let bx = x - bdr.space;
                        let by = text_top;
                        let bw = rendered_width + bdr.space * 2.0;
                        let bh = metrics.height();
                        let half = bdr.width * 0.5;
                        // Top
                        commands.push(DrawCommand::Line {
                            line: crate::render::geometry::PtLineSegment::new(
                                PtOffset::new(bx, by + half),
                                PtOffset::new(bx + bw, by + half),
                            ),
                            color: bdr.color,
                            width: bdr.width,
                        });
                        // Bottom
                        commands.push(DrawCommand::Line {
                            line: crate::render::geometry::PtLineSegment::new(
                                PtOffset::new(bx, by + bh - half),
                                PtOffset::new(bx + bw, by + bh - half),
                            ),
                            color: bdr.color,
                            width: bdr.width,
                        });
                        // Left
                        commands.push(DrawCommand::Line {
                            line: crate::render::geometry::PtLineSegment::new(
                                PtOffset::new(bx + half, by),
                                PtOffset::new(bx + half, by + bh),
                            ),
                            color: bdr.color,
                            width: bdr.width,
                        });
                        // Right
                        commands.push(DrawCommand::Line {
                            line: crate::render::geometry::PtLineSegment::new(
                                PtOffset::new(bx + bw - half, by),
                                PtOffset::new(bx + bw - half, by + bh),
                            ),
                            color: bdr.color,
                            width: bdr.width,
                        });
                    }

                    let y = *cursor_y + line.ascent + *baseline_offset;
                    commands.push(DrawCommand::Text {
                        position: PtOffset::new(x + *text_offset, y),
                        text: text.clone(),
                        font_family: font.family.clone(),
                        char_spacing: font.char_spacing + distribution_extra,
                        font_size: font.size,
                        bold: font.bold,
                        italic: font.italic,
                        color: *color,
                        text_scale: font.text_scale,
                    });

                    if let Some(link) = hyperlink_url {
                        let rect = crate::render::geometry::PtRect::from_xywh(
                            x,
                            *cursor_y,
                            rendered_width,
                            line_height,
                        );
                        // The external/internal kind is carried on the fragment
                        // (§17.16.22), so route by the ADT — no URL-scheme guess.
                        match link {
                            LinkTarget::External(url) => {
                                commands.push(DrawCommand::LinkAnnotation {
                                    rect,
                                    url: url.clone(),
                                })
                            }
                            LinkTarget::Internal(dest) => {
                                commands.push(DrawCommand::InternalLink {
                                    rect,
                                    destination: dest.clone(),
                                })
                            }
                        }
                    }

                    if font.underline {
                        // §17.3.2.40: underline position and thickness from font metrics.
                        // Skia provides underlinePosition (negative = below baseline)
                        // and underlineThickness.
                        let underline_y = y - font.underline_position;
                        let stroke_width = font.underline_thickness;
                        commands.push(DrawCommand::Underline {
                            line: crate::render::geometry::PtLineSegment::new(
                                PtOffset::new(x, underline_y),
                                PtOffset::new(x + rendered_width, underline_y),
                            ),
                            color: *color,
                            width: stroke_width,
                        });
                    }

                    x += rendered_width;
                }
                Fragment::Image {
                    size,
                    image_data,
                    src_rect,
                    ..
                } => {
                    if let Some(data) = image_data {
                        commands.push(DrawCommand::Image {
                            rect: crate::render::geometry::PtRect::from_xywh(
                                x,
                                *cursor_y,
                                size.width,
                                size.height,
                            ),
                            image_data: data.clone(),
                            src_rect: *src_rect,
                            float: false,
                        });
                    }
                    x += size.width
                        + distribution_extra
                            * distribution_gap_count_after(fragments, frag_idx, line.end) as f32;
                }
                Fragment::Emoji {
                    text,
                    typeface,
                    size,
                    presentation,
                    structure,
                    advance,
                    metrics,
                    line_metrics: _,
                    baseline_offset,
                } => {
                    // Place the cluster's box so its top sits at the line
                    // baseline minus the typeface ascent, matching where text
                    // glyphs at the same baseline would sit.
                    let baseline_y = *cursor_y + line.ascent + *baseline_offset;
                    let top_y = baseline_y - metrics.ascent;
                    commands.push(DrawCommand::EmojiCluster {
                        rect: crate::render::geometry::PtRect::from_xywh(
                            x,
                            top_y,
                            *advance,
                            metrics.height(),
                        ),
                        text: text.clone(),
                        typeface: typeface.clone(),
                        size: *size,
                        presentation: *presentation,
                        structure: *structure,
                    });
                    x += *advance
                        + distribution_extra
                            * distribution_gap_count_after(fragments, frag_idx, line.end) as f32;
                }
                Fragment::Tab {
                    font: tab_font,
                    color: tab_color,
                    ..
                } => {
                    // §17.3.1.37: resolve to the next tab stop.
                    // Tab stop positions are absolute from the paragraph's
                    // left edge, not relative to the text indent.
                    let (tab_pos, tab_stop) =
                        find_next_tab_stop(x, &style.tabs, line_available, style.default_tab_stop);

                    let new_x = if let Some(ts) = tab_stop {
                        // §17.3.1.37: this tab's *zone* is the content from
                        // here to the next tab (regular or position) or the
                        // line end, whichever comes first. §17.18.85 decides
                        // which point of that zone lands on the stop.
                        let end = zone_end(fragments, frag_idx, line.end);
                        let zone = &fragments[frag_idx + 1..end];
                        let offset = resolve_zone_anchor(
                            ts.alignment,
                            zone,
                            measure_text,
                            style.locale.decimal_separator(),
                        )
                        .offset(|| zone_width(fragments, frag_idx + 1, end));
                        // Never move the pen backwards: a zone wider than the
                        // space before its stop overflows to the right rather
                        // than overprinting what precedes it.
                        (tab_pos - offset).max(x)
                    } else {
                        tab_pos
                    };

                    // Emit leader characters between tab start and tab position.
                    if let Some(ts) = tab_stop {
                        emit_tab_leader(
                            commands,
                            LeaderStyle {
                                leader: ts.leader,
                                font: tab_font,
                                color: *tab_color,
                            },
                            x,
                            new_x,
                            *cursor_y + line.ascent,
                            measure_text,
                        );
                    }

                    x = new_x;
                }
                Fragment::PTab {
                    align,
                    relative_to,
                    leader,
                    font: tab_font,
                    color: tab_color,
                    ..
                } => {
                    // §17.3.1.30: an absolute-position tab has no explicit
                    // stop; its anchor is derived from `relative_to` and the
                    // width of the zone it positions (this tab to the next tab
                    // or the line end).
                    let end = zone_end(fragments, frag_idx, line.end);
                    let geometry = PTabGeometry {
                        max_width: params.max_width,
                        indent_left: style.indent_left,
                        indent_first_line: style.indent_first_line,
                        content_width,
                        float_left: lp.float_left,
                        float_right: lp.float_right,
                    };
                    let new_x = match resolve_ptab(*align, *relative_to, geometry, x, || {
                        zone_width(fragments, frag_idx + 1, end)
                    }) {
                        PTabPlacement::Placed(at) => at,
                        PTabPlacement::AdvancesToNextLine { anchor_x } => {
                            if frag_idx == line.start {
                                // Fitting breaks the line before any tab whose
                                // anchor is behind the pen, so reaching here
                                // means the tab is already first on its line.
                                // Nothing has been drawn yet, so honouring the
                                // anchor cannot overprint — including when it
                                // sits left of `indent_left`, which is exactly
                                // what `relativeTo="margin"` asks for. The
                                // floor is the line's own left edge: a zone
                                // wider than its anchor has nowhere better to
                                // go, and advancing again would not move it.
                                anchor_x.max(Pt::ZERO)
                            } else {
                                // Defensive: fitting should have broken before
                                // this tab. If it did not, refusing to move
                                // backwards is the only safe answer.
                                anchor_x.max(x)
                            }
                        }
                    };

                    emit_tab_leader(
                        commands,
                        LeaderStyle {
                            leader: *leader,
                            font: tab_font,
                            color: *tab_color,
                        },
                        x,
                        new_x,
                        *cursor_y + line.ascent,
                        measure_text,
                    );

                    x = new_x;
                }
                Fragment::LineBreak { .. } | Fragment::ColumnBreak | Fragment::PageBreak { .. } => {
                }
                Fragment::Bookmark { name } => {
                    commands.push(DrawCommand::NamedDestination {
                        position: PtOffset::new(x, *cursor_y),
                        name: name.clone(),
                    });
                }
            }
        }

        *cursor_y += line_height;
    }

    if outline.is_some() {
        commands.push(DrawCommand::Outline(OutlineMark::End));
    }
}

/// True for fragments that place content by tab semantics — a regular tab
/// stop (§17.3.1.37) or an absolute-position tab (§17.3.1.30). Both terminate
/// a tab zone and suppress paragraph alignment for the line.
fn is_tab_like(f: &Fragment) -> bool {
    matches!(f, Fragment::Tab { .. } | Fragment::PTab { .. })
}

/// Index at which the zone opened by a tab at `tab_idx` ends: the next
/// tab-like fragment or line break, or `limit`.
///
/// §17.3.1.37/§17.3.1.30: a tab positions the content up to the next tab, so
/// every tab arm — regular, position, and the fitter's look-ahead — needs this
/// same boundary. Emission passes the line end as `limit`; fitting, which has
/// no lines yet, passes the fragment count.
pub(crate) fn zone_end(fragments: &[Fragment], tab_idx: usize, limit: usize) -> usize {
    fragments[tab_idx + 1..limit]
        .iter()
        .position(|f| is_tab_like(f) || f.is_line_break())
        .map_or(limit, |offset| tab_idx + 1 + offset)
}

/// Total width of `fragments[start..end]`.
///
/// Always call this behind a thunk: `left`-aligned tabs anchor at the span
/// start and never need it, and this walk is on the per-line hot path.
pub(crate) fn zone_width(fragments: &[Fragment], start: usize, end: usize) -> Pt {
    fragments[start..end].iter().map(|f| f.width()).sum()
}

/// §17.3.1.37: find the next tab stop position greater than `current_x`.
/// Returns (position, optional tab stop definition).
/// If no custom tab stop matches, falls back to the document's default tab-stop
/// interval (§17.15.1.25 `w:defaultTabStop`, spec default 36pt / 0.5 inch).
pub(super) fn find_next_tab_stop(
    current_x: Pt,
    tabs: &[TabStopDef],
    line_width: Pt,
    default_interval: Pt,
) -> (Pt, Option<&TabStopDef>) {
    // Find the first custom tab stop past current position. §17.18.85: a `bar`
    // entry is a rule, not a stop, so a tab passes over it — skipping it here
    // is what lets the pen reach the next real stop, or fall through to the
    // default interval below when a bar is the only entry.
    for ts in tabs {
        if ts.position > current_x && TabStopRole::of(ts.alignment) == TabStopRole::PositionsContent
        {
            return (ts.position, Some(ts));
        }
    }

    // No custom tab stop — use the document default interval. Guard against a
    // zero/negative interval (a degenerate w:defaultTabStop) to avoid a stall.
    let interval = if default_interval.raw() > 0.0 {
        default_interval.raw()
    } else {
        36.0
    };
    let next = ((current_x.raw() / interval).floor() + 1.0) * interval;
    (Pt::new(next.min(line_width.raw())), None)
}

/// §17.3.1.30: the paragraph geometry a position tab resolves its anchor
/// against.
///
/// Line *fitting* and line *emission* must agree about where a ptab puts
/// content — fitting decides whether the tab can be honoured on this line,
/// emission decides the exact x. Both derive it from this one struct through
/// [`resolve_ptab`], so the two cannot drift apart.
#[derive(Clone, Copy, Debug)]
pub struct PTabGeometry {
    /// Full constraint width, before paragraph indents. `relativeTo="margin"`
    /// measures against this.
    pub max_width: Pt,
    /// Paragraph left indent. Line fitting seeds its pen here, matching where
    /// emission starts a line — otherwise the two classify the same tab
    /// differently and emission, unable to break, falls back to clamping.
    pub indent_left: Pt,
    /// §17.3.1.12 first-line indent, added to the seed on the first line only.
    /// Callers that fit a single continuation line pass `Pt::ZERO`.
    pub indent_first_line: Pt,
    /// Constraint width net of indents. `relativeTo="indent"` measures against
    /// `indent_left + content_width`, less the float narrowing below.
    pub content_width: Pt,
    /// §20.4.2: width stolen from this line's left by an overlapping float.
    /// The indented region is narrower on such a line, so the tab's span must
    /// follow it or the zone is positioned on top of the float.
    pub float_left: Pt,
    /// §20.4.2: width stolen from this line's right by an overlapping float.
    pub float_right: Pt,
}

/// §17.3.1.30: outcome of resolving a position tab against the current pen.
///
/// The `AdvancesToNextLine` case is why this is an ADT rather than a `Pt`: it
/// is a *line-breaking* outcome, not a coordinate, and collapsing it to
/// `max(desired, pen)` is exactly the clamp that drew content off the page.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PTabPlacement {
    /// The alignment point is at or ahead of the pen; content starts here.
    Placed(Pt),
    /// The alignment point lies behind the pen. §17.3.1.30 advances the tab to
    /// that location on the *next* line; `anchor_x` is where content will land
    /// once it gets there.
    AdvancesToNextLine { anchor_x: Pt },
}

/// §17.3.1.30: resolve a position tab against `pen_x` and its zone width.
///
/// `zone_width` yields the total width of the content this tab positions —
/// from the tab to the next tab or the line end. It is a thunk because a
/// `left` tab anchors at the span start and never needs it.
pub fn resolve_ptab(
    align: crate::model::PTabAlignment,
    relative_to: crate::model::PTabRelativeTo,
    geometry: PTabGeometry,
    pen_x: Pt,
    zone_width: impl FnOnce() -> Pt,
) -> PTabPlacement {
    use crate::model::{PTabAlignment, PTabRelativeTo};

    // The reference span. x=0 is the constraint's left edge (the page/cell
    // text margin); `content_width` is already net of paragraph indents, so
    // the right indent edge sits at `indent_left + content_width`.
    let (span_start, span_end) = match relative_to {
        // `margin` names the page text margins, which a float does not move —
        // the span is the same on every line of the paragraph.
        PTabRelativeTo::Margin => (Pt::ZERO, geometry.max_width),
        // `indent` names the indented region, which *is* narrower on a line an
        // overlapping float steals width from. Exact Word behaviour here is
        // unconfirmed; the line-local reading is chosen because it is the only
        // one that cannot position a zone on top of the float.
        PTabRelativeTo::Indent => (
            geometry.indent_left + geometry.float_left,
            geometry.indent_left + geometry.content_width - geometry.float_right,
        ),
    };
    // The alignment picks the anchor within that span, and then which point of
    // the zone lands on it — mirroring §17.18.85's tab-stop zone anchoring.
    let anchor = match align {
        PTabAlignment::Left => span_start,
        PTabAlignment::Center => (span_start + span_end) * 0.5,
        PTabAlignment::Right => span_end,
    };
    let anchor_x = match align {
        PTabAlignment::Left => anchor,
        PTabAlignment::Center => anchor - zone_width() * 0.5,
        PTabAlignment::Right => anchor - zone_width(),
    };

    if anchor_x >= pen_x {
        PTabPlacement::Placed(anchor_x)
    } else {
        PTabPlacement::AdvancesToNextLine { anchor_x }
    }
}

/// §17.18.85: which point of a tab's *zone* — the content from the tab to the
/// next tab or the line end — lands on the stop.
///
/// `Decimal` is why this is an enum rather than a fraction of the zone width:
/// its anchor is a property of the zone's **text**, not of its geometry.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ZoneAnchor {
    /// The zone starts at the stop (`left`; also `bar` and `clear`, neither of
    /// which repositions the content that follows).
    Start,
    /// The zone ends at the stop (`right`).
    End,
    /// The zone is centred on the stop (`center`).
    Middle,
    /// This offset into the zone lands on the stop (`decimal`).
    At(Pt),
}

impl ZoneAnchor {
    /// Distance from the zone's start to its anchor point. `zone_width` is a
    /// thunk so a `Start` anchor — the overwhelmingly common case — never pays
    /// for the O(zone) width sum.
    fn offset(self, zone_width: impl FnOnce() -> Pt) -> Pt {
        match self {
            ZoneAnchor::Start => Pt::ZERO,
            ZoneAnchor::End => zone_width(),
            ZoneAnchor::Middle => zone_width() * 0.5,
            ZoneAnchor::At(offset) => offset,
        }
    }
}

/// §17.18.85: the two exclusive roles an entry in `w:tabs` can have.
///
/// A `bar` entry is **not** a stop a tab character can land on. It names a
/// column of the paragraph where a vertical rule is drawn — on every line,
/// whether or not the paragraph contains a tab at all — and tabbing passes
/// straight over it to the next real stop. Word's own tab dialog says as much:
/// a bar tab does not position text.
///
/// The two roles want two different things from the same slice, and answering
/// both from one `TabAlignment` match is what let a `bar` swallow a tab
/// character *and* draw nothing. [`ZoneAnchor`] answers a question only a
/// positioning stop has: where within its zone the content anchors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabStopRole {
    /// Positions the content that follows a tab character.
    PositionsContent,
    /// §17.18.85 `bar`: draws a vertical rule; invisible to a tab character.
    DrawsRule,
}

impl TabStopRole {
    /// Total over `TabAlignment` by construction — a new variant must state
    /// its role here rather than inheriting a catch-all's.
    fn of(alignment: crate::model::TabAlignment) -> Self {
        use crate::model::TabAlignment;
        match alignment {
            TabAlignment::Left
            | TabAlignment::Center
            | TabAlignment::Right
            | TabAlignment::Decimal
            // §17.3.1.38 `clear` deletes a stop during style merge, so one
            // reaching layout at all is inert. Leaving it a positioning stop
            // keeps the `left` behaviour it has always had; giving it a rule
            // would invent a mark the document never asked for.
            | TabAlignment::Clear => Self::PositionsContent,
            TabAlignment::Bar => Self::DrawsRule,
        }
    }
}

/// §17.18.85 gives the rule no weight and `w:tab` has no attribute for one.
/// Word draws a hairline; 0.75pt is the same default this renderer already
/// applies to an unspecified DrawingML outline, so the two agree instead of
/// each inventing a number.
const BAR_RULE_WIDTH: Pt = Pt::new(0.75);

/// The colour a paragraph's bar rules take.
///
/// §17.3.1.38's rule for tab leaders — a decoration has no formatting of its
/// own, it takes the formatting in effect — is the nearest precedent, but a bar
/// rule has no owning run to read it from: it is drawn on lines holding no tab.
/// Word keys it off the paragraph mark's run properties, which do not reach
/// layout. The paragraph's first run is the closest thing that does, and taking
/// it from the *paragraph* rather than from each line is what keeps one rule
/// one colour when the paragraph wraps or splits across pages.
fn paragraph_bar_color(fragments: &[Fragment]) -> crate::render::resolve::color::RgbColor {
    fragments
        .iter()
        .find_map(|f| match f {
            Fragment::Text { color, .. } => Some(*color),
            _ => None,
        })
        .unwrap_or(crate::render::resolve::color::RgbColor::BLACK)
}

/// §17.18.85: draw every `bar` stop's rule across one line's vertical band.
///
/// The x is the stop's own position and owes nothing to the line — not to its
/// content, its alignment offset, or its float indent — because a bar names a
/// fixed column of the paragraph. Successive lines' bands abut, so the
/// per-line segments read as one continuous rule.
fn emit_bar_rules(
    commands: &mut Vec<DrawCommand>,
    tabs: &[TabStopDef],
    color: crate::render::resolve::color::RgbColor,
    line_top: Pt,
    line_height: Pt,
) {
    for ts in tabs
        .iter()
        .filter(|ts| TabStopRole::of(ts.alignment) == TabStopRole::DrawsRule)
    {
        commands.push(DrawCommand::Line {
            line: crate::render::geometry::PtLineSegment::new(
                PtOffset::new(ts.position, line_top),
                PtOffset::new(ts.position, line_top + line_height),
            ),
            color,
            width: BAR_RULE_WIDTH,
        });
    }
}

/// §17.18.85: resolve a tab stop's alignment into the zone anchor it implies.
///
/// Total over `TabAlignment` by construction — a new variant must state its
/// anchor here rather than inheriting a catch-all's `left`.
fn resolve_zone_anchor(
    alignment: crate::model::TabAlignment,
    zone: &[Fragment],
    measure_text: MeasureTextFn<'_>,
    separator: char,
) -> ZoneAnchor {
    use crate::model::TabAlignment;
    match alignment {
        // `clear` removes the stop (§17.3.1.38) and so never shifts what
        // follows — exactly `left`'s placement.
        TabAlignment::Left | TabAlignment::Clear => ZoneAnchor::Start,
        TabAlignment::Right => ZoneAnchor::End,
        TabAlignment::Center => ZoneAnchor::Middle,
        TabAlignment::Decimal => decimal_anchor(zone, measure_text, separator),
        // Unreachable in practice: `find_next_tab_stop` never hands back a
        // rule stop, so no zone is ever anchored against one. Kept so the
        // match stays total, and answering `Start` is the harmless reading if
        // a future caller does reach it.
        TabAlignment::Bar => ZoneAnchor::Start,
    }
}

/// Offset of the first `separator` in `zone` — the paragraph's decimal
/// character, per §17.3.2.20 `w:lang`, not a constant.
///
/// Falls back to [`ZoneAnchor::End`] when the zone contains none: Word
/// right-aligns a separator-less decimal zone, which is what keeps a column of
/// figures flush when one entry is a whole number. Left-aligning it instead
/// would make that one entry stick out by its full width.
fn decimal_anchor(
    zone: &[Fragment],
    measure_text: MeasureTextFn<'_>,
    separator: char,
) -> ZoneAnchor {
    let mut before = Pt::ZERO;
    for fragment in zone {
        if let Fragment::Text {
            text, font, width, ..
        } = fragment
            && let Some(byte_idx) = text.find(separator)
        {
            let prefix = &text[..byte_idx];
            let prefix_width = match measure_text {
                Some(measure) => measure(prefix, font).0,
                // No measurer (fitting-only paths): apportion the
                // fragment's known width by character share. Approximate
                // for proportional faces, but the alternative is ignoring
                // the separator entirely.
                None => {
                    let total = text.chars().count();
                    if total == 0 {
                        Pt::ZERO
                    } else {
                        *width * (prefix.chars().count() as f32 / total as f32)
                    }
                }
            };
            return ZoneAnchor::At(before + prefix_width);
        }
        before += fragment.width();
    }
    ZoneAnchor::End
}

/// §17.3.1.38: how a tab leader is drawn.
///
/// A leader has no formatting of its own — it takes the formatting in effect
/// at the tab, i.e. the `<w:rPr>` of the run carrying the `<w:tab/>` or
/// `<w:ptab/>`. Bundling the three fields keeps them travelling together: the
/// glyph, the font it is measured *and* drawn in, and its colour are one
/// decision, and splitting them is how the hardcoded-Times-New-Roman default
/// survived as long as it did.
pub(super) struct LeaderStyle<'a> {
    pub leader: crate::model::TabLeader,
    pub font: &'a super::super::fragment::FontProps,
    pub color: crate::render::resolve::color::RgbColor,
}

/// Emit leader characters (dots, hyphens, etc.) between tab start and end.
pub(super) fn emit_tab_leader(
    commands: &mut Vec<DrawCommand>,
    style: LeaderStyle<'_>,
    x_start: Pt,
    x_end: Pt,
    baseline_y: Pt,
    measure_text: MeasureTextFn<'_>,
) {
    use crate::model::TabLeader;

    let LeaderStyle {
        leader,
        font,
        color,
    } = style;
    let leader_char = match leader {
        TabLeader::Dot => ".",
        TabLeader::Hyphen => "-",
        TabLeader::Underscore => "_",
        TabLeader::MiddleDot => "\u{00B7}",
        TabLeader::Heavy => "_",
        TabLeader::None => return,
    };

    let gap = x_end - x_start;
    if gap <= Pt::ZERO {
        return;
    }

    // Measure the leader glyph in the run's own font, so the repeat count
    // matches the glyphs actually drawn below. Character spacing and text
    // scaling are run-level effects on *text*, not on a fill pattern, so the
    // measurement font drops them while keeping family/size/style.
    let leader_font = super::super::fragment::FontProps {
        char_spacing: Pt::ZERO,
        text_scale: 1.0,
        ..font.clone()
    };

    let char_width = if let Some(m) = measure_text {
        m(leader_char, &leader_font).0
    } else {
        LEADER_CHAR_WIDTH_FALLBACK
    };

    if char_width <= Pt::ZERO {
        return;
    }

    let count = ((gap / char_width) as usize).min(500);
    if count == 0 {
        return;
    }

    let leader_text: String = leader_char.repeat(count);
    let leader_width = char_width * count as f32;

    // Right-align the leader dots within the gap (looks cleaner).
    let leader_x = x_end - leader_width;

    commands.push(DrawCommand::Text {
        position: PtOffset::new(leader_x.max(x_start), baseline_y),
        text: Rc::from(leader_text.as_str()),
        font_family: leader_font.family,
        char_spacing: Pt::ZERO,
        font_size: leader_font.size,
        bold: leader_font.bold,
        italic: leader_font.italic,
        color,
        text_scale: 1.0,
    });
}

/// §17.3.1.33: resolve the effective line height from the natural height
/// and the line spacing rule.
///
/// For Auto mode, the multiplier applies only to text metrics — inline
/// images use their natural height without scaling. The final line height
/// is `max(text_height * multiplier, total_height)`.
/// §17.3.1.33 line height for one line, then §20.1.2.1.18 `@lnSpcReduction`.
///
/// `auto_fit` is a parameter rather than something callers apply afterwards
/// because four separate places resolve a line height — fitting, emission,
/// the border box and the empty-paragraph path — and they must agree to the
/// point. Owning the reduction here is what makes forgetting it impossible;
/// applying it *after* the match is what lets it cross the `Auto` floor below,
/// which is the whole point of the attribute (see
/// [`ShapeAutoFit::scale_line_height`](crate::render::layout::ShapeAutoFit::scale_line_height)).
pub(super) fn resolve_line_height(
    natural: Pt,
    text_height: Pt,
    rule: &LineSpacingRule,
    auto_fit: crate::render::layout::ShapeAutoFit,
) -> Pt {
    let resolved = match rule {
        LineSpacingRule::Auto(multiplier) => {
            let scaled_text = text_height * *multiplier;
            // Use the scaled text height or the full natural height (which
            // includes images), whichever is larger.
            scaled_text.max(natural)
        }
        LineSpacingRule::Exact(h) => *h,
        LineSpacingRule::AtLeast(min) => natural.max(*min),
    };
    auto_fit.scale_line_height(resolved)
}

#[cfg(test)]
mod tests {
    use super::{
        Fragment, PTabGeometry, PTabPlacement, ZoneAnchor, find_next_tab_stop, resolve_ptab,
        resolve_zone_anchor,
    };
    use crate::model;
    use crate::render::dimension::Pt;
    use crate::render::layout::paragraph::TabStopDef;

    // ── find_next_tab_stop ────────────────────────────────────────────────────

    #[test]
    fn default_interval_36pt_advances_half_inch() {
        // No custom stops: land on the next multiple of the default interval.
        let (pos, ts) = find_next_tab_stop(Pt::ZERO, &[], Pt::new(1000.0), Pt::new(36.0));
        assert_eq!(pos.raw(), 36.0);
        assert!(ts.is_none());
        let (pos, _) = find_next_tab_stop(Pt::new(40.0), &[], Pt::new(1000.0), Pt::new(36.0));
        assert_eq!(pos.raw(), 72.0);
    }

    #[test]
    fn custom_default_interval_is_honored() {
        // A custom w:defaultTabStop (e.g. 1440tw = 72pt / 1 inch) changes the grid.
        let (pos, _) = find_next_tab_stop(Pt::ZERO, &[], Pt::new(1000.0), Pt::new(72.0));
        assert_eq!(pos.raw(), 72.0);
        let (pos, _) = find_next_tab_stop(Pt::new(80.0), &[], Pt::new(1000.0), Pt::new(72.0));
        assert_eq!(pos.raw(), 144.0);
    }

    #[test]
    fn zero_interval_falls_back_to_36pt() {
        // A degenerate w:defaultTabStop of 0 must not stall the tab grid.
        let (pos, _) = find_next_tab_stop(Pt::ZERO, &[], Pt::new(1000.0), Pt::ZERO);
        assert_eq!(pos.raw(), 36.0);
    }

    #[test]
    fn custom_tab_stop_preferred_over_default_grid() {
        // A 72pt custom stop beats the 36pt default grid when the cursor is before it.
        let tabs = vec![TabStopDef {
            position: Pt::new(72.0),
            alignment: model::TabAlignment::Left,
            leader: model::TabLeader::None,
        }];
        let (pos, ts) = find_next_tab_stop(Pt::new(10.0), &tabs, Pt::new(1000.0), Pt::new(36.0));
        assert_eq!(pos.raw(), 72.0, "custom stop wins over 36pt grid");
        assert!(ts.is_some(), "custom TabStopDef returned");
    }

    #[test]
    fn cursor_past_all_custom_stops_falls_back_to_default_grid() {
        // All custom stops are behind the cursor — fall back to the default interval.
        let tabs = vec![TabStopDef {
            position: Pt::new(72.0),
            alignment: model::TabAlignment::Left,
            leader: model::TabLeader::None,
        }];
        // cursor=80 → floor(80/36)+1 = 2+1 = 3 → 3×36 = 108
        let (pos, ts) = find_next_tab_stop(Pt::new(80.0), &tabs, Pt::new(1000.0), Pt::new(36.0));
        assert_eq!(pos.raw(), 108.0, "default grid past the custom stop");
        assert!(ts.is_none(), "no custom stop returned");
    }

    // ── ZoneAnchor (§17.18.85) ────────────────────────────────────────────────

    /// A text fragment of known width, for anchor arithmetic.
    fn text_fragment(text: &str, width: f32) -> Fragment {
        use crate::render::layout::fragment::{FontProps, TextMetrics};
        use std::rc::Rc;
        Fragment::Text {
            text: text.into(),
            font: Rc::new(FontProps {
                family: Rc::from("Test"),
                size: Pt::new(12.0),
                bold: false,
                italic: false,
                underline: false,
                char_spacing: Pt::ZERO,
                text_scale: 1.0,
                underline_position: Pt::ZERO,
                underline_thickness: Pt::ZERO,
            }),
            color: crate::render::resolve::color::RgbColor::BLACK,
            shading: None,
            border: None,
            width: Pt::new(width),
            trimmed_width: Pt::new(width),
            metrics: TextMetrics {
                ascent: Pt::new(10.0),
                descent: Pt::new(4.0),
                leading: Pt::ZERO,
            },
            hyperlink_url: None,
            baseline_offset: Pt::ZERO,
            text_offset: Pt::ZERO,
            is_footnote_ref: false,
        }
    }

    #[test]
    fn every_alignment_resolves_without_a_catch_all() {
        // The point of the enum: each §17.18.85 value states its own anchor.
        // `bar` and `clear` do not reposition following content, so they
        // anchor at the zone start exactly as `left` does.
        use model::TabAlignment::*;
        for (alignment, expected) in [
            (Left, ZoneAnchor::Start),
            (Bar, ZoneAnchor::Start),
            (Clear, ZoneAnchor::Start),
            (Right, ZoneAnchor::End),
            (Center, ZoneAnchor::Middle),
        ] {
            assert_eq!(
                resolve_zone_anchor(alignment, &[], None, '.'),
                expected,
                "{alignment:?}"
            );
        }
    }

    #[test]
    fn a_start_anchor_never_sums_the_zone_width() {
        // The thunk exists so the common case skips an O(zone) walk; if it is
        // ever called for `Start` this panics.
        let offset = ZoneAnchor::Start.offset(|| panic!("zone width computed for a Start anchor"));
        assert_eq!(offset, Pt::ZERO);
    }

    #[test]
    fn zone_anchor_offsets_are_measured_from_the_zone_start() {
        let width = || Pt::new(80.0);
        assert_eq!(ZoneAnchor::End.offset(width).raw(), 80.0);
        assert_eq!(ZoneAnchor::Middle.offset(width).raw(), 40.0);
        assert_eq!(ZoneAnchor::At(Pt::new(12.5)).offset(width).raw(), 12.5);
    }

    #[test]
    fn a_decimal_zone_with_no_separator_anchors_at_its_end() {
        let zone = [text_fragment("1234", 40.0)];
        assert_eq!(
            resolve_zone_anchor(model::TabAlignment::Decimal, &zone, None, '.'),
            ZoneAnchor::End,
            "right-align rather than left-align, so a whole number stays flush"
        );
    }

    #[test]
    fn a_decimal_zone_without_a_measurer_apportions_by_character_share() {
        // "12.5" is 4 chars, 40pt wide; the 2-char prefix is half of it.
        let zone = [text_fragment("12.5", 40.0)];
        assert_eq!(
            resolve_zone_anchor(model::TabAlignment::Decimal, &zone, None, '.'),
            ZoneAnchor::At(Pt::new(20.0))
        );
    }

    #[test]
    fn a_decimal_anchor_accumulates_fragments_before_the_separator() {
        // The separator can arrive in a later fragment; everything before it
        // still counts toward the offset.
        let zone = [text_fragment("12", 20.0), text_fragment(".5", 20.0)];
        assert_eq!(
            resolve_zone_anchor(model::TabAlignment::Decimal, &zone, None, '.'),
            ZoneAnchor::At(Pt::new(20.0)),
            "20pt of leading fragment + 0pt before the separator in the second"
        );
    }

    // ── resolve_ptab (§17.3.1.30) ─────────────────────────────────────────────

    fn ptab_geometry() -> PTabGeometry {
        PTabGeometry {
            max_width: Pt::new(400.0),
            indent_left: Pt::ZERO,
            indent_first_line: Pt::ZERO,
            content_width: Pt::new(400.0),
            float_left: Pt::ZERO,
            float_right: Pt::ZERO,
        }
    }

    #[test]
    fn a_left_ptab_never_sums_its_zone() {
        // `left` anchors at the span start, which does not depend on the zone
        // at all — so the O(zone) walk must not run. Same guarantee as
        // `ZoneAnchor::Start`, and the reason the width arrives as a thunk.
        let placement = resolve_ptab(
            model::PTabAlignment::Left,
            model::PTabRelativeTo::Margin,
            ptab_geometry(),
            Pt::ZERO,
            || panic!("zone width computed for a left position tab"),
        );
        assert_eq!(placement, PTabPlacement::Placed(Pt::ZERO));
    }

    #[test]
    fn right_and_centre_ptabs_measure_the_zone_once() {
        // The counterpart: these do depend on the zone, and each must call the
        // thunk exactly once — not per alignment branch.
        for (align, expected) in [
            (model::PTabAlignment::Right, 300.0),
            (model::PTabAlignment::Center, 150.0),
        ] {
            let calls = std::cell::Cell::new(0);
            let placement = resolve_ptab(
                align,
                model::PTabRelativeTo::Margin,
                ptab_geometry(),
                Pt::ZERO,
                || {
                    calls.set(calls.get() + 1);
                    Pt::new(100.0)
                },
            );
            assert_eq!(
                placement,
                PTabPlacement::Placed(Pt::new(expected)),
                "{align:?}"
            );
            assert_eq!(calls.get(), 1, "{align:?} measures the zone once");
        }
    }
}
