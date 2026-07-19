//! A self-drawn, zoomable/pannable Maidenhead grid map -- no real-world
//! coastline imagery (nothing reliable to source/embed here), just a
//! clean equirectangular field/square grid with letter labels, the same
//! approach plenty of standalone ham-radio grid-square pickers use. One
//! widget, two uses: the Settings popover picks a home locator with it
//! (`MapMode::Pick`), and the standalone contact-map window displays a
//! house icon + lines out to everyone heard from with it (`MapMode::View`).

use crate::maidenhead;
use std::sync::OnceLock;
use std::time::SystemTime;

/// Natural Earth 1:110m land outlines (public domain, no attribution
/// required -- see https://www.naturalearthdata.com/), pre-converted by
/// `src/bin/convert_land_geojson.rs` into a flat binary so the shipped app
/// needs no JSON parser at runtime. Format: `u32` ring count, then per
/// ring a `u32` point count followed by that many little-endian
/// `(f32 lat, f32 lon)` pairs.
static LAND_OUTLINES_BIN: &[u8] = include_bytes!("../assets/land_data/land_outlines.bin");

fn land_outlines() -> &'static Vec<Vec<(f32, f32)>> {
    static RINGS: OnceLock<Vec<Vec<(f32, f32)>>> = OnceLock::new();
    RINGS.get_or_init(|| {
        let mut pos = 0usize;
        let read_u32 = |buf: &[u8], pos: &mut usize| -> u32 {
            let v = u32::from_le_bytes(buf[*pos..*pos + 4].try_into().unwrap());
            *pos += 4;
            v
        };
        let read_f32 = |buf: &[u8], pos: &mut usize| -> f32 {
            let v = f32::from_le_bytes(buf[*pos..*pos + 4].try_into().unwrap());
            *pos += 4;
            v
        };
        let ring_count = read_u32(LAND_OUTLINES_BIN, &mut pos);
        let mut rings = Vec::with_capacity(ring_count as usize);
        for _ in 0..ring_count {
            let point_count = read_u32(LAND_OUTLINES_BIN, &mut pos);
            let mut ring = Vec::with_capacity(point_count as usize);
            for _ in 0..point_count {
                let lat = read_f32(LAND_OUTLINES_BIN, &mut pos);
                let lon = read_f32(LAND_OUTLINES_BIN, &mut pos);
                ring.push((lat, lon));
            }
            rings.push(ring);
        }
        rings
    })
}

const MIN_ZOOM: f32 = 1.0;
const MAX_ZOOM: f32 = 25.0;
/// Below this zoom, only the 18x18 field grid is drawn; past it, the 10x10
/// per-field square subdivisions fade in too -- otherwise the whole-globe
/// view is a wall of square lines nobody can read.
const SQUARE_LINES_MIN_ZOOM: f32 = 3.0;

pub struct MapView {
    pan: egui::Vec2,
    zoom: f32,
}

impl Default for MapView {
    fn default() -> Self {
        Self { pan: egui::Vec2::ZERO, zoom: MIN_ZOOM }
    }
}

impl MapView {
    fn project(&self, rect: egui::Rect, lat: f64, lon: f64) -> egui::Pos2 {
        let nx = (lon + 180.0) / 360.0;
        let ny = (90.0 - lat) / 180.0;
        let world_w = rect.width() * self.zoom;
        let world_h = rect.height() * self.zoom;
        egui::pos2(rect.min.x + self.pan.x + nx as f32 * world_w, rect.min.y + self.pan.y + ny as f32 * world_h)
    }

    fn unproject(&self, rect: egui::Rect, pos: egui::Pos2) -> (f64, f64) {
        let world_w = rect.width() * self.zoom;
        let world_h = rect.height() * self.zoom;
        let nx = ((pos.x - rect.min.x - self.pan.x) / world_w) as f64;
        let ny = ((pos.y - rect.min.y - self.pan.y) / world_h) as f64;
        (90.0 - ny * 180.0, nx * 360.0 - 180.0)
    }

    /// Keeps the world image covering the whole rect (no visible edge/void)
    /// after a pan or zoom change.
    fn clamp_pan(&mut self, rect: egui::Rect) {
        let world_w = rect.width() * self.zoom;
        let world_h = rect.height() * self.zoom;
        self.pan.x = self.pan.x.clamp(rect.width() - world_w, 0.0);
        self.pan.y = self.pan.y.clamp(rect.height() - world_h, 0.0);
    }
}

pub enum ContactVia {
    Mail,
    Social,
}

pub struct Contact {
    pub locator: String,
    pub label: String,
    pub last_heard: SystemTime,
    pub via: ContactVia,
}

pub enum MapMode<'a> {
    /// Click anywhere to select a locator; `selected` also drives the
    /// highlighted square shown on entry (e.g. re-opening Settings shows
    /// wherever was picked last time).
    Pick { selected: &'a mut Option<String> },
    View { home: Option<&'a str>, contacts: &'a [Contact] },
}

/// Draws the grid map into whatever space is available in `ui` and handles
/// its own pan (drag) / zoom (scroll) / click input.
pub fn map_ui(ui: &mut egui::Ui, view: &mut MapView, mode: MapMode, height: f32) -> egui::Response {
    let desired_size = egui::vec2(ui.available_width(), height);
    let (rect, response) = ui.allocate_exact_size(desired_size, egui::Sense::click_and_drag());

    if response.dragged() {
        view.pan += response.drag_delta();
        view.clamp_pan(rect);
    }
    if response.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll.abs() > 0.01 {
            if let Some(cursor) = response.hover_pos() {
                let (lat, lon) = view.unproject(rect, cursor);
                view.zoom = (view.zoom * (1.0 + scroll * 0.002)).clamp(MIN_ZOOM, MAX_ZOOM);
                // Re-anchor so the point under the cursor doesn't jump.
                let new_pos = view.project(rect, lat, lon);
                view.pan += cursor - new_pos;
                view.clamp_pan(rect);
            }
        }
    }

    let painter = ui.painter_at(rect);
    let visuals = ui.visuals();
    let bg = visuals.extreme_bg_color;
    let grid_color = visuals.weak_text_color();
    let field_label_color = visuals.text_color().gamma_multiply(0.55);

    painter.rect_filled(rect, egui::Rounding::same(6.0), bg);

    // Real (if low-resolution) coastlines under the grid, so a picked/
    // heard-from square reads against actual geography instead of a blank
    // grid -- see `land_outlines()` above for provenance/licensing.
    let land_color = visuals.weak_text_color().gamma_multiply(0.9);
    for ring in land_outlines() {
        let mut prev: Option<egui::Pos2> = None;
        let mut prev_lon: Option<f32> = None;
        for &(lat, lon) in ring {
            let p = view.project(rect, lat as f64, lon as f64);
            if let (Some(prev_p), Some(prev_lon)) = (prev, prev_lon) {
                // Skip segments that cross the antimeridian rather than
                // drawing a spurious line clear across the map.
                if (lon - prev_lon).abs() < 180.0 {
                    painter.line_segment([prev_p, p], egui::Stroke::new(1.0_f32, land_color));
                }
            }
            prev = Some(p);
            prev_lon = Some(lon);
        }
    }

    let (lat_top, lon_left) = view.unproject(rect, rect.left_top());
    let (lat_bottom, lon_right) = view.unproject(rect, rect.right_bottom());
    let lon_min = (lon_left.min(lon_right) - 20.0).max(-180.0);
    let lon_max = (lon_left.max(lon_right) + 20.0).min(180.0);
    let lat_min = (lat_bottom.min(lat_top) - 10.0).max(-90.0);
    let lat_max = (lat_bottom.max(lat_top) + 10.0).min(90.0);

    // Field grid (18x18, always drawn).
    let mut lon = (lon_min / 20.0).floor() * 20.0;
    while lon <= lon_max {
        let p0 = view.project(rect, -90.0, lon);
        let p1 = view.project(rect, 90.0, lon);
        painter.line_segment([p0, p1], egui::Stroke::new(1.0_f32, grid_color));
        lon += 20.0;
    }
    let mut lat = (lat_min / 10.0).floor() * 10.0;
    while lat <= lat_max {
        let p0 = view.project(rect, lat, -180.0);
        let p1 = view.project(rect, lat, 180.0);
        painter.line_segment([p0, p1], egui::Stroke::new(1.0_f32, grid_color));
        lat += 10.0;
    }

    // Square subdivisions (10x10 per field), only once zoomed in enough to
    // actually read them.
    if view.zoom >= SQUARE_LINES_MIN_ZOOM {
        let square_color = grid_color.gamma_multiply(0.5);
        let mut lon = (lon_min / 2.0).floor() * 2.0;
        while lon <= lon_max {
            let p0 = view.project(rect, lat_min.max(-90.0), lon);
            let p1 = view.project(rect, lat_max.min(90.0), lon);
            painter.line_segment([p0, p1], egui::Stroke::new(0.5_f32, square_color));
            lon += 2.0;
        }
        let mut lat = lat_min.floor();
        while lat <= lat_max {
            let p0 = view.project(rect, lat, lon_min.max(-180.0));
            let p1 = view.project(rect, lat, lon_max.min(180.0));
            painter.line_segment([p0, p1], egui::Stroke::new(0.5_f32, square_color));
            lat += 1.0;
        }
    }

    // Field letter labels (e.g. "IO"), one per visible field cell.
    let mut lon = (lon_min / 20.0).floor() * 20.0;
    while lon < lon_max {
        let mut lat = (lat_min / 10.0).floor() * 10.0;
        while lat < lat_max {
            let center = view.project(rect, lat + 5.0, lon + 10.0);
            if rect.contains(center) {
                let label = maidenhead::to_locator(lat + 5.0, lon + 10.0);
                painter.text(center, egui::Align2::CENTER_CENTER, &label[0..2], egui::FontId::proportional(12.0), field_label_color);
            }
            lat += 10.0;
        }
        lon += 20.0;
    }

    match mode {
        MapMode::Pick { selected } => {
            if response.clicked() {
                if let Some(pos) = response.interact_pointer_pos() {
                    let (lat, lon) = view.unproject(rect, pos);
                    *selected = Some(maidenhead::to_locator(lat, lon));
                }
            }
            if let Some(locator) = selected.as_deref() {
                draw_square_highlight(&painter, view, rect, locator, crate::theme::ACCENT);
            }
        }
        MapMode::View { home, contacts } => {
            let home_pos = home.and_then(|h| maidenhead::to_latlon_center(h)).map(|(lat, lon)| view.project(rect, lat, lon));

            for contact in contacts {
                let Some((lat, lon)) = maidenhead::to_latlon_center(&contact.locator) else { continue };
                let contact_pos = view.project(rect, lat, lon);
                if let Some(home_pos) = home_pos {
                    painter.line_segment([home_pos, contact_pos], egui::Stroke::new(1.5_f32, crate::theme::ACCENT.gamma_multiply(0.7)));
                }
                let dot_color = match contact.via {
                    ContactVia::Mail => crate::theme::ACCENT,
                    ContactVia::Social => crate::theme::SUCCESS,
                };
                painter.circle_filled(contact_pos, 4.0, dot_color);
                painter.text(
                    contact_pos + egui::vec2(6.0, -6.0),
                    egui::Align2::LEFT_BOTTOM,
                    format!("{} ({})", contact.label, contact.locator),
                    egui::FontId::proportional(11.0),
                    ui.visuals().text_color(),
                );
            }

            if let Some(home_pos) = home_pos {
                painter.text(home_pos, egui::Align2::CENTER_CENTER, "\u{1F3E0}", egui::FontId::proportional(18.0), ui.visuals().text_color());
            }
        }
    }

    response
}

fn draw_square_highlight(painter: &egui::Painter, view: &MapView, rect: egui::Rect, locator: &str, color: egui::Color32) {
    let Some((lat, lon)) = maidenhead::to_latlon_center(locator) else { return };
    // The square spans 2 degrees longitude x 1 degree latitude around its center.
    let top_left = view.project(rect, lat + 0.5, lon - 1.0);
    let bottom_right = view.project(rect, lat - 0.5, lon + 1.0);
    painter.rect_stroke(egui::Rect::from_two_pos(top_left, bottom_right), egui::Rounding::ZERO, egui::Stroke::new(2.0_f32, color));
    painter.circle_filled(view.project(rect, lat, lon), 3.0, color);
}
