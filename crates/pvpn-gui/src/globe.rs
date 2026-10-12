//! A spinnable globe with the servers this network knows about on it.
//!
//! Orthographic projection of the Natural Earth 1:110m land polygons,
//! clipped properly at the horizon (a ring leaving the visible hemisphere
//! is continued along the limb until it comes back), so no continent smears
//! across the face when it rotates behind.

use crate::data::Kind;
use gtk::cairo;
use gtk::glib;
use gtk::prelude::*;
use std::cell::RefCell;
use std::f64::consts::PI;
use std::rc::Rc;
use std::sync::OnceLock;

const LAND: &str = include_str!("../data/land.txt");

fn land() -> &'static Vec<Vec<(f64, f64)>> {
    static CELL: OnceLock<Vec<Vec<(f64, f64)>>> = OnceLock::new();
    CELL.get_or_init(|| parse_land(LAND))
}

/// `lon,lat lon,lat …` per line, `#` comments.
pub fn parse_land(text: &str) -> Vec<Vec<(f64, f64)>> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            l.split_whitespace()
                .filter_map(|pair| {
                    let (lon, lat) = pair.split_once(',')?;
                    Some((lat.parse().ok()?, lon.parse().ok()?))
                })
                .collect()
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct Marker {
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub kind: Kind,
    pub rank: Option<usize>,
    /// Draw its name, not just its dot.
    pub label: bool,
    pub current: bool,
    pub tooltip: String,
}

struct State {
    lat0: f64,
    lon0: f64,
    target: Option<(f64, f64)>,
    zoom: f64,
    drag_origin: Option<(f64, f64)>,
    markers: Vec<Marker>,
    home: Option<(f64, f64)>,
    /// Home → current server, drawn as a great-circle arc.
    route: Option<(f64, f64)>,
    dark: bool,
    busy: bool,
    phase: f64,
    /// Screen positions from the last draw, for hit testing.
    hits: Vec<(f64, f64, String)>,
}

#[derive(Clone)]
pub struct Globe {
    pub area: gtk::DrawingArea,
    state: Rc<RefCell<State>>,
    on_pick: Rc<RefCell<Option<Box<dyn Fn(&str, f64, f64)>>>>,
}

/// Rotated unit vector for (lat, lon) with the view centred on (lat0, lon0):
/// +z towards the viewer, +y up.
fn project(lat: f64, lon: f64, lat0: f64, lon0: f64) -> (f64, f64, f64) {
    let (lat, lon, lat0, lon0) = (lat.to_radians(), lon.to_radians(), lat0.to_radians(), lon0.to_radians());
    let d = lon - lon0;
    let x = lat.cos() * d.sin();
    let y = lat0.cos() * lat.sin() - lat0.sin() * lat.cos() * d.cos();
    let z = lat0.sin() * lat.sin() + lat0.cos() * lat.cos() * d.cos();
    (x, y, z)
}

/// Where a ring crosses the horizon between two rotated points, as a
/// screen angle on the limb.
fn limb_angle(p: (f64, f64, f64), q: (f64, f64, f64)) -> f64 {
    let t = p.2 / (p.2 - q.2);
    let x = p.0 + t * (q.0 - p.0);
    let y = p.1 + t * (q.1 - p.1);
    // Screen y points down.
    (-y).atan2(x)
}

/// Continue along the limb from angle `a` to `b` the short way round.
fn limb_arc(cr: &cairo::Context, cx: f64, cy: f64, r: f64, a: f64, b: f64) {
    let mut delta = b - a;
    while delta > PI {
        delta -= 2.0 * PI;
    }
    while delta < -PI {
        delta += 2.0 * PI;
    }
    if delta >= 0.0 {
        cr.arc(cx, cy, r, a, a + delta);
    } else {
        cr.arc_negative(cx, cy, r, a, a + delta);
    }
}

/// Wrap a longitude into (-180, 180].
fn wrap(lon: f64) -> f64 {
    let mut l = (lon + 180.0).rem_euclid(360.0) - 180.0;
    if l <= -180.0 {
        l += 360.0;
    }
    l
}

impl Globe {
    pub fn new() -> Self {
        let area = gtk::DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .content_width(360)
            .content_height(360)
            .has_tooltip(true)
            .build();
        let home = pvpn_core::geo::local_coordinates();
        let state = Rc::new(RefCell::new(State {
            lat0: home.map(|h| h.0.clamp(-60.0, 60.0)).unwrap_or(20.0),
            lon0: home.map(|h| h.1).unwrap_or(100.0),
            target: None,
            zoom: 1.0,
            drag_origin: None,
            markers: Vec::new(),
            home,
            route: None,
            dark: adw::StyleManager::default().is_dark(),
            busy: false,
            phase: 0.0,
            hits: Vec::new(),
        }));
        let globe = Self {
            area,
            state,
            on_pick: Rc::new(RefCell::new(None)),
        };
        globe.wire();
        globe
    }

    fn wire(&self) {
        let st = self.state.clone();
        self.area.set_draw_func(move |_, cr, w, h| {
            draw(&mut st.borrow_mut(), cr, w as f64, h as f64);
        });

        let drag = gtk::GestureDrag::new();
        let st = self.state.clone();
        drag.connect_drag_begin(move |_, _, _| {
            let mut s = st.borrow_mut();
            s.target = None;
            s.drag_origin = Some((s.lat0, s.lon0));
        });
        let st = self.state.clone();
        let area = self.area.clone();
        drag.connect_drag_update(move |_, dx, dy| {
            let mut s = st.borrow_mut();
            if let Some((lat, lon)) = s.drag_origin {
                let r = radius(area.width() as f64, area.height() as f64, s.zoom);
                s.lon0 = wrap(lon - (dx / r).to_degrees());
                s.lat0 = (lat + (dy / r).to_degrees()).clamp(-85.0, 85.0);
            }
            drop(s);
            area.queue_draw();
        });
        let st = self.state.clone();
        drag.connect_drag_end(move |_, _, _| {
            st.borrow_mut().drag_origin = None;
        });
        self.area.add_controller(drag);

        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let st = self.state.clone();
        let area = self.area.clone();
        scroll.connect_scroll(move |_, _, dy| {
            let mut s = st.borrow_mut();
            s.zoom = (s.zoom * if dy < 0.0 { 1.12 } else { 1.0 / 1.12 }).clamp(0.7, 4.0);
            drop(s);
            area.queue_draw();
            glib::Propagation::Stop
        });
        self.area.add_controller(scroll);

        let click = gtk::GestureClick::new();
        let st = self.state.clone();
        let pick = self.on_pick.clone();
        click.connect_released(move |gesture, _, x, y| {
            let hit = nearest(&st.borrow().hits, x, y, 12.0);
            if let Some(name) = hit {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                if let Some(cb) = pick.borrow().as_ref() {
                    cb(&name, x, y);
                }
            }
        });
        self.area.add_controller(click);

        let st = self.state.clone();
        self.area.connect_query_tooltip(move |_, x, y, _, tip| {
            let s = st.borrow();
            let Some(name) = nearest(&s.hits, x as f64, y as f64, 10.0) else {
                return false;
            };
            let text = s
                .markers
                .iter()
                .find(|m| m.name == name)
                .map(|m| m.tooltip.clone())
                .unwrap_or(name);
            tip.set_text(Some(&text));
            true
        });

        // Redraw in the right palette when the system theme flips.
        let st = self.state.clone();
        let area = self.area.clone();
        adw::StyleManager::default().connect_dark_notify(move |sm| {
            st.borrow_mut().dark = sm.is_dark();
            area.queue_draw();
        });

        // The only animation: easing towards a target, and the pulse on the
        // current server while something is connecting. Idle otherwise.
        let st = self.state.clone();
        self.area.add_tick_callback(move |area, clock| {
            let mut s = st.borrow_mut();
            let mut redraw = false;
            if let Some((tlat, tlon)) = s.target {
                let mut dlon = wrap(tlon - s.lon0);
                if dlon.abs() < 0.2 && (tlat - s.lat0).abs() < 0.2 {
                    s.lat0 = tlat;
                    s.lon0 = tlon;
                    s.target = None;
                } else {
                    dlon *= 0.1;
                    s.lon0 = wrap(s.lon0 + dlon);
                    s.lat0 += (tlat - s.lat0) * 0.1;
                }
                redraw = true;
            }
            if s.busy || s.markers.iter().any(|m| m.current) {
                s.phase = (clock.frame_time() as f64 / 1_000_000.0) % 2.0;
                redraw = true;
            }
            drop(s);
            if redraw {
                area.queue_draw();
            }
            glib::ControlFlow::Continue
        });
    }

    pub fn connect_pick(&self, f: impl Fn(&str, f64, f64) + 'static) {
        *self.on_pick.borrow_mut() = Some(Box::new(f));
    }

    pub fn set_markers(&self, markers: Vec<Marker>) {
        let mut s = self.state.borrow_mut();
        s.route = markers.iter().find(|m| m.current).map(|m| (m.lat, m.lon));
        s.markers = markers;
        drop(s);
        self.area.queue_draw();
    }

    pub fn set_busy(&self, busy: bool) {
        self.state.borrow_mut().busy = busy;
        self.area.queue_draw();
    }

    /// Turn the globe to face a point.
    pub fn fly_to(&self, lat: f64, lon: f64) {
        self.state.borrow_mut().target = Some((lat.clamp(-60.0, 60.0), wrap(lon)));
    }

    pub fn fly_home(&self) {
        let home = self.state.borrow().home;
        if let Some((lat, lon)) = home {
            self.fly_to(lat, lon);
        }
    }
}

fn radius(w: f64, h: f64, zoom: f64) -> f64 {
    (w.min(h) / 2.0 - 14.0).max(20.0) * zoom
}

fn nearest(hits: &[(f64, f64, String)], x: f64, y: f64, within: f64) -> Option<String> {
    hits.iter()
        .map(|(hx, hy, n)| (((hx - x).powi(2) + (hy - y).powi(2)).sqrt(), n))
        .filter(|(d, _)| *d <= within)
        .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap())
        .map(|(_, n)| n.clone())
}

fn kind_rgb(kind: Kind) -> (f64, f64, f64) {
    match kind {
        Kind::Working => (0.18, 0.76, 0.49),
        Kind::Fast => (0.21, 0.52, 0.89),
        Kind::Known => (0.60, 0.60, 0.59),
        Kind::Blocked => (0.88, 0.11, 0.14),
    }
}

fn draw(s: &mut State, cr: &cairo::Context, w: f64, h: f64) {
    let (cx, cy) = (w / 2.0, h / 2.0);
    let r = radius(w, h, s.zoom);
    let (lat0, lon0) = (s.lat0, s.lon0);
    let dark = s.dark;

    // Atmosphere.
    let glow = cairo::RadialGradient::new(cx, cy, r * 0.95, cx, cy, r * 1.12);
    let (gr, gg, gb) = if dark { (0.35, 0.55, 1.0) } else { (0.35, 0.55, 0.95) };
    glow.add_color_stop_rgba(0.0, gr, gg, gb, 0.35);
    glow.add_color_stop_rgba(1.0, gr, gg, gb, 0.0);
    let _ = cr.set_source(&glow);
    cr.arc(cx, cy, r * 1.12, 0.0, 2.0 * PI);
    let _ = cr.fill();

    // Ocean, lit from the upper left.
    let ocean = cairo::RadialGradient::new(cx - r * 0.35, cy - r * 0.35, r * 0.1, cx, cy, r);
    if dark {
        ocean.add_color_stop_rgb(0.0, 0.10, 0.20, 0.36);
        ocean.add_color_stop_rgb(1.0, 0.03, 0.07, 0.15);
    } else {
        ocean.add_color_stop_rgb(0.0, 0.80, 0.89, 0.98);
        ocean.add_color_stop_rgb(1.0, 0.52, 0.68, 0.88);
    }
    let _ = cr.set_source(&ocean);
    cr.arc(cx, cy, r, 0.0, 2.0 * PI);
    let _ = cr.fill();

    let _ = cr.save();
    cr.arc(cx, cy, r, 0.0, 2.0 * PI);
    cr.clip();

    // Graticule.
    cr.set_line_width(0.6);
    cr.set_source_rgba(1.0, 1.0, 1.0, if dark { 0.07 } else { 0.25 });
    for lat in (-60..=60).step_by(30) {
        polyline(cr, cx, cy, r, (0..=72).map(|i| (lat as f64, -180.0 + i as f64 * 5.0)), lat0, lon0);
    }
    for lon in (-180..180).step_by(30) {
        polyline(cr, cx, cy, r, (0..=36).map(|i| (-90.0 + i as f64 * 5.0, lon as f64)), lat0, lon0);
    }

    // Land.
    for ring in land() {
        land_path(cr, cx, cy, r, ring, lat0, lon0);
    }
    if dark {
        cr.set_source_rgb(0.20, 0.33, 0.30);
    } else {
        cr.set_source_rgb(0.93, 0.95, 0.90);
    }
    let _ = cr.fill_preserve();
    cr.set_line_width(0.7);
    cr.set_source_rgba(1.0, 1.0, 1.0, if dark { 0.18 } else { 0.9 });
    let _ = cr.stroke();

    // Shade the far side of the sphere a little for depth.
    let shade = cairo::RadialGradient::new(cx - r * 0.3, cy - r * 0.3, r * 0.2, cx, cy, r * 1.05);
    shade.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, if dark { 0.04 } else { 0.10 });
    shade.add_color_stop_rgba(1.0, 0.0, 0.0, 0.0, if dark { 0.35 } else { 0.18 });
    let _ = cr.set_source(&shade);
    cr.paint().ok();

    // Home → current server.
    if let (Some(home), Some(dest)) = (s.home, s.route) {
        cr.set_line_width(2.0);
        cr.set_dash(&[6.0, 4.0], s.phase * 10.0);
        cr.set_source_rgba(0.18, 0.76, 0.49, 0.9);
        polyline(cr, cx, cy, r, great_circle(home, dest, 64).into_iter(), lat0, lon0);
        cr.set_dash(&[], 0.0);
    }
    let _ = cr.restore();

    // Home.
    if let Some((hlat, hlon)) = s.home {
        let (x, y, z) = project(hlat, hlon, lat0, lon0);
        if z > 0.0 {
            let (px, py) = (cx + r * x, cy - r * y);
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.95);
            cr.arc(px, py, 4.0, 0.0, 2.0 * PI);
            let _ = cr.fill();
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.5);
            cr.set_line_width(1.5);
            cr.arc(px, py, 8.0, 0.0, 2.0 * PI);
            let _ = cr.stroke();
            label(cr, px + 10.0, py + 4.0, "You", dark, None);
        }
    }

    // Servers, back to front: unlabelled, labelled, current.
    s.hits.clear();
    let mut order: Vec<&Marker> = s.markers.iter().collect();
    order.sort_by_key(|m| (m.current, m.label, std::cmp::Reverse(m.rank.unwrap_or(usize::MAX))));
    let mut placed: Vec<(f64, f64, f64, f64)> = Vec::new();
    let mut hits = Vec::new();
    for m in order {
        let (x, y, z) = project(m.lat, m.lon, lat0, lon0);
        if z < 0.05 {
            continue;
        }
        let (px, py) = (cx + r * x, cy - r * y);
        let (mr, mg, mb) = kind_rgb(m.kind);
        let size = if m.current { 7.0 } else if m.label { 5.0 } else { 3.5 };
        if m.current {
            let pulse = (s.phase * PI).sin().abs();
            cr.set_source_rgba(mr, mg, mb, 0.35 * (1.0 - pulse) + 0.1);
            cr.arc(px, py, size + 6.0 + 8.0 * pulse, 0.0, 2.0 * PI);
            let _ = cr.fill();
        }
        cr.set_source_rgb(mr, mg, mb);
        cr.arc(px, py, size, 0.0, 2.0 * PI);
        let _ = cr.fill_preserve();
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.9);
        cr.set_line_width(1.2);
        let _ = cr.stroke();
        if m.kind == Kind::Blocked {
            cr.set_source_rgb(1.0, 1.0, 1.0);
            cr.set_line_width(1.4);
            let k = size * 0.55;
            cr.move_to(px - k, py - k);
            cr.line_to(px + k, py + k);
            cr.move_to(px + k, py - k);
            cr.line_to(px - k, py + k);
            let _ = cr.stroke();
        }
        if m.label || m.current {
            let text = match m.rank {
                Some(n) => format!("{n}  {}", m.name),
                None => m.name.clone(),
            };
            let (lw, lh) = measure(cr, &text);
            let rect = (px + size + 4.0, py - lh / 2.0 - 3.0, lw + 10.0, lh + 6.0);
            let clash = placed.iter().any(|p| {
                rect.0 < p.0 + p.2 && p.0 < rect.0 + rect.2 && rect.1 < p.1 + p.3 && p.1 < rect.1 + rect.3
            });
            if !clash || m.current {
                label(cr, rect.0, py + lh / 2.0 - 1.0, &text, dark, Some((mr, mg, mb, m.current)));
                placed.push(rect);
            }
        }
        hits.push((px, py, m.name.clone()));
    }
    s.hits = hits;

    // Rim.
    cr.new_path();
    cr.set_source_rgba(1.0, 1.0, 1.0, if dark { 0.15 } else { 0.6 });
    cr.set_line_width(1.0);
    cr.arc(cx, cy, r, 0.0, 2.0 * PI);
    let _ = cr.stroke();
}

fn measure(cr: &cairo::Context, text: &str) -> (f64, f64) {
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(11.0);
    cr.text_extents(text)
        .map(|e| (e.x_advance(), 11.0))
        .unwrap_or((text.len() as f64 * 6.5, 11.0))
}

/// A pill with text; `accent` tints it with a marker's colour.
fn label(cr: &cairo::Context, x: f64, baseline: f64, text: &str, dark: bool, accent: Option<(f64, f64, f64, bool)>) {
    let (w, h) = measure(cr, text);
    let (bx, by, bw, bh) = (x, baseline - h - 2.0, w + 10.0, h + 6.0);
    rounded(cr, bx, by, bw, bh, bh / 2.0);
    match accent {
        Some((r, g, b, true)) => cr.set_source_rgba(r, g, b, 0.95),
        _ if dark => cr.set_source_rgba(0.08, 0.10, 0.14, 0.82),
        _ => cr.set_source_rgba(1.0, 1.0, 1.0, 0.9),
    }
    let _ = cr.fill_preserve();
    if let Some((r, g, b, false)) = accent {
        cr.set_source_rgba(r, g, b, 0.9);
        cr.set_line_width(1.2);
        let _ = cr.stroke();
    } else {
        cr.new_path();
    }
    match accent {
        Some((_, _, _, true)) => cr.set_source_rgb(1.0, 1.0, 1.0),
        _ if dark => cr.set_source_rgb(0.95, 0.95, 0.97),
        _ => cr.set_source_rgb(0.12, 0.12, 0.15),
    }
    cr.move_to(bx + 5.0, baseline + 1.0);
    let _ = cr.show_text(text);
    // show_text leaves a current point; the next arc would draw a line from it.
    cr.new_path();
}

fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

/// Stroke the visible stretches of a line of (lat, lon) points.
fn polyline(
    cr: &cairo::Context,
    cx: f64,
    cy: f64,
    r: f64,
    points: impl Iterator<Item = (f64, f64)>,
    lat0: f64,
    lon0: f64,
) {
    let mut pen = false;
    for (lat, lon) in points {
        let (x, y, z) = project(lat, lon, lat0, lon0);
        if z >= 0.0 {
            if pen {
                cr.line_to(cx + r * x, cy - r * y);
            } else {
                cr.move_to(cx + r * x, cy - r * y);
                pen = true;
            }
        } else {
            pen = false;
        }
    }
    let _ = cr.stroke();
}

/// Add one land ring to the path, clipped at the horizon.
fn land_path(cr: &cairo::Context, cx: f64, cy: f64, r: f64, ring: &[(f64, f64)], lat0: f64, lon0: f64) {
    let pts: Vec<(f64, f64, f64)> = ring.iter().map(|&(lat, lon)| project(lat, lon, lat0, lon0)).collect();
    let n = pts.len();
    // Start from a visible point so every hidden stretch has both ends.
    let Some(start) = pts.iter().position(|p| p.2 >= 0.0) else {
        return;
    };
    cr.new_sub_path();
    let first = pts[start];
    cr.move_to(cx + r * first.0, cy - r * first.1);
    let mut exit: Option<f64> = None;
    for step in 1..=n {
        let prev = pts[(start + step - 1) % n];
        let cur = pts[(start + step) % n];
        match (prev.2 >= 0.0, cur.2 >= 0.0) {
            (true, true) => cr.line_to(cx + r * cur.0, cy - r * cur.1),
            (true, false) => {
                let a = limb_angle(prev, cur);
                cr.line_to(cx + r * a.cos(), cy + r * a.sin());
                exit = Some(a);
            }
            (false, true) => {
                let a = limb_angle(cur, prev);
                if let Some(e) = exit.take() {
                    limb_arc(cr, cx, cy, r, e, a);
                }
                cr.line_to(cx + r * cur.0, cy - r * cur.1);
            }
            (false, false) => {}
        }
    }
    cr.close_path();
}

/// Points along the great circle from `a` to `b`, both (lat, lon).
pub fn great_circle(a: (f64, f64), b: (f64, f64), steps: usize) -> Vec<(f64, f64)> {
    let v = |(lat, lon): (f64, f64)| {
        let (lat, lon) = (lat.to_radians(), lon.to_radians());
        (lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin())
    };
    let (p, q) = (v(a), v(b));
    let dot = (p.0 * q.0 + p.1 * q.1 + p.2 * q.2).clamp(-1.0, 1.0);
    let omega = dot.acos();
    if omega.abs() < 1e-9 {
        return vec![a, b];
    }
    (0..=steps)
        .map(|i| {
            let t = i as f64 / steps as f64;
            let s1 = ((1.0 - t) * omega).sin() / omega.sin();
            let s2 = (t * omega).sin() / omega.sin();
            let (x, y, z) = (s1 * p.0 + s2 * q.0, s1 * p.1 + s2 * q.1, s1 * p.2 + s2 * q.2);
            (z.atan2((x * x + y * y).sqrt()).to_degrees(), y.atan2(x).to_degrees())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_coastline_parses() {
        let rings = parse_land(LAND);
        assert!(rings.len() > 100, "{} rings", rings.len());
        assert!(rings.iter().all(|r| r.len() >= 4));
        assert!(rings.iter().flatten().all(|(lat, lon)| lat.abs() <= 90.0 && lon.abs() <= 180.0));
    }

    #[test]
    fn the_view_centre_faces_the_viewer_and_its_antipode_does_not() {
        let (x, y, z) = project(-33.9, 151.2, -33.9, 151.2);
        assert!(x.abs() < 1e-9 && y.abs() < 1e-9 && (z - 1.0).abs() < 1e-9);
        assert!(project(33.9, -28.8, -33.9, 151.2).2 < -0.99);
    }

    #[test]
    fn great_circle_ends_where_asked() {
        let arc = great_circle((-33.9, 151.2), (1.35, 103.8), 16);
        let (lat, lon) = *arc.last().unwrap();
        assert!((lat - 1.35).abs() < 1e-6 && (lon - 103.8).abs() < 1e-6);
        assert_eq!(arc.len(), 17);
    }

    #[test]
    fn longitudes_wrap() {
        assert_eq!(wrap(190.0), -170.0);
        assert_eq!(wrap(-180.0), 180.0);
        assert_eq!(wrap(45.0), 45.0);
    }
}
