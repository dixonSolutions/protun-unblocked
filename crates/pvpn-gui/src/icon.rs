//! The orb, drawn at tray sizes in the colour of the current state.
//!
//! Drawn rather than shipped as files so the tray needs nothing installed:
//! StatusNotifierItem accepts raw ARGB pixmaps, and the same drawing serves
//! every state and size.

use gtk::cairo;
use std::f64::consts::PI;

/// Inner (lit) and outer colours for each state's CSS name.
pub fn palette(css: &str) -> ((f64, f64, f64), (f64, f64, f64)) {
    match css {
        "on" => ((0.56, 0.94, 0.72), (0.10, 0.55, 0.32)),
        "busy" => ((1.0, 0.90, 0.50), (0.80, 0.55, 0.05)),
        "dead" => ((1.0, 0.55, 0.50), (0.70, 0.10, 0.12)),
        _ => ((0.72, 0.76, 0.82), (0.30, 0.34, 0.42)),
    }
}

pub fn draw_orb(cr: &cairo::Context, size: f64, css: &str) {
    let ((ir, ig, ib), (or, og, ob)) = palette(css);
    let c = size / 2.0;
    let r = size * 0.46;
    let g = cairo::RadialGradient::new(c - r * 0.35, c - r * 0.4, r * 0.1, c, c, r);
    g.add_color_stop_rgb(0.0, ir, ig, ib);
    g.add_color_stop_rgb(1.0, or, og, ob);
    let _ = cr.set_source(&g);
    cr.arc(c, c, r, 0.0, 2.0 * PI);
    let _ = cr.fill();

    // The tunnel arch.
    let aw = r * 0.95;
    let top = c - r * 0.05;
    let bottom = c + r * 0.62;
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.set_line_width((size * 0.1).max(1.5));
    cr.set_line_cap(cairo::LineCap::Round);
    cr.move_to(c - aw / 2.0, bottom);
    cr.line_to(c - aw / 2.0, top);
    cr.arc(c, top, aw / 2.0, PI, 2.0 * PI);
    cr.line_to(c + aw / 2.0, bottom);
    let _ = cr.stroke();
}

/// ARGB32 in network byte order, straight (not premultiplied) alpha — the
/// StatusNotifierItem pixmap format.
pub fn tray_pixmap(size: i32, css: &str) -> Vec<u8> {
    let Ok(mut surface) = cairo::ImageSurface::create(cairo::Format::ARgb32, size, size) else {
        return Vec::new();
    };
    {
        let Ok(cr) = cairo::Context::new(&surface) else {
            return Vec::new();
        };
        draw_orb(&cr, size as f64, css);
    }
    surface.flush();
    let stride = surface.stride() as usize;
    let Ok(data) = surface.data() else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size as usize {
        for x in 0..size as usize {
            let i = y * stride + x * 4;
            let px = u32::from_ne_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
            let a = (px >> 24) & 0xff;
            let un = |v: u32| if a == 0 { 0 } else { ((v * 255 + a / 2) / a).min(255) };
            let (r, g, b) = (un((px >> 16) & 0xff), un((px >> 8) & 0xff), un(px & 0xff));
            out.extend_from_slice(&[a as u8, r as u8, g as u8, b as u8]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixmaps_are_the_right_size_and_not_empty() {
        let px = tray_pixmap(24, "on");
        assert_eq!(px.len(), 24 * 24 * 4);
        let centre = (12 * 24 + 4) * 4;
        assert_eq!(px[centre], 255, "the orb is opaque inside");
        assert_eq!(px[0], 0, "the corner is transparent");
    }

    #[test]
    fn states_have_different_colours() {
        assert_ne!(palette("on"), palette("dead"));
        assert_ne!(palette("busy"), palette("off"));
    }
}
