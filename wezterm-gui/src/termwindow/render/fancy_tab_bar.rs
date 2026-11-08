use crate::color::LinearRgba;
use crate::customglyph::*;
use crate::quad::QuadTrait;
use crate::tabbar::{TabBarItem, TabEntry};
use crate::termwindow::box_model::*;
use crate::termwindow::render::corners::*;

use crate::termwindow::render::window_buttons::window_button_element;
use crate::termwindow::{RenderState, UIItem, UIItemType};
use crate::utilsprites::RenderMetrics;
use config::{Dimension, DimensionContext, TabBarColors};
use std::rc::Rc;
use wezterm_font::LoadedFont;
use wezterm_term::color::{ColorAttribute, ColorPalette};
use window::{IntegratedTitleButtonAlignment, IntegratedTitleButtonStyle, RectF};

/// Half a stroke in from the near edge / far edge of a corner glyph, so a
/// stroke of `line_width` lands inside the tab's border pixels.
const NEAR: BlockCoord = BlockCoord::FracWithOffset(0, 1, LineScale::Div(2));
const FAR: BlockCoord = BlockCoord::FracWithOffset(1, 1, LineScale::Div(-2));

const TOP_LEFT_ARC: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(NEAR, BlockCoord::One),
        PolyCommand::QuadTo {
            control: (NEAR, NEAR),
            to: (BlockCoord::One, NEAR),
        },
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Outline,
}];

const TOP_RIGHT_ARC: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Zero, NEAR),
        PolyCommand::QuadTo {
            control: (FAR, NEAR),
            to: (FAR, BlockCoord::One),
        },
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Outline,
}];

const BOTTOM_LEFT_ARC: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(NEAR, BlockCoord::Zero),
        PolyCommand::QuadTo {
            control: (NEAR, FAR),
            to: (BlockCoord::One, FAR),
        },
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Outline,
}];

const BOTTOM_RIGHT_ARC: &[Poly] = &[Poly {
    path: &[
        PolyCommand::MoveTo(BlockCoord::Zero, FAR),
        PolyCommand::QuadTo {
            control: (FAR, FAR),
            to: (FAR, BlockCoord::Zero),
        },
    ],
    intensity: BlockAlpha::Full,
    style: PolyStyle::Outline,
}];

/// Opacity of the waiting glow rings, innermost first.
const GLOW_ALPHA: [f32; 3] = [0.5, 0.26, 0.12];
/// Above the tab bar (zindex 10 + children), below modal overlays (100).
const DECORATION_ZINDEX: i8 = 20;
/// Corner radius of every tab, as a fraction of the title font's cell
/// height (same radius on both axes, so the corners are circular).
const TAB_CORNER_RADIUS_CELLS: f32 = 0.40;
/// Radius of the flare where the active tab's door meets the separator,
/// as a fraction of the title font's cell height.
const TAB_DOOR_FILLET_CELLS: f32 = 0.30;

#[derive(Debug, Clone, Copy, PartialEq)]
enum DecoCorner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl DecoCorner {
    fn poly(self) -> &'static [Poly] {
        match self {
            Self::TopLeft => TOP_LEFT_ARC,
            Self::TopRight => TOP_RIGHT_ARC,
            Self::BottomLeft => BOTTOM_LEFT_ARC,
            Self::BottomRight => BOTTOM_RIGHT_ARC,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Deco {
    Rect {
        rect: RectF,
        color: LinearRgba,
    },
    Arc {
        corner: DecoCorner,
        rect: RectF,
        line_width: f32,
        color: LinearRgba,
        /// Fill inside the arc. When known, the arc is painted as a filled
        /// quarter disc in `color` with a disc of `inner` inset by
        /// `line_width` on top: both edges are antialiased fills, so the arc
        /// matches the straight strokes. Without it the arc is stroked.
        inner: Option<LinearRgba>,
    },
    /// A solid quarter disc, used to round a convex corner of the strokes.
    Quarter {
        corner: DecoCorner,
        rect: RectF,
        color: LinearRgba,
    },
}

/// (width, height) of each rounded corner; (0, 0) for a square corner.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct CornerSizes {
    top_left: (f32, f32),
    top_right: (f32, f32),
    bottom_left: (f32, f32),
    bottom_right: (f32, f32),
}

impl CornerSizes {
    fn from_pixel(corners: Option<&PixelCorners>) -> Self {
        fn size(p: &PixelSizedPoly) -> (f32, f32) {
            if p.poly.is_empty() || p.width <= 0. || p.height <= 0. {
                (0., 0.)
            } else {
                (p.width, p.height)
            }
        }
        match corners {
            Some(c) => Self {
                top_left: size(&c.top_left),
                top_right: size(&c.top_right),
                bottom_left: size(&c.bottom_left),
                bottom_right: size(&c.bottom_right),
            },
            None => Self::default(),
        }
    }

    /// Concentric corners for a ring `by` pixels outside this one.
    fn grow(self, by: f32) -> Self {
        fn g(c: (f32, f32), by: f32) -> (f32, f32) {
            if c.0 > 0. && c.1 > 0. {
                (c.0 + by, c.1 + by)
            } else {
                (0., 0.)
            }
        }
        Self {
            top_left: g(self.top_left, by),
            top_right: g(self.top_right, by),
            bottom_left: g(self.bottom_left, by),
            bottom_right: g(self.bottom_right, by),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct TabGeom {
    active: bool,
    waiting: bool,
    /// The tab's border rect.
    rect: RectF,
    corners: CornerSizes,
    /// The tab's current fill (hover-aware), when it is a plain color.
    fill: Option<LinearRgba>,
    /// Accent declared by format-tab-title (U+001D); colors the separator
    /// and door while this tab is active.
    accent: Option<LinearRgba>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct DecoStyle {
    /// Separator and active-door color; None disables both.
    active_edge: Option<LinearRgba>,
    waiting_edge: LinearRgba,
    glow: LinearRgba,
    /// Stroke width of the waiting ring.
    line: f32,
    /// Stroke width of the separator and the active door.
    edge_line: f32,
    /// Width of one glow ring.
    ring: f32,
    /// Height of the waiting indicator bar.
    lamp: f32,
    /// Radius where the door's side strokes meet the separator (the near
    /// edge), so the junction flares out instead of meeting at a right
    /// angle.
    fillet: f32,
    at_bottom: bool,
}

/// Radius of the flare where the active tab's door meets the separator. At
/// least one stroke wide so the rounding never pinches the line.
fn door_fillet(style: &DecoStyle, rect: RectF) -> f32 {
    style
        .fillet
        .min(rect.width() / 2.)
        .max(style.edge_line.round().max(1.))
}

fn push_rect(out: &mut Vec<Deco>, rect: RectF, color: LinearRgba) {
    if rect.width() > 0. && rect.height() > 0. {
        out.push(Deco::Rect { rect, color });
    }
}

/// Stroke the outline of `r`, rounded where `corners` says so. An open side
/// is skipped and its corners are treated as square, so the side strokes
/// run all the way to it, less `near_trim` (the door's bottom fillet).
/// `inner` is the fill inside the outline (see `Deco::Arc`).
fn push_outline(
    out: &mut Vec<Deco>,
    r: RectF,
    corners: CornerSizes,
    line: f32,
    color: LinearRgba,
    inner: Option<LinearRgba>,
    open_top: bool,
    open_bottom: bool,
    near_trim: f32,
) {
    let zero = (0., 0.);
    let tl = if open_top { zero } else { corners.top_left };
    let tr = if open_top { zero } else { corners.top_right };
    let bl = if open_bottom {
        zero
    } else {
        corners.bottom_left
    };
    let br = if open_bottom {
        zero
    } else {
        corners.bottom_right
    };
    let top_trim = if open_top { near_trim } else { 0. };
    let bottom_trim = if open_bottom { near_trim } else { 0. };

    push_rect(
        out,
        euclid::rect(
            r.min_x(),
            r.min_y() + tl.1 + top_trim,
            line,
            r.height() - tl.1 - bl.1 - top_trim - bottom_trim,
        ),
        color,
    );
    push_rect(
        out,
        euclid::rect(
            r.max_x() - line,
            r.min_y() + tr.1 + top_trim,
            line,
            r.height() - tr.1 - br.1 - top_trim - bottom_trim,
        ),
        color,
    );
    if !open_top {
        push_rect(
            out,
            euclid::rect(r.min_x() + tl.0, r.min_y(), r.width() - tl.0 - tr.0, line),
            color,
        );
    }
    if !open_bottom {
        push_rect(
            out,
            euclid::rect(
                r.min_x() + bl.0,
                r.max_y() - line,
                r.width() - bl.0 - br.0,
                line,
            ),
            color,
        );
    }
    for (corner, size, origin) in [
        (DecoCorner::TopLeft, tl, (r.min_x(), r.min_y())),
        (DecoCorner::TopRight, tr, (r.max_x() - tr.0, r.min_y())),
        (DecoCorner::BottomLeft, bl, (r.min_x(), r.max_y() - bl.1)),
        (
            DecoCorner::BottomRight,
            br,
            (r.max_x() - br.0, r.max_y() - br.1),
        ),
    ] {
        if size.0 > 0. && size.1 > 0. {
            out.push(Deco::Arc {
                corner,
                rect: euclid::rect(origin.0, origin.1, size.0, size.1),
                line_width: line,
                color,
                inner,
            });
        }
    }
}

/// Lay out the tab bar decorations in paint order: waiting glow, separator,
/// active door and waiting rings, indicator bars.
///
/// The "far" edge of a tab faces away from the terminal content (the top
/// edge for a top tab bar); the separator runs along the bar's near edge.
fn tab_bar_decorations(bar: RectF, tabs: &[TabGeom], style: &DecoStyle) -> Vec<Deco> {
    let mut out = vec![];
    let at_bottom = style.at_bottom;
    let line = style.line;

    // Glow: concentric rings outside the far edge and both sides, open on
    // the near edge. Only as many rings as fit between the tab and the bar.
    for tab in tabs.iter().filter(|t| t.waiting) {
        let r = tab.rect;
        let room = if at_bottom {
            bar.max_y() - r.max_y()
        } else {
            r.min_y() - bar.min_y()
        };
        let rings = ((room / style.ring).floor().max(0.) as usize).min(GLOW_ALPHA.len());
        for (i, alpha) in GLOW_ALPHA.iter().take(rings).enumerate() {
            let off = style.ring * (i + 1) as f32;
            let top = if at_bottom {
                r.min_y()
            } else {
                r.min_y() - off
            };
            let grect = euclid::rect(r.min_x() - off, top, r.width() + 2. * off, r.height() + off);
            let mut ring = vec![];
            push_outline(
                &mut ring,
                grect,
                tab.corners.grow(off),
                style.ring,
                style.glow.mul_alpha(*alpha),
                None,
                at_bottom,
                !at_bottom,
                0.,
            );
            for deco in ring {
                match deco {
                    Deco::Rect { rect, color } => {
                        if let Some(rect) = rect.intersection(&bar) {
                            push_rect(&mut out, rect, color);
                        }
                    }
                    arc => out.push(arc),
                }
            }
        }
    }

    if let Some(default_edge) = style.active_edge {
        let line = style.edge_line;
        let active = tabs.iter().find(|t| t.active);
        // The active tab's accent colors the whole separator, tying the
        // content area to the selected tab.
        let edge = active.and_then(|a| a.accent).unwrap_or(default_edge);
        let y = if at_bottom {
            bar.min_y()
        } else {
            bar.max_y() - line
        };
        match active {
            Some(a) => {
                let r = a.rect;
                // Fillet radius where the door meets the separator.
                let fillet = door_fillet(style, r);
                let door = if a.waiting { style.waiting_edge } else { edge };
                // The separator stops a fillet short of the door on each
                // side; the quarter discs below join it to the door posts.
                push_rect(
                    &mut out,
                    euclid::rect(bar.min_x(), y, r.min_x() + fillet - bar.min_x(), line),
                    edge,
                );
                push_rect(
                    &mut out,
                    euclid::rect(
                        r.max_x() - fillet,
                        y,
                        bar.max_x() - (r.max_x() - fillet),
                        line,
                    ),
                    edge,
                );
                // If the tab does not reach the separator, extend the door
                // posts to the fillet.
                let (gap_y, gap_end) = if at_bottom {
                    (bar.min_y() + fillet, r.min_y())
                } else {
                    (r.max_y(), bar.max_y() - fillet)
                };
                for x in [r.min_x(), r.max_x() - line] {
                    push_rect(
                        &mut out,
                        euclid::rect(x, gap_y, line, gap_end - gap_y),
                        door,
                    );
                }
                let near = if at_bottom {
                    (bar.min_y(), DecoCorner::BottomRight, DecoCorner::BottomLeft)
                } else {
                    (
                        bar.max_y() - fillet,
                        DecoCorner::TopRight,
                        DecoCorner::TopLeft,
                    )
                };
                for (x, corner) in [(r.min_x(), near.1), (r.max_x() - fillet, near.2)] {
                    out.push(Deco::Quarter {
                        corner,
                        rect: euclid::rect(x, near.0, fillet, fillet),
                        color: door,
                    });
                }
            }
            None => push_rect(
                &mut out,
                euclid::rect(bar.min_x(), y, bar.width(), line),
                edge,
            ),
        }
    }

    for tab in tabs {
        match (tab.active, style.active_edge) {
            (true, Some(edge)) => {
                let color = if tab.waiting {
                    style.waiting_edge
                } else {
                    tab.accent.unwrap_or(edge)
                };
                push_outline(
                    &mut out,
                    tab.rect,
                    tab.corners,
                    style.edge_line,
                    color,
                    tab.fill,
                    at_bottom,
                    !at_bottom,
                    door_fillet(style, tab.rect),
                );
            }
            _ if tab.waiting => push_outline(
                &mut out,
                tab.rect,
                tab.corners,
                line,
                style.waiting_edge,
                tab.fill,
                false,
                false,
                0.,
            ),
            _ => {}
        }
    }

    for tab in tabs.iter().filter(|t| t.waiting) {
        let r = tab.rect;
        let c = tab.corners;
        let lamp = if at_bottom {
            euclid::rect(
                r.min_x() + c.bottom_left.0,
                r.max_y() - style.lamp,
                r.width() - c.bottom_left.0 - c.bottom_right.0,
                style.lamp,
            )
        } else {
            euclid::rect(
                r.min_x() + c.top_left.0,
                r.min_y(),
                r.width() - c.top_left.0 - c.top_right.0,
                style.lamp,
            )
        };
        push_rect(&mut out, lamp, style.glow);
    }

    out
}

fn collect_tab_geoms(
    element: &ComputedElement,
    info: &dyn Fn(usize) -> (bool, Option<LinearRgba>),
    fill: &dyn Fn(&ComputedElement) -> Option<LinearRgba>,
    out: &mut Vec<TabGeom>,
) {
    if let Some(UIItemType::TabBar(TabBarItem::Tab { tab_idx, active })) = &element.item_type {
        // The first match is the tab itself; its right-floated suffix reuses
        // the same item type, so do not descend any further.
        let (waiting, accent) = info(*tab_idx);
        out.push(TabGeom {
            active: *active,
            waiting,
            rect: element.border_rect,
            corners: CornerSizes::from_pixel(element.border_corners.as_ref()),
            fill: fill(element),
            accent,
        });
        return;
    }
    if let ComputedElementContent::Children(kids) = &element.content {
        for kid in kids {
            collect_tab_geoms(kid, info, fill, out);
        }
    }
}

impl DecoCorner {
    fn filled_poly(self) -> &'static [Poly] {
        match self {
            Self::TopLeft => TOP_LEFT_ROUNDED_CORNER,
            Self::TopRight => TOP_RIGHT_ROUNDED_CORNER,
            Self::BottomLeft => BOTTOM_LEFT_ROUNDED_CORNER,
            Self::BottomRight => BOTTOM_RIGHT_ROUNDED_CORNER,
        }
    }

    /// The quad of the concentric disc `inset` pixels inside the one that
    /// fills `rect`. The disc center is the rect corner facing the tab body.
    fn inset(self, rect: RectF, inset: f32) -> RectF {
        let w = (rect.width() - inset).max(0.);
        let h = (rect.height() - inset).max(0.);
        let (x, y) = match self {
            Self::TopLeft => (rect.min_x() + inset, rect.min_y() + inset),
            Self::TopRight => (rect.min_x(), rect.min_y() + inset),
            Self::BottomLeft => (rect.min_x() + inset, rect.min_y()),
            Self::BottomRight => (rect.min_x(), rect.min_y()),
        };
        euclid::rect(x, y, w, h)
    }
}

const X_BUTTON: &[Poly] = &[
    Poly {
        path: &[
            PolyCommand::MoveTo(BlockCoord::One, BlockCoord::Zero),
            PolyCommand::LineTo(BlockCoord::Zero, BlockCoord::One),
        ],
        intensity: BlockAlpha::Full,
        style: PolyStyle::Outline,
    },
    Poly {
        path: &[
            PolyCommand::MoveTo(BlockCoord::Zero, BlockCoord::Zero),
            PolyCommand::LineTo(BlockCoord::One, BlockCoord::One),
        ],
        intensity: BlockAlpha::Full,
        style: PolyStyle::Outline,
    },
];

const PLUS_BUTTON: &[Poly] = &[
    Poly {
        path: &[
            PolyCommand::MoveTo(BlockCoord::Frac(1, 2), BlockCoord::Zero),
            PolyCommand::LineTo(BlockCoord::Frac(1, 2), BlockCoord::One),
        ],
        intensity: BlockAlpha::Full,
        style: PolyStyle::Outline,
    },
    Poly {
        path: &[
            PolyCommand::MoveTo(BlockCoord::Zero, BlockCoord::Frac(1, 2)),
            PolyCommand::LineTo(BlockCoord::One, BlockCoord::Frac(1, 2)),
        ],
        intensity: BlockAlpha::Full,
        style: PolyStyle::Outline,
    },
];

impl crate::TermWindow {
    pub fn invalidate_fancy_tab_bar(&mut self) {
        self.fancy_tab_bar.take();
    }

    pub fn build_fancy_tab_bar(&self, palette: &ColorPalette) -> anyhow::Result<ComputedElement> {
        let tab_bar_height = self.tab_bar_pixel_height()?;
        let font = self.fonts.title_font()?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let items = self.tab_bar.items();
        let colors = self
            .config
            .colors
            .as_ref()
            .and_then(|c| c.tab_bar.as_ref())
            .cloned()
            .unwrap_or_else(TabBarColors::default);

        let mut left_status = vec![];
        let mut left_eles = vec![];
        let mut right_eles = vec![];
        let bar_colors = ElementColors {
            border: BorderColor::default(),
            bg: if self.focused.is_some() {
                self.config.window_frame.active_titlebar_bg
            } else {
                self.config.window_frame.inactive_titlebar_bg
            }
            .to_linear()
            .into(),
            text: if self.focused.is_some() {
                self.config.window_frame.active_titlebar_fg
            } else {
                self.config.window_frame.inactive_titlebar_fg
            }
            .to_linear()
            .into(),
        };
        let tab_vertical_alignment = if self.config.tab_bar_at_bottom {
            VerticalAlign::Top
        } else {
            VerticalAlign::Bottom
        };
        // Upstream hardcodes a 1px tab border. Make it configurable so the
        // inactive-tab divider can be made heavier (and scale with DPI) to
        // keep adjacent tabs visually distinct.
        let border_width =
            self.config
                .tab_bar_tab_border_width
                .evaluate_as_pixels(DimensionContext {
                    dpi: self.dimensions.dpi as f32,
                    pixel_max: self.dimensions.pixel_width as f32,
                    pixel_cell: metrics.cell_size.width as f32,
                });
        // Upstream sizes the rounded tab corners as half a cell in each
        // direction, which is an ellipse (cells are taller than wide) and
        // reads as a chamfer once the corner is outlined. Use one pixel
        // radius for both axes so the corners are circular.
        let corner_radius = Dimension::Pixels(
            (metrics.cell_size.height as f32 * TAB_CORNER_RADIUS_CELLS)
                .round()
                .max(2.),
        );

        let item_to_elem = |item: &TabEntry| -> Element {
            let element = Element::with_line(&font, &item.title, palette);

            let bg_color = item
                .title
                .get_cell(0)
                .and_then(|c| match c.attrs().background() {
                    ColorAttribute::Default => None,
                    col => Some(palette.resolve_bg(col)),
                });
            let fg_color = item
                .title
                .get_cell(0)
                .and_then(|c| match c.attrs().foreground() {
                    ColorAttribute::Default => None,
                    col => Some(palette.resolve_fg(col)),
                });

            let new_tab = colors.new_tab();
            let new_tab_hover = colors.new_tab_hover();
            let active_tab = colors.active_tab();
            let is_bottom = self.config.tab_bar_at_bottom;

            match item.item {
                TabBarItem::RightStatus | TabBarItem::LeftStatus | TabBarItem::None => element
                    .item_type(UIItemType::TabBar(TabBarItem::None))
                    .line_height(Some(1.75))
                    .margin(BoxDimension {
                        left: Dimension::Cells(0.),
                        right: Dimension::Cells(0.),
                        top: Dimension::Cells(0.0),
                        bottom: Dimension::Cells(0.),
                    })
                    .padding(BoxDimension {
                        left: Dimension::Cells(0.5),
                        right: Dimension::Cells(0.),
                        top: Dimension::Cells(0.),
                        bottom: Dimension::Cells(0.),
                    })
                    .border(BoxDimension::new(Dimension::Pixels(0.)))
                    .colors(bar_colors.clone()),
                TabBarItem::NewTabButton => Element::new(
                    &font,
                    ElementContent::Poly {
                        line_width: metrics.underline_height.max(2),
                        poly: SizedPoly {
                            poly: PLUS_BUTTON,
                            width: Dimension::Pixels(metrics.cell_size.height as f32 / 2.),
                            height: Dimension::Pixels(metrics.cell_size.height as f32 / 2.),
                        },
                    },
                )
                .vertical_align(VerticalAlign::Middle)
                .item_type(UIItemType::TabBar(item.item.clone()))
                .margin(BoxDimension {
                    left: Dimension::Cells(0.5),
                    right: Dimension::Cells(0.),
                    top: Dimension::Cells(0.2),
                    bottom: Dimension::Cells(0.),
                })
                .padding(BoxDimension {
                    left: Dimension::Cells(0.5),
                    right: Dimension::Cells(0.5),
                    top: Dimension::Cells(0.2),
                    bottom: Dimension::Cells(0.25),
                })
                .border(BoxDimension::new(Dimension::Pixels(1.)))
                .colors(ElementColors {
                    border: BorderColor::default(),
                    bg: new_tab.bg_color.to_linear().into(),
                    text: new_tab.fg_color.to_linear().into(),
                })
                .hover_colors(Some(ElementColors {
                    border: BorderColor::default(),
                    bg: new_tab_hover.bg_color.to_linear().into(),
                    text: new_tab_hover.fg_color.to_linear().into(),
                })),
                TabBarItem::Tab { active, .. } if active => element
                    .vertical_align(tab_vertical_alignment)
                    .item_type(UIItemType::TabBar(item.item.clone()))
                    .margin(if is_bottom {
                        BoxDimension {
                            left: Dimension::Cells(0.),
                            right: Dimension::Cells(0.),
                            top: Dimension::Cells(0.),
                            bottom: Dimension::Cells(0.2),
                        }
                    } else {
                        BoxDimension {
                            left: Dimension::Cells(0.),
                            right: Dimension::Cells(0.),
                            top: Dimension::Cells(0.2),
                            bottom: Dimension::Cells(0.),
                        }
                    })
                    .padding(BoxDimension {
                        left: Dimension::Cells(0.5),
                        right: Dimension::Cells(0.5),
                        top: Dimension::Cells(0.25),
                        bottom: Dimension::Cells(0.25),
                    })
                    .border(BoxDimension::new(Dimension::Pixels(border_width)))
                    .border_corners(Some(if is_bottom {
                        Corners {
                            top_left: SizedPoly::none(),
                            top_right: SizedPoly::none(),
                            bottom_left: SizedPoly {
                                width: corner_radius,
                                height: corner_radius,
                                poly: BOTTOM_LEFT_ROUNDED_CORNER,
                            },
                            bottom_right: SizedPoly {
                                width: corner_radius,
                                height: corner_radius,
                                poly: BOTTOM_RIGHT_ROUNDED_CORNER,
                            },
                        }
                    } else {
                        Corners {
                            top_left: SizedPoly {
                                width: corner_radius,
                                height: corner_radius,
                                poly: TOP_LEFT_ROUNDED_CORNER,
                            },
                            top_right: SizedPoly {
                                width: corner_radius,
                                height: corner_radius,
                                poly: TOP_RIGHT_ROUNDED_CORNER,
                            },
                            bottom_left: SizedPoly::none(),
                            bottom_right: SizedPoly::none(),
                        }
                    }))
                    .colors(ElementColors {
                        // The active door and the waiting ring are stroked by
                        // paint_tab_bar_decorations. The element border keeps
                        // the fill color: rounded corners are filled quarter
                        // discs in the border color, so a colored border would
                        // turn them into solid blobs.
                        border: BorderColor::new(
                            bg_color
                                .unwrap_or_else(|| active_tab.bg_color.into())
                                .to_linear(),
                        ),
                        bg: bg_color
                            .unwrap_or_else(|| active_tab.bg_color.into())
                            .to_linear()
                            .into(),
                        text: fg_color
                            .unwrap_or_else(|| active_tab.fg_color.into())
                            .to_linear()
                            .into(),
                    }),
                TabBarItem::Tab { .. } => element
                    .vertical_align(tab_vertical_alignment)
                    .item_type(UIItemType::TabBar(item.item.clone()))
                    .margin(if is_bottom {
                        BoxDimension {
                            left: Dimension::Cells(0.),
                            right: Dimension::Cells(0.),
                            top: Dimension::Cells(0.),
                            bottom: Dimension::Cells(0.2),
                        }
                    } else {
                        BoxDimension {
                            left: Dimension::Cells(0.),
                            right: Dimension::Cells(0.),
                            top: Dimension::Cells(0.2),
                            bottom: Dimension::Cells(0.),
                        }
                    })
                    .padding(BoxDimension {
                        left: Dimension::Cells(0.5),
                        right: Dimension::Cells(0.5),
                        top: Dimension::Cells(0.25),
                        bottom: Dimension::Cells(0.25),
                    })
                    .border(BoxDimension::new(Dimension::Pixels(border_width)))
                    .border_corners(Some(if is_bottom {
                        Corners {
                            top_left: SizedPoly {
                                width: Dimension::Cells(0.),
                                height: Dimension::Cells(0.33),
                                poly: &[],
                            },
                            top_right: SizedPoly {
                                width: Dimension::Cells(0.),
                                height: Dimension::Cells(0.33),
                                poly: &[],
                            },
                            bottom_left: SizedPoly {
                                width: corner_radius,
                                height: corner_radius,
                                poly: BOTTOM_LEFT_ROUNDED_CORNER,
                            },
                            bottom_right: SizedPoly {
                                width: corner_radius,
                                height: corner_radius,
                                poly: BOTTOM_RIGHT_ROUNDED_CORNER,
                            },
                        }
                    } else {
                        Corners {
                            top_left: SizedPoly {
                                width: corner_radius,
                                height: corner_radius,
                                poly: TOP_LEFT_ROUNDED_CORNER,
                            },
                            top_right: SizedPoly {
                                width: corner_radius,
                                height: corner_radius,
                                poly: TOP_RIGHT_ROUNDED_CORNER,
                            },
                            bottom_left: SizedPoly {
                                width: Dimension::Cells(0.),
                                height: Dimension::Cells(0.33),
                                poly: &[],
                            },
                            bottom_right: SizedPoly {
                                width: Dimension::Cells(0.),
                                height: Dimension::Cells(0.33),
                                poly: &[],
                            },
                        }
                    }))
                    .colors({
                        let inactive_tab = colors.inactive_tab();
                        let bg = bg_color
                            .unwrap_or_else(|| inactive_tab.bg_color.into())
                            .to_linear();
                        let edge = colors.inactive_tab_edge().to_linear();
                        ElementColors {
                            border: BorderColor {
                                left: bg,
                                right: edge,
                                top: bg,
                                bottom: bg,
                            },
                            bg: bg.into(),
                            text: fg_color
                                .unwrap_or_else(|| inactive_tab.fg_color.into())
                                .to_linear()
                                .into(),
                        }
                    })
                    .hover_colors({
                        let inactive_tab_hover = colors.inactive_tab_hover();
                        let hover_bg = bg_color
                            .unwrap_or_else(|| inactive_tab_hover.bg_color.into())
                            .to_linear();
                        Some(ElementColors {
                            // Keep the inactive divider visible while the
                            // pointer is over the tab; upstream replaced all
                            // four borders with the hover background and the
                            // line disappeared. inactive_tab_edge_hover was
                            // previously defined but never read.
                            border: BorderColor {
                                left: hover_bg,
                                right: colors.inactive_tab_edge_hover().to_linear(),
                                top: hover_bg,
                                bottom: hover_bg,
                            },
                            bg: hover_bg.into(),
                            text: fg_color
                                .unwrap_or_else(|| inactive_tab_hover.fg_color.into())
                                .to_linear()
                                .into(),
                        })
                    }),
                TabBarItem::WindowButton(button) => window_button_element(
                    button,
                    self.window_state.contains(window::WindowState::MAXIMIZED),
                    &font,
                    &metrics,
                    &self.config,
                ),
            }
        };

        let num_tabs: f32 = items
            .iter()
            .map(|item| match item.item {
                TabBarItem::NewTabButton | TabBarItem::Tab { .. } => 1.,
                _ => 0.,
            })
            .sum();
        let tab_count = items
            .iter()
            .filter(|item| matches!(item.item, TabBarItem::Tab { .. }))
            .count() as f32;
        let new_tab_width = if self.config.show_new_tab_button_in_tab_bar {
            // Mirror NewTabButton's poly width, horizontal padding
            // (0.5 + 0.5 cells), 1px borders and left margin (0.5 cell).
            metrics.cell_size.height as f32 * 0.5 + metrics.cell_size.width as f32 * 1.5 + 2.
        } else {
            0.
        };
        let window_buttons_at_left = self
            .config
            .window_decorations
            .contains(window::WindowDecorations::INTEGRATED_BUTTONS)
            && (self.config.integrated_title_button_alignment
                == IntegratedTitleButtonAlignment::Left
                // macOS reserves space for its native traffic lights; the
                // fork-drawn ones follow the configured alignment instead.
                || (self.config.integrated_title_button_style
                    == IntegratedTitleButtonStyle::MacOsNative
                    && cfg!(target_os = "macos")));
        // The right status and the integrated window buttons float over the
        // tab strip, so the uniform tab share must leave room for them.
        let reserved_side_width: f32 = items
            .iter()
            .filter_map(|item| {
                let occupies_side = match item.item {
                    TabBarItem::LeftStatus => true,
                    TabBarItem::RightStatus => true,
                    TabBarItem::WindowButton(_) => true,
                    _ => false,
                };
                occupies_side.then(|| match item.item {
                    // Fork-drawn traffic lights are circles, not glyphs.
                    TabBarItem::WindowButton(_)
                        if self.config.integrated_title_button_style
                            == IntegratedTitleButtonStyle::MacOsNative =>
                    {
                        crate::termwindow::render::window_buttons::traffic_light_width(&metrics)
                    }
                    _ => item.title.len() as f32 * metrics.cell_size.width as f32,
                })
            })
            .sum::<f32>()
            // The status text and buttons render more compactly than a
            // character-cell estimate; reserving the full estimate leaves a
            // visible gap. The ratio is configurable so it can be tuned
            // without rebuilding the fork.
            * (self.config.tab_bar_side_reserve_ratio as f32).clamp(0.0, 1.0);
        let uniform_width =
            (self.dimensions.pixel_width as f32 - reserved_side_width - new_tab_width).max(0.);
        let max_tab_width = if self.config.tab_bar_uniform_width && tab_count > 0. {
            // Split the remaining bar between the tabs only. The new tab
            // button and the side status/buttons have fixed sizes, so they
            // must not consume one tab's share. Use the terminal's pixel width
            // rather than dimensions.pixel_width: the latter can be a
            // stale/window-frame value that is wider than the actual client
            // area and makes the first tab claim too much.
            (uniform_width / tab_count).max(0.)
        } else {
            ((self.dimensions.pixel_width as f32 / num_tabs)
                - (1.5 * metrics.cell_size.width as f32))
                .max(0.)
        };

        // Uniform mode consumes the remaining space as it walks the items, so
        // every tab gets the same share instead of each one claiming a global
        // estimate and squeezing the last tab.
        let mut remaining_tabs = tab_count;
        let mut remaining_width = uniform_width;

        // Reserve space for the native titlebar buttons
        if self
            .config
            .window_decorations
            .contains(::window::WindowDecorations::INTEGRATED_BUTTONS)
            && self.config.integrated_title_button_style == IntegratedTitleButtonStyle::MacOsNative
            && !self.window_state.contains(window::WindowState::FULL_SCREEN)
        {
            left_status.push(
                Element::new(&font, ElementContent::Text("".to_string())).margin(BoxDimension {
                    left: Dimension::Cells(4.0), // FIXME: determine exact width of macos ... buttons
                    right: Dimension::Cells(0.),
                    top: Dimension::Cells(0.),
                    bottom: Dimension::Cells(0.),
                }),
            );
        }

        for item in items {
            match item.item {
                TabBarItem::LeftStatus => left_status.push(item_to_elem(item)),
                TabBarItem::None | TabBarItem::RightStatus => right_eles.push(item_to_elem(item)),
                TabBarItem::WindowButton(_) => {
                    if self.config.integrated_title_button_alignment
                        == IntegratedTitleButtonAlignment::Left
                    {
                        left_eles.push(item_to_elem(item))
                    } else {
                        right_eles.push(item_to_elem(item))
                    }
                }
                TabBarItem::Tab { tab_idx, active } => {
                    let mut elem = item_to_elem(item);
                    if self.config.tab_bar_uniform_width && remaining_tabs > 0. {
                        let tab_width = (remaining_width / remaining_tabs).max(0.);
                        remaining_width = (remaining_width - tab_width).max(0.);
                        remaining_tabs -= 1.;
                        // min_width is a content width; subtract the tab's
                        // horizontal padding (1 cell) and borders (2 × width)
                        // so the rendered element matches tab_width exactly.
                        elem.min_width = Some(Dimension::Pixels(
                            (tab_width - metrics.cell_size.width as f32 - 2. * border_width)
                                .max(0.),
                        ));
                        elem.max_width = Some(Dimension::Pixels(tab_width));
                    } else {
                        elem.max_width = Some(Dimension::Pixels(max_tab_width));
                    }
                    let tab_colors = elem.colors.clone();
                    elem.content = match elem.content {
                        ElementContent::Text(_) => unreachable!(),
                        ElementContent::Poly { .. } => unreachable!(),
                        ElementContent::Children(mut kids) => {
                            if self.config.show_close_tab_button_in_tabs {
                                kids.push(make_x_button(&font, &metrics, &colors, tab_idx, active));
                            }
                            if let Some(right) = &item.right_title {
                                // Float the callback's right-hand suffix before
                                // the close button so it stays on the tab's
                                // right edge regardless of proportional title
                                // glyph widths.
                                kids.push(
                                    Element::with_line(&font, right, palette)
                                        .vertical_align(tab_vertical_alignment)
                                        .item_type(UIItemType::TabBar(item.item.clone()))
                                        .float(Float::Right)
                                        .zindex(1)
                                        .colors(tab_colors),
                                );
                            }
                            ElementContent::Children(kids)
                        }
                    };
                    left_eles.push(elem);
                }
                _ => left_eles.push(item_to_elem(item)),
            }
        }

        let mut children = vec![];

        if !left_status.is_empty() {
            children.push(
                Element::new(&font, ElementContent::Children(left_status))
                    .colors(bar_colors.clone()),
            );
        }

        let left_padding = if window_buttons_at_left {
            if self.config.integrated_title_button_style == IntegratedTitleButtonStyle::MacOsNative
            {
                if !self.window_state.contains(window::WindowState::FULL_SCREEN) {
                    Dimension::Pixels(70.0)
                } else {
                    Dimension::Cells(0.5)
                }
            } else {
                Dimension::Pixels(0.0)
            }
        } else {
            Dimension::Cells(0.5)
        };

        children.push(
            Element::new(&font, ElementContent::Children(left_eles))
                .vertical_align(tab_vertical_alignment)
                .colors(bar_colors.clone())
                .padding(BoxDimension {
                    left: left_padding,
                    right: Dimension::Cells(0.),
                    top: Dimension::Cells(0.),
                    bottom: Dimension::Cells(0.),
                })
                .zindex(1),
        );
        children.push(
            Element::new(&font, ElementContent::Children(right_eles))
                .colors(bar_colors.clone())
                .float(Float::Right),
        );

        let content = ElementContent::Children(children);

        let tabs = Element::new(&font, content)
            .display(DisplayType::Block)
            .item_type(UIItemType::TabBar(TabBarItem::None))
            .min_width(Some(Dimension::Pixels(self.dimensions.pixel_width as f32)))
            .min_height(Some(Dimension::Pixels(tab_bar_height)))
            .vertical_align(tab_vertical_alignment)
            .colors(bar_colors);

        let border = self.get_os_border();

        let mut computed = self.compute_element(
            &LayoutContext {
                height: DimensionContext {
                    dpi: self.dimensions.dpi as f32,
                    pixel_max: self.dimensions.pixel_height as f32,
                    pixel_cell: metrics.cell_size.height as f32,
                },
                width: DimensionContext {
                    dpi: self.dimensions.dpi as f32,
                    pixel_max: self.dimensions.pixel_width as f32,
                    pixel_cell: metrics.cell_size.width as f32,
                },
                bounds: euclid::rect(
                    border.left.get() as f32,
                    0.,
                    self.dimensions.pixel_width as f32 - (border.left + border.right).get() as f32,
                    tab_bar_height,
                ),
                metrics: &metrics,
                gl_state: self.render_state.as_ref().unwrap(),
                zindex: 10,
            },
            &tabs,
        )?;

        computed.translate(euclid::vec2(
            0.,
            if self.config.tab_bar_at_bottom {
                self.dimensions.pixel_height as f32
                    - (computed.bounds.height() + border.bottom.get() as f32)
            } else {
                border.top.get() as f32
            },
        ));

        Ok(computed)
    }

    pub fn paint_fancy_tab_bar(&self) -> anyhow::Result<Vec<UIItem>> {
        let computed = self.fancy_tab_bar.as_ref().ok_or_else(|| {
            anyhow::anyhow!("paint_fancy_tab_bar called but fancy_tab_bar is None")
        })?;
        let ui_items = computed.ui_items();

        let gl_state = self.render_state.as_ref().unwrap();
        self.render_element(&computed, gl_state, None)?;
        self.paint_tab_bar_decorations(computed, gl_state)?;

        Ok(ui_items)
    }

    /// Stroke the separator, the active-tab door and the waiting ring, glow
    /// and indicator bar on a layer above the tab bar. Nothing here changes
    /// the tab bar layout.
    fn paint_tab_bar_decorations(
        &self,
        computed: &ComputedElement,
        gl_state: &RenderState,
    ) -> anyhow::Result<()> {
        let colors = self
            .config
            .colors
            .as_ref()
            .and_then(|c| c.tab_bar.as_ref())
            .cloned()
            .unwrap_or_else(TabBarColors::default);
        let tab_info = |idx: usize| -> (bool, Option<LinearRgba>) {
            self.tab_bar
                .items()
                .iter()
                .find(|item| matches!(item.item, TabBarItem::Tab { tab_idx, .. } if tab_idx == idx))
                .map(|item| (item.agent_waiting, item.accent.map(|c| c.to_linear())))
                .unwrap_or((false, None))
        };
        let any_waiting = self.tab_bar.items().iter().any(|item| item.agent_waiting);
        let active_edge = colors.active_tab_edge().map(|c| c.to_linear());
        if active_edge.is_none() && !any_waiting {
            return Ok(());
        }

        let mut tabs = vec![];
        collect_tab_geoms(computed, &tab_info, &|e| self.tab_fill(e), &mut tabs);
        let font = self.fonts.title_font()?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let px = |d: Dimension| {
            d.evaluate_as_pixels(DimensionContext {
                dpi: self.dimensions.dpi as f32,
                pixel_max: self.dimensions.pixel_width as f32,
                pixel_cell: metrics.cell_size.width as f32,
            })
            .round()
        };
        let line = px(self.config.tab_bar_tab_border_width).max(1.);
        let edge_line = match px(self.config.tab_bar_active_edge_width) {
            w if w >= 1. => w,
            _ => line,
        };
        let scale = (self.dimensions.dpi as f32 / 96.).max(1.);
        let style = DecoStyle {
            active_edge,
            waiting_edge: colors.waiting_tab_edge().to_linear(),
            glow: colors.waiting_tab_glow().to_linear(),
            line,
            edge_line,
            ring: scale.round().max(1.),
            lamp: (3. * scale).round().max(2.),
            fillet: ((metrics.cell_size.height as f32 * TAB_DOOR_FILLET_CELLS).round())
                .max(edge_line),
            at_bottom: self.config.tab_bar_at_bottom,
        };

        let decos = tab_bar_decorations(computed.bounds, &tabs, &style);
        if decos.is_empty() {
            return Ok(());
        }
        let layer = gl_state.layer_for_zindex(DECORATION_ZINDEX)?;
        let mut layers = layer.quad_allocator();
        for deco in decos {
            match deco {
                Deco::Rect { rect, color } => {
                    self.filled_rectangle(&mut layers, 0, rect, color)?;
                }
                Deco::Arc {
                    corner,
                    rect,
                    line_width,
                    color,
                    inner: Some(inner),
                } => {
                    let size = |r: RectF| euclid::size2(r.width().round(), r.height().round());
                    self.poly_quad(
                        &mut layers,
                        0,
                        rect.origin,
                        corner.filled_poly(),
                        1,
                        size(rect),
                        color,
                    )?
                    .set_grayscale();
                    let inset = corner.inset(rect, line_width);
                    if inset.width() >= 1. && inset.height() >= 1. {
                        self.poly_quad(
                            &mut layers,
                            0,
                            inset.origin,
                            corner.filled_poly(),
                            1,
                            size(inset),
                            inner,
                        )?
                        .set_grayscale();
                    }
                }
                Deco::Arc {
                    corner,
                    rect,
                    line_width,
                    color,
                    inner: None,
                } => {
                    self.poly_quad(
                        &mut layers,
                        0,
                        rect.origin,
                        corner.poly(),
                        line_width.round().max(1.) as isize,
                        rect.size,
                        color,
                    )?
                    .set_grayscale();
                }
                Deco::Quarter {
                    corner,
                    rect,
                    color,
                } => {
                    self.poly_quad(
                        &mut layers,
                        0,
                        rect.origin,
                        corner.filled_poly(),
                        1,
                        euclid::size2(rect.width().round(), rect.height().round()),
                        color,
                    )?
                    .set_grayscale();
                }
            }
        }
        Ok(())
    }

    /// The fill a tab element is painted with this frame: its hover colors
    /// while the pointer is over it (same test as `render_element`), when
    /// that fill is a plain color.
    fn tab_fill(&self, element: &ComputedElement) -> Option<LinearRgba> {
        let hovering = element.hover_colors.is_some()
            && match &self.current_mouse_event {
                Some(event) => {
                    let x = event.coords.x as f32;
                    let y = event.coords.y as f32;
                    x >= element.bounds.min_x()
                        && x <= element.bounds.max_x()
                        && y >= element.bounds.min_y()
                        && y <= element.bounds.max_y()
                }
                None => false,
            }
            && matches!(
                self.current_mouse_capture,
                None | Some(crate::termwindow::MouseCapture::UI)
            );
        let colors = match (&element.hover_colors, hovering) {
            (Some(hover), true) => hover,
            _ => &element.colors,
        };
        match colors.bg {
            InheritableColor::Color(color) => Some(color),
            _ => None,
        }
    }
}

fn make_x_button(
    font: &Rc<LoadedFont>,
    metrics: &RenderMetrics,
    colors: &TabBarColors,
    tab_idx: usize,
    active: bool,
) -> Element {
    Element::new(
        &font,
        ElementContent::Poly {
            line_width: metrics.underline_height.max(2),
            poly: SizedPoly {
                poly: X_BUTTON,
                width: Dimension::Pixels(metrics.cell_size.height as f32 / 2.),
                height: Dimension::Pixels(metrics.cell_size.height as f32 / 2.),
            },
        },
    )
    // Ensure that we draw our background over the
    // top of the rest of the tab contents
    .zindex(1)
    .vertical_align(VerticalAlign::Middle)
    .float(Float::Right)
    .item_type(UIItemType::CloseTab(tab_idx))
    .hover_colors({
        let inactive_tab_hover = colors.inactive_tab_hover();
        let active_tab = colors.active_tab();

        Some(ElementColors {
            border: BorderColor::default(),
            bg: (if active {
                inactive_tab_hover.bg_color
            } else {
                active_tab.bg_color
            })
            .to_linear()
            .into(),
            text: (if active {
                inactive_tab_hover.fg_color
            } else {
                active_tab.fg_color
            })
            .to_linear()
            .into(),
        })
    })
    .padding(BoxDimension {
        left: Dimension::Cells(0.25),
        right: Dimension::Cells(0.25),
        top: Dimension::Cells(0.25),
        bottom: Dimension::Cells(0.25),
    })
    .margin(BoxDimension {
        left: Dimension::Cells(0.5),
        right: Dimension::Cells(0.),
        top: Dimension::Cells(0.),
        bottom: Dimension::Cells(0.),
    })
}

#[cfg(test)]
mod decoration_tests {
    use super::*;

    const BLUE: LinearRgba = LinearRgba(0., 0., 1., 1.);
    const ORANGE: LinearRgba = LinearRgba(1., 0.5, 0., 1.);
    const GLOW: LinearRgba = LinearRgba(1., 0.6, 0., 1.);
    const FILL: LinearRgba = LinearRgba(0.5, 0.1, 0.1, 1.);

    fn style(at_bottom: bool) -> DecoStyle {
        DecoStyle {
            active_edge: Some(BLUE),
            waiting_edge: ORANGE,
            glow: GLOW,
            line: 1.,
            edge_line: 1.,
            ring: 1.,
            lamp: 3.,
            fillet: 1.,
            at_bottom,
        }
    }

    fn tab(x: f32, active: bool, waiting: bool) -> TabGeom {
        // The bar is 30px high; tabs sit on its bottom edge with 4px above.
        TabGeom {
            active,
            waiting,
            rect: euclid::rect(x, 4., 100., 26.),
            corners: CornerSizes {
                top_left: (5., 10.),
                top_right: (5., 10.),
                ..CornerSizes::default()
            },
            fill: Some(FILL),
            accent: None,
        }
    }

    fn bar() -> RectF {
        euclid::rect(0., 0., 400., 30.)
    }

    fn rects(decos: &[Deco], color: LinearRgba) -> Vec<RectF> {
        decos
            .iter()
            .filter_map(|d| match d {
                Deco::Rect { rect, color: c } if *c == color => Some(*rect),
                _ => None,
            })
            .collect()
    }

    fn arcs(decos: &[Deco], color: LinearRgba) -> Vec<DecoCorner> {
        decos
            .iter()
            .filter_map(|d| match d {
                Deco::Arc {
                    corner, color: c, ..
                } if *c == color => Some(*corner),
                _ => None,
            })
            .collect()
    }

    fn quarters(decos: &[Deco], color: LinearRgba) -> Vec<(DecoCorner, RectF)> {
        decos
            .iter()
            .filter_map(|d| match d {
                Deco::Quarter {
                    corner,
                    rect,
                    color: c,
                } if *c == color => Some((*corner, *rect)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn nothing_without_active_edge_or_waiting() {
        let mut s = style(false);
        s.active_edge = None;
        let tabs = [tab(10., true, false), tab(110., false, false)];
        assert!(tab_bar_decorations(bar(), &tabs, &s).is_empty());
    }

    #[test]
    fn separator_folds_around_the_active_tab() {
        let tabs = [tab(10., false, false), tab(110., true, false)];
        let decos = tab_bar_decorations(bar(), &tabs, &style(false));
        let blue = rects(&decos, BLUE);
        // Separator up to the door's left post, and from its right post on.
        assert!(blue.contains(&euclid::rect(0., 29., 111., 1.)));
        assert!(blue.contains(&euclid::rect(209., 29., 191., 1.)));
        // Door: posts below the rounded corners, top between them, stopping
        // one fillet short of the separator.
        assert!(blue.contains(&euclid::rect(110., 14., 1., 15.)));
        assert!(blue.contains(&euclid::rect(209., 14., 1., 15.)));
        assert!(blue.contains(&euclid::rect(115., 4., 90., 1.)));
        // Fillet quarters join the posts to the separator.
        assert_eq!(
            quarters(&decos, BLUE),
            vec![
                (DecoCorner::TopRight, euclid::rect(110., 29., 1., 1.)),
                (DecoCorner::TopLeft, euclid::rect(209., 29., 1., 1.)),
            ]
        );
        // Open towards the content: nothing between the posts on the bottom row.
        assert!(!blue
            .iter()
            .any(|r| r.min_x() >= 111. && r.max_x() <= 209. && r.min_y() >= 29.));
        assert_eq!(
            arcs(&decos, BLUE),
            vec![DecoCorner::TopLeft, DecoCorner::TopRight]
        );
    }

    #[test]
    fn waiting_inactive_tab_gets_ring_lamp_and_glow() {
        let tabs = [tab(10., true, false), tab(110., false, true)];
        let decos = tab_bar_decorations(bar(), &tabs, &style(false));
        let orange = rects(&decos, ORANGE);
        // Closed ring: posts, top and bottom.
        assert!(orange.contains(&euclid::rect(110., 14., 1., 16.)));
        assert!(orange.contains(&euclid::rect(209., 14., 1., 16.)));
        assert!(orange.contains(&euclid::rect(115., 4., 90., 1.)));
        assert!(orange.contains(&euclid::rect(110., 29., 100., 1.)));
        // Indicator bar along the far (top) edge between the corners.
        assert!(rects(&decos, GLOW).contains(&euclid::rect(115., 4., 90., 3.)));
        // Three glow rings fit in the 4px above the tab, fading outwards.
        for (i, alpha) in GLOW_ALPHA.iter().enumerate() {
            let off = (i + 1) as f32;
            let ring = rects(&decos, GLOW.mul_alpha(*alpha));
            assert!(
                ring.contains(&euclid::rect(115., 4. - off, 90., 1.)),
                "ring {} top",
                i
            );
            assert!(
                ring.contains(&euclid::rect(110. - off, 14., 1., 16.)),
                "ring {} left",
                i
            );
            assert!(!ring.iter().any(|r| r.max_y() > 30.), "ring {} open", i);
        }
        // The separator is painted first, so the ring wins on the bottom row.
        let sep = decos
            .iter()
            .position(|d| matches!(d, Deco::Rect { color, .. } if *color == BLUE))
            .unwrap();
        let ring_bottom = decos
            .iter()
            .position(|d| {
                matches!(d, Deco::Rect { rect, color } if *color == ORANGE && rect.min_y() == 29.)
            })
            .unwrap();
        assert!(sep < ring_bottom);
    }

    #[test]
    fn waiting_active_tab_uses_the_waiting_door() {
        let tabs = [tab(10., true, true)];
        let decos = tab_bar_decorations(bar(), &tabs, &style(false));
        let orange = rects(&decos, ORANGE);
        assert!(orange.contains(&euclid::rect(15., 4., 90., 1.)));
        assert!(!orange.iter().any(|r| r.min_y() >= 29.));
        assert_eq!(
            arcs(&decos, ORANGE),
            vec![DecoCorner::TopLeft, DecoCorner::TopRight]
        );
    }

    #[test]
    fn waiting_without_active_edge_keeps_a_closed_ring() {
        let mut s = style(false);
        s.active_edge = None;
        let decos = tab_bar_decorations(bar(), &[tab(10., true, true)], &s);
        assert!(rects(&decos, BLUE).is_empty());
        assert!(rects(&decos, ORANGE).contains(&euclid::rect(10., 29., 100., 1.)));
    }

    #[test]
    fn glow_rings_are_limited_by_the_room_above() {
        let mut t = tab(10., false, true);
        t.rect = euclid::rect(10., 2., 100., 28.);
        let decos = tab_bar_decorations(bar(), &[t], &style(false));
        assert!(!rects(&decos, GLOW.mul_alpha(GLOW_ALPHA[0])).is_empty());
        assert!(!rects(&decos, GLOW.mul_alpha(GLOW_ALPHA[1])).is_empty());
        assert!(rects(&decos, GLOW.mul_alpha(GLOW_ALPHA[2])).is_empty());
    }

    #[test]
    fn bottom_tab_bar_is_mirrored() {
        let tabs = [TabGeom {
            active: true,
            waiting: false,
            rect: euclid::rect(10., 0., 100., 26.),
            corners: CornerSizes {
                bottom_left: (5., 10.),
                bottom_right: (5., 10.),
                ..CornerSizes::default()
            },
            fill: Some(FILL),
            accent: None,
        }];
        let decos = tab_bar_decorations(bar(), &tabs, &style(true));
        let blue = rects(&decos, BLUE);
        assert!(blue.contains(&euclid::rect(0., 0., 11., 1.)));
        assert!(blue.contains(&euclid::rect(15., 25., 90., 1.)));
        // Posts start one fillet below the separator, mirrors of the top bar.
        assert!(blue.contains(&euclid::rect(10., 1., 1., 15.)));
        assert_eq!(
            quarters(&decos, BLUE),
            vec![
                (DecoCorner::BottomRight, euclid::rect(10., 0., 1., 1.)),
                (DecoCorner::BottomLeft, euclid::rect(109., 0., 1., 1.)),
            ]
        );
        assert_eq!(
            arcs(&decos, BLUE),
            vec![DecoCorner::BottomLeft, DecoCorner::BottomRight]
        );
    }

    #[test]
    fn door_posts_reach_a_separator_below_the_tab() {
        let mut t = tab(10., true, false);
        t.rect = euclid::rect(10., 4., 100., 24.);
        let decos = tab_bar_decorations(bar(), &[t], &style(false));
        let blue = rects(&decos, BLUE);
        assert!(blue.contains(&euclid::rect(10., 28., 1., 1.)));
        assert!(blue.contains(&euclid::rect(109., 28., 1., 1.)));
        assert_eq!(
            quarters(&decos, BLUE),
            vec![
                (DecoCorner::TopRight, euclid::rect(10., 29., 1., 1.)),
                (DecoCorner::TopLeft, euclid::rect(109., 29., 1., 1.)),
            ]
        );
    }

    #[test]
    fn door_meets_the_separator_with_a_flare() {
        let mut s = style(false);
        s.edge_line = 2.;
        s.fillet = 6.;
        let decos = tab_bar_decorations(bar(), &[tab(10., true, false)], &s);
        let blue = rects(&decos, BLUE);
        // The separator stops 6px short of each door post.
        assert!(blue.contains(&euclid::rect(0., 28., 16., 2.)));
        assert!(blue.contains(&euclid::rect(104., 28., 296., 2.)));
        // The post ends 6px above the bar edge.
        assert!(blue.contains(&euclid::rect(10., 14., 2., 10.)));
        assert_eq!(
            quarters(&decos, BLUE),
            vec![
                (DecoCorner::TopRight, euclid::rect(10., 24., 6., 6.)),
                (DecoCorner::TopLeft, euclid::rect(104., 24., 6., 6.)),
            ]
        );
        // The flare never eats into the bottom row between the posts.
        assert!(!blue
            .iter()
            .any(|r| r.min_x() >= 16. && r.max_x() <= 104. && r.min_y() >= 28.));
    }

    #[test]
    fn outline_arcs_are_filled_against_the_tab_fill_and_glow_arcs_are_stroked() {
        let tabs = [tab(10., true, false), tab(110., false, true)];
        let decos = tab_bar_decorations(bar(), &tabs, &style(false));
        for deco in &decos {
            if let Deco::Arc {
                color, inner, rect, ..
            } = deco
            {
                if *color == BLUE || *color == ORANGE {
                    assert_eq!(*inner, Some(FILL), "outline arc {:?}", rect);
                } else {
                    assert_eq!(*inner, None, "glow arc {:?}", rect);
                }
            }
        }
        // Without a known fill the outline falls back to a stroked arc.
        let mut t = tab(10., true, false);
        t.fill = None;
        let decos = tab_bar_decorations(bar(), &[t], &style(false));
        assert!(decos
            .iter()
            .all(|d| !matches!(d, Deco::Arc { inner: Some(_), .. })));
    }

    #[test]
    fn active_tab_accent_colors_separator_and_door() {
        const ACCENT: LinearRgba = LinearRgba(0.6, 0.15, 0.15, 1.);
        let mut active = tab(110., true, false);
        active.accent = Some(ACCENT);
        let mut other = tab(10., false, false);
        // An inactive tab's accent is ignored.
        other.accent = Some(GLOW);
        let decos = tab_bar_decorations(bar(), &[other, active], &style(false));
        let accent = rects(&decos, ACCENT);
        assert!(accent.contains(&euclid::rect(0., 29., 111., 1.)));
        assert!(accent.contains(&euclid::rect(209., 29., 191., 1.)));
        assert!(accent.contains(&euclid::rect(115., 4., 90., 1.)));
        assert_eq!(
            arcs(&decos, ACCENT),
            vec![DecoCorner::TopLeft, DecoCorner::TopRight]
        );
        assert!(rects(&decos, BLUE).is_empty());

        // A waiting active tab keeps the orange door; the separator keeps
        // the accent.
        active.waiting = true;
        let decos = tab_bar_decorations(bar(), &[active], &style(false));
        assert!(rects(&decos, ACCENT).contains(&euclid::rect(0., 29., 111., 1.)));
        assert!(rects(&decos, ORANGE).contains(&euclid::rect(115., 4., 90., 1.)));
    }

    #[test]
    fn separator_and_door_use_their_own_width() {
        let mut s = style(false);
        s.edge_line = 3.;
        let tabs = [
            tab(10., false, false),
            tab(110., true, false),
            tab(210., false, true),
        ];
        let decos = tab_bar_decorations(bar(), &tabs, &s);
        let blue = rects(&decos, BLUE);
        // Separator 3px high, joining the 3px door posts.
        assert!(blue.contains(&euclid::rect(0., 27., 113., 3.)));
        assert!(blue.contains(&euclid::rect(207., 27., 193., 3.)));
        assert!(blue.contains(&euclid::rect(110., 14., 3., 13.)));
        assert!(blue.contains(&euclid::rect(115., 4., 90., 3.)));
        // The waiting ring keeps the tab border width.
        let orange = rects(&decos, ORANGE);
        assert!(orange.contains(&euclid::rect(210., 14., 1., 16.)));
        assert!(orange.contains(&euclid::rect(210., 29., 100., 1.)));
    }

    #[test]
    fn inset_disc_shares_the_outer_disc_center() {
        let r = euclid::rect(10., 20., 8., 8.);
        assert_eq!(
            DecoCorner::TopLeft.inset(r, 1.),
            euclid::rect(11., 21., 7., 7.)
        );
        assert_eq!(
            DecoCorner::TopRight.inset(r, 1.),
            euclid::rect(10., 21., 7., 7.)
        );
        assert_eq!(
            DecoCorner::BottomLeft.inset(r, 1.),
            euclid::rect(11., 20., 7., 7.)
        );
        assert_eq!(
            DecoCorner::BottomRight.inset(r, 1.),
            euclid::rect(10., 20., 7., 7.)
        );
    }
}
