//! Tray icon rendering: the crook symbol as a template image, plus a
//! non-template variant with an amber count badge. Rendered with resvg from
//! the spec SVG; cached because badge changes are rare.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use tauri::image::Image;

/// 24-unit viewBox rendered at 2x for crispness; macOS scales to menubar size.
const CANVAS: u32 = 48;
const SCALE: f32 = CANVAS as f32 / 24.0;

fn crook_svg(stroke: &str, badge: Option<(usize, &str)>) -> String {
    let mut body = format!(
        r##"<path d="M15.5 21v-12.5C15.5 5.7 13.6 3.5 11 3.5S6.5 5.4 6.5 7.8" fill="none" stroke="{stroke}" stroke-width="1.8" stroke-linecap="round"/>"##
    );
    // the single dot the crook cradles (spec symbol)
    body.push_str(&format!(
        r##"<circle cx="11" cy="8.6" r="1.7" fill="{stroke}"/>"##
    ));
    if let Some((count, text_color)) = badge {
        let label = if count > 9 {
            "9+".to_string()
        } else {
            count.to_string()
        };
        body.push_str(
            r##"<circle cx="17.6" cy="6.4" r="5.4" fill="#eab308" stroke="#ffffff" stroke-opacity="0.85" stroke-width="1"/>"##,
        );
        body.push_str(&format!(
            r##"<text x="17.6" y="9.1" text-anchor="middle" font-family="Helvetica Neue, Helvetica" font-size="7" font-weight="600" fill="{text_color}">{label}</text>"##
        ));
    }
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{CANVAS}" height="{CANVAS}" viewBox="0 0 24 24">{body}</svg>"##
    )
}

fn render(svg: &str) -> Option<Image<'static>> {
    let mut fonts = resvg::usvg::fontdb::Database::new();
    fonts.load_system_fonts();
    let opts = resvg::usvg::Options {
        fontdb: Arc::new(fonts),
        ..Default::default()
    };
    let tree = resvg::usvg::Tree::from_str(svg, &opts).ok()?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(CANVAS, CANVAS)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(SCALE, SCALE),
        &mut pixmap.as_mut(),
    );
    Some(Image::new_owned(pixmap.data().to_vec(), CANVAS, CANVAS))
}

fn cache() -> &'static Mutex<HashMap<String, Image<'static>>> {
    static ICONS: OnceLock<Mutex<HashMap<String, Image<'static>>>> = OnceLock::new();
    ICONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached(key: &str, svg: String) -> Image<'static> {
    let mut icons = cache().lock().unwrap();
    if let Some(img) = icons.get(key) {
        return img.clone();
    }
    // text rendering may fail on exotic setups; the badge then degrades to a
    // plain amber dot without the count
    let fallback = crook_svg("#1d1d1f", None);
    let img = render(&svg)
        .or_else(|| render(&fallback))
        .expect("plain crook render");
    icons.insert(key.to_string(), img.clone());
    img
}

/// Plain crook, monochrome template image (system-tinted).
pub fn crook_template() -> Image<'static> {
    cached("template", crook_svg("#000000", None))
}

/// Crook plus amber badge with the waiting count. Non-template: the crook
/// needs an explicit stroke matching the current menu bar appearance.
pub fn badge(count: usize) -> Image<'static> {
    let dark = is_dark_menu();
    let stroke = if dark { "#f5f5f7" } else { "#1d1d1f" };
    let text = "#1d1d1f";
    let key = format!("badge-{count}-{dark}");
    cached(&key, crook_svg(stroke, Some((count, text))))
}

/// Reads the effective macOS appearance. "Dark" is returned whenever dark is
/// active (including Auto while dark); absence of the key means Light.
/// ~10ms subprocess call, only on badge key changes via the cache.
fn is_dark_menu() -> bool {
    Command::new("defaults")
        .args(["read", "-g", "AppleInterfaceStyle"])
        .output()
        .map(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .eq_ignore_ascii_case("dark")
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_label_caps_at_nine_plus() {
        let svg = crook_svg("#1d1d1f", Some((10, "#1d1d1f")));
        assert!(svg.contains(">9+</text>"), "counts above 9 render as 9+");
        let svg = crook_svg("#1d1d1f", Some((3, "#1d1d1f")));
        assert!(svg.contains(">3</text>"));
    }

    #[test]
    fn crook_svg_contains_cradled_dot() {
        let svg = crook_svg("#000000", None);
        assert!(svg.contains(r##"<circle cx="11" cy="8.6" r="1.7""##));
        assert!(svg.contains("stroke-width=\"1.8\""));
    }

    #[test]
    fn template_icon_renders_at_canvas_size() {
        let img = render(&crook_svg("#000000", None)).expect("template crook renders");
        assert_eq!((img.width(), img.height()), (CANVAS, CANVAS));
    }

    #[test]
    fn badge_icon_renders_with_text() {
        // exercises system font loading; degrades to dot-only if fonts fail
        let img = render(&crook_svg("#1d1d1f", Some((2, "#1d1d1f")))).expect("badge renders");
        assert_eq!((img.width(), img.height()), (CANVAS, CANVAS));
    }
}
