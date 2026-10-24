//! Pure window geometry (shell.md §6.3.4, §6.4, §6.5.3; contracts.md §4.2): widget placement and anchored resizing,
//! chip placement beside the predicted context menu, size/hit sanitizing. All values are DIP of the target monitor;
//! the window glue (vox-app) converts to physical pixels with that monitor's scale factor.

use crate::constants::*;
use crate::js::js_round;
use crate::settings::WidgetPosition;
use crate::app::ChipMode;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self { x, y, width, height }
    }
    pub fn right(&self) -> f64 {
        self.x + self.width
    }
    pub fn bottom(&self) -> f64 {
        self.y + self.height
    }
    pub fn size(&self) -> Size {
        Size { width: self.width, height: self.height }
    }
    /// Half-open containment `[x, x+w) × [y, y+h)`.
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }
    /// Overlap test where touching edges do not count.
    pub fn intersects(&self, o: &Rect) -> bool {
        self.x < o.right() && o.x < self.right() && self.y < o.bottom() && o.y < self.bottom()
    }
}

/// Horizontal anchor (edge kept fixed when the widget resizes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HAnchor {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VAnchor {
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    pub h: HAnchor,
    pub v: VAnchor,
}

impl Default for Anchor {
    /// The initial widget anchor: right, bottom.
    fn default() -> Self {
        Self { h: HAnchor::Right, v: VAnchor::Bottom }
    }
}

/// Moves `r` inside `area` without resizing (an oversized rect pins to the area's top-left), then rounds every field
/// with `Math.round`.
pub fn clamp_to_area(r: Rect, area: Rect) -> Rect {
    let x = area.x.max(r.x.min(area.x + area.width - r.width));
    let y = area.y.max(r.y.min(area.y + area.height - r.height));
    Rect::new(js_round(x), js_round(y), js_round(r.width), js_round(r.height))
}

/// Quadrant anchor: centre strictly right of the area's mid-x → right; strictly below mid-y → bottom.
pub fn anchor_for(r: Rect, area: Rect) -> Anchor {
    let h = if r.x + r.width / 2.0 > area.x + area.width / 2.0 { HAnchor::Right } else { HAnchor::Left };
    let v = if r.y + r.height / 2.0 > area.y + area.height / 2.0 { VAnchor::Bottom } else { VAnchor::Top };
    Anchor { h, v }
}

/// New bounds for a size change keeping the anchored edges fixed, clamped into `area`.
pub fn resize_anchored(old: Rect, size: Size, anchor: Anchor, area: Rect) -> Rect {
    let x = match anchor.h {
        HAnchor::Left => old.x,
        HAnchor::Right => old.x + old.width - size.width,
        HAnchor::Center => old.x + (old.width - size.width) / 2.0,
    };
    let y = match anchor.v {
        VAnchor::Top => old.y,
        VAnchor::Bottom => old.y + old.height - size.height,
    };
    clamp_to_area(Rect::new(x, y, size.width, size.height), area)
}

/// Inputs for [`place_widget`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WidgetPlacementInput {
    pub position: WidgetPosition,
    pub size: Size,
    /// Work area (of the display nearest the cursor; for `remember` with coordinates: the display matching the saved
    /// rect).
    pub area: Rect,
    pub cursor: Point,
    pub custom: Option<Point>,
}

/// Widget bounds and anchor per `widget.position` (shell.md §6.3.4).
pub fn place_widget(i: WidgetPlacementInput) -> (Rect, Anchor) {
    let (w, h, a, m) = (i.size.width, i.size.height, i.area, WIDGET_MARGIN);
    let bottom_right = || (Rect::new(a.right() - w - m, a.bottom() - h - m, w, h), Anchor { h: HAnchor::Right, v: VAnchor::Bottom });
    let (r, anchor) = match i.position {
        WidgetPosition::BottomRight => bottom_right(),
        WidgetPosition::TopRight => (Rect::new(a.right() - w - m, a.y + m, w, h), Anchor { h: HAnchor::Right, v: VAnchor::Top }),
        WidgetPosition::BottomCenter => {
            (Rect::new(a.x + (a.width - w) / 2.0, a.bottom() - h - m, w, h), Anchor { h: HAnchor::Center, v: VAnchor::Bottom })
        }
        WidgetPosition::NearCursor => {
            let mut x = i.cursor.x + NEAR_CURSOR_DX;
            let mut y = i.cursor.y + NEAR_CURSOR_DY;
            let mut anchor = Anchor { h: HAnchor::Left, v: VAnchor::Top };
            if x + w > a.right() - m {
                x = i.cursor.x - w - NEAR_CURSOR_FLIP;
                anchor.h = HAnchor::Right;
            }
            if y + h > a.bottom() - m {
                y = i.cursor.y - h - NEAR_CURSOR_FLIP;
                anchor.v = VAnchor::Bottom;
            }
            (Rect::new(x, y, w, h), anchor)
        }
        WidgetPosition::Remember => match i.custom {
            Some(p) => {
                let r = clamp_to_area(Rect::new(p.x, p.y, w, h), a);
                (r, anchor_for(r, a))
            }
            None => bottom_right(),
        },
    };
    (clamp_to_area(r, a), anchor)
}

/// Which way a native context menu opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuH {
    Right,
    Left,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuV {
    Down,
    Up,
    Unknown,
}

/// Predicts the menu's flip from the room around the cursor.
pub fn predict_menu(cursor: Point, area: Rect, small: Size, large: Size) -> (MenuH, MenuV) {
    let room_right = a_right(area) - cursor.x;
    let room_below = area.y + area.height - cursor.y;
    let h = if room_right >= large.width {
        MenuH::Right
    } else if room_right < small.width {
        MenuH::Left
    } else {
        MenuH::Unknown
    };
    let v = if room_below >= large.height {
        MenuV::Down
    } else if room_below < small.height {
        MenuV::Up
    } else {
        MenuV::Unknown
    };
    (h, v)
}

fn a_right(a: Rect) -> f64 {
    a.x + a.width
}

/// Every rect the menu may occupy.
pub fn menu_rects(cursor: Point, pred: (MenuH, MenuV), menu: Size) -> Vec<Rect> {
    let hs: Vec<MenuH> = if pred.0 == MenuH::Unknown { vec![MenuH::Right, MenuH::Left] } else { vec![pred.0] };
    let vs: Vec<MenuV> = if pred.1 == MenuV::Unknown { vec![MenuV::Down, MenuV::Up] } else { vec![pred.1] };
    let mut out = Vec::new();
    for h in &hs {
        for v in &vs {
            let x = if *h == MenuH::Right { cursor.x } else { cursor.x - menu.width };
            let y = if *v == MenuV::Down { cursor.y } else { cursor.y - menu.height };
            out.push(Rect::new(x, y, menu.width, menu.height));
        }
    }
    out
}

/// Candidate chip positions, best first.
pub fn context_candidates(cursor: Point, size: Size, h: MenuH, menu: Size) -> Vec<Rect> {
    let (w, hh) = (size.width, size.height);
    let cy = cursor.y - hh / 2.0;
    let xv = match h {
        MenuH::Left => cursor.x - w,
        MenuH::Right => cursor.x,
        MenuH::Unknown => cursor.x - w / 2.0,
    };
    vec![
        Rect::new(cursor.x - CHIP_GAP - w, cy, w, hh),
        Rect::new(cursor.x + CHIP_GAP, cy, w, hh),
        Rect::new(xv, cursor.y - CHIP_GAP_V - hh, w, hh),
        Rect::new(xv, cursor.y + CHIP_GAP_V, w, hh),
        Rect::new(cursor.x - menu.width - CHIP_GAP - w, cy, w, hh),
        Rect::new(cursor.x + menu.width + CHIP_GAP, cy, w, hh),
    ]
}

fn first_clear(candidates: &[Rect], area: Rect, menus: &[Rect]) -> Option<Rect> {
    candidates.iter().map(|c| clamp_to_area(*c, area)).find(|r| !menus.iter().any(|m| r.intersects(m)))
}

fn size_of(t: (f64, f64)) -> Size {
    Size { width: t.0, height: t.1 }
}

/// Chip bounds: context mode beside the predicted menu (never on it), selection mode above-right of the cursor.
pub fn place_chip(mode: ChipMode, cursor: Point, size: Size, area: Rect) -> Rect {
    match mode {
        ChipMode::Context => {
            let (small, guess, large) = (size_of(CONTEXT_MENU_SMALL), size_of(CONTEXT_MENU_GUESS), size_of(CONTEXT_MENU_LARGE));
            let p = predict_menu(cursor, area, small, large);
            if let Some(r) = first_clear(&context_candidates(cursor, size, p.0, large), area, &menu_rects(cursor, p, large)) {
                return r;
            }
            let g = predict_menu(cursor, area, guess, guess);
            if let Some(r) = first_clear(&context_candidates(cursor, size, g.0, guess), area, &menu_rects(cursor, g, guess)) {
                return r;
            }
            clamp_to_area(Rect::new(cursor.x - CHIP_GAP - size.width, cursor.y - size.height / 2.0, size.width, size.height), area)
        }
        ChipMode::Selection => {
            let x = cursor.x + SELECTION_CHIP_GAP;
            let mut y = cursor.y - SELECTION_CHIP_GAP - size.height;
            if y < area.y {
                y = cursor.y + SELECTION_CHIP_GAP;
            }
            clamp_to_area(Rect::new(x, y, size.width, size.height), area)
        }
    }
}

/// `sanitizeSize`: finite width/height clamped to `[min, max]` then rounded; `None` when not finite.
pub fn sanitize_size(width: f64, height: f64, min: (f64, f64), max: (f64, f64)) -> Option<Size> {
    if !width.is_finite() || !height.is_finite() {
        return None;
    }
    Some(Size { width: js_round(width.clamp(min.0, max.0)), height: js_round(height.clamp(min.1, max.1)) })
}

/// `sanitizeHit`: the card rectangle inside the window, floored/ceiled and clipped; smaller than 8×8 → `None`
/// (the whole window counts).
pub fn sanitize_hit(hit: Rect, size: Size) -> Option<Rect> {
    if ![hit.x, hit.y, hit.width, hit.height].iter().all(|v| v.is_finite()) {
        return None;
    }
    let left = hit.x.floor().max(0.0);
    let top = hit.y.floor().max(0.0);
    let right = size.width.min((hit.x + hit.width).ceil());
    let bottom = size.height.min((hit.y + hit.height).ceil());
    if right - left < HIT_RECT_MIN || bottom - top < HIT_RECT_MIN {
        return None;
    }
    Some(Rect::new(left, top, right - left, bottom - top))
}

/// `pointOnCard`: cursor within the card (window-relative `hit`, or the whole window).
pub fn point_on_card(p: Point, window: Rect, hit: Option<Rect>) -> bool {
    let card = hit.unwrap_or(Rect::new(0.0, 0.0, window.width, window.height));
    let x = p.x - window.x - card.x;
    let y = p.y - window.y - card.y;
    x >= 0.0 && x < card.width && y >= 0.0 && y < card.height
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect { x: 0.0, y: 0.0, width: 1920.0, height: 1040.0 };
    const W: Size = Size { width: 380.0, height: 168.0 };

    fn place(position: WidgetPosition, cursor: Point, custom: Option<Point>) -> (Rect, Anchor) {
        place_widget(WidgetPlacementInput { position, size: W, area: AREA, cursor, custom })
    }

    #[test]
    fn widget_positions() {
        let c = Point { x: 500.0, y: 500.0 };
        assert_eq!(place(WidgetPosition::BottomRight, c, None).0, Rect::new(1516.0, 848.0, 380.0, 168.0));
        assert_eq!(place(WidgetPosition::TopRight, c, None).0, Rect::new(1516.0, 24.0, 380.0, 168.0));
        assert_eq!(place(WidgetPosition::BottomCenter, c, None).0, Rect::new(770.0, 848.0, 380.0, 168.0));
        assert_eq!(place(WidgetPosition::NearCursor, c, None), (Rect::new(516.0, 520.0, 380.0, 168.0), Anchor { h: HAnchor::Left, v: VAnchor::Top }));
        let edge = Point { x: 1800.0, y: 1000.0 };
        assert_eq!(place(WidgetPosition::NearCursor, edge, None), (Rect::new(1404.0, 816.0, 380.0, 168.0), Anchor { h: HAnchor::Right, v: VAnchor::Bottom }));
        // remember: clamped back inside, anchor by quadrant
        let (r, a) = place(WidgetPosition::Remember, c, Some(Point { x: 1800.0, y: -50.0 }));
        assert_eq!(r, Rect::new(1540.0, 0.0, 380.0, 168.0));
        assert_eq!(a, Anchor { h: HAnchor::Right, v: VAnchor::Top });
        assert_eq!(place(WidgetPosition::Remember, c, None).0, place(WidgetPosition::BottomRight, c, None).0);
    }

    #[test]
    fn resize_and_clamp() {
        let old = Rect::new(1516.0, 848.0, 380.0, 168.0);
        let r = resize_anchored(old, Size { width: 380.0, height: 200.0 }, Anchor::default(), AREA);
        assert_eq!(r, Rect::new(1516.0, 816.0, 380.0, 200.0));
        assert_eq!(clamp_to_area(Rect::new(10.0, 10.0, 3000.0, 50.0), AREA).x, 0.0);
        assert_eq!(clamp_to_area(Rect::new(-100.5, 10.0, 10.0, 10.0), Rect::new(-500.0, 0.0, 600.0, 600.0)).x, -100.0);
    }

    #[test]
    fn chip_placement() {
        let size = Size { width: 153.0, height: 38.0 };
        // plenty of room: menu opens right/down, chip left of the cursor
        let r = place_chip(ChipMode::Context, Point { x: 800.0, y: 400.0 }, size, AREA);
        assert_eq!(r, Rect::new(629.0, 381.0, 153.0, 38.0));
        // at the right edge the menu flips left over [1440, 1800); left/right of the cursor both overlap it (the right
        // spot is clamped back to 1767), so the chip goes above, hanging off the menu's own edge
        let r = place_chip(ChipMode::Context, Point { x: 1800.0, y: 400.0 }, size, AREA);
        assert_eq!(r, Rect::new(1647.0, 342.0, 153.0, 38.0));
        let s = place_chip(ChipMode::Selection, Point { x: 100.0, y: 10.0 }, Size { width: 82.0, height: 38.0 }, AREA);
        assert_eq!(s, Rect::new(120.0, 30.0, 82.0, 38.0));
        let s = place_chip(ChipMode::Selection, Point { x: 100.0, y: 500.0 }, Size { width: 82.0, height: 38.0 }, AREA);
        assert_eq!(s, Rect::new(120.0, 442.0, 82.0, 38.0));
        // Small work area (shell.md §6.5.3): pass 1 finds nothing clear of the large-menu range; pass 2 with the
        // typical size does (expected values from the spec pseudo-code).
        let small = Rect::new(0.0, 0.0, 600.0, 500.0);
        assert_eq!(place_chip(ChipMode::Context, Point { x: 90.0, y: 0.0 }, size, small), Rect::new(428.0, 0.0, 153.0, 38.0));
        // … and when pass 2 fails too, the classic spot clamped into the area (pass 3).
        assert_eq!(place_chip(ChipMode::Context, Point { x: 130.0, y: 0.0 }, size, small), Rect::new(0.0, 0.0, 153.0, 38.0));
    }

    #[test]
    fn anchors_and_centered_resize() {
        // Quadrants: strictly right of / below the area's centre.
        assert_eq!(anchor_for(Rect::new(900.0, 500.0, 200.0, 100.0), AREA), Anchor { h: HAnchor::Right, v: VAnchor::Bottom });
        assert_eq!(anchor_for(Rect::new(860.0, 470.0, 200.0, 100.0), AREA), Anchor { h: HAnchor::Left, v: VAnchor::Top });
        assert_eq!(anchor_for(Rect::new(0.0, 900.0, 200.0, 100.0), AREA), Anchor { h: HAnchor::Left, v: VAnchor::Bottom });
        // A bottom-centre widget keeps its centre and bottom edge.
        let old = Rect::new(770.0, 848.0, 380.0, 168.0);
        let r = resize_anchored(old, Size { width: 400.0, height: 200.0 }, Anchor { h: HAnchor::Center, v: VAnchor::Bottom }, AREA);
        assert_eq!(r, Rect::new(760.0, 816.0, 400.0, 200.0));
    }

    #[test]
    fn sizes_and_hits() {
        assert_eq!(sanitize_size(10.0, 2000.4, WIDGET_SIZE_MIN, WIDGET_SIZE_MAX), Some(Size { width: 40.0, height: 900.0 }));
        assert_eq!(sanitize_size(f64::NAN, 1.0, WIDGET_SIZE_MIN, WIDGET_SIZE_MAX), None);
        let s = Size { width: 380.0, height: 168.0 };
        assert_eq!(sanitize_hit(Rect::new(-2.5, 0.4, 400.0, 100.2), s), Some(Rect::new(0.0, 0.0, 380.0, 101.0)));
        assert_eq!(sanitize_hit(Rect::new(0.0, 0.0, 7.0, 100.0), s), None);
        let win = Rect::new(100.0, 100.0, 380.0, 168.0);
        assert!(point_on_card(Point { x: 100.0, y: 100.0 }, win, None));
        assert!(!point_on_card(Point { x: 480.0, y: 100.0 }, win, None));
    }
}
