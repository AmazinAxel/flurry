// Bakes the symbolic icons flurry draws into path data (no SVG parsing at runtime).
// Sources are the SVGs vendored in icons/ (see icons/README.md).

use std::fmt::Write;
use std::path::PathBuf;

const NAMES: &[&str] = &[
    "audio-input-microphone", "microphone-disabled", "bluetooth-active", "dialog-password", "display-brightness",
    "emblem-default", "media-playback-pause", "media-playback-start", "media-record",
    "network-wireless-acquiring", "network-wireless-encrypted", "network-wireless-offline", "network-wireless",
    "notifications-disabled", "view-refresh",
];



fn flatten(group: &usvg::Group, opacity: f32, out: &mut String) {
    for node in group.children() {
        match node {
            usvg::Node::Group(g) => flatten(g, opacity * g.opacity().get(), out),
            usvg::Node::Path(p) => {
                if !p.is_visible() {
                    continue;
                }
                let ts = p.abs_transform();
                if let Some(fill) = p.fill() {
                    let even_odd = fill.rule() == usvg::FillRule::EvenOdd;
                    if let Some(path) = p.data().clone().transform(ts) {
                        push_shape(&path, opacity * fill.opacity().get(), even_odd, out);
                    }
                }
                if let Some(stroke) = p.stroke()
                    && let Some(s) = stroke.to_tiny_skia_stroke()
                    && let Some(outline) = p.data().stroke(&s, 1.0)
                    && let Some(path) = outline.transform(ts)
                {
                    push_shape(&path, opacity * stroke.opacity().get(), false, out);
                }
            }
            _ => {}
        }
    }
}

trait ToStroke {
    fn to_tiny_skia_stroke(&self) -> Option<usvg::tiny_skia_path::Stroke>;
}
impl ToStroke for usvg::Stroke {
    fn to_tiny_skia_stroke(&self) -> Option<usvg::tiny_skia_path::Stroke> {
        Some(usvg::tiny_skia_path::Stroke {
            width: self.width().get(),
            miter_limit: self.miterlimit().get(),
            line_cap: match self.linecap() {
                usvg::LineCap::Butt => usvg::tiny_skia_path::LineCap::Butt,
                usvg::LineCap::Round => usvg::tiny_skia_path::LineCap::Round,
                usvg::LineCap::Square => usvg::tiny_skia_path::LineCap::Square,
            },
            line_join: match self.linejoin() {
                usvg::LineJoin::Miter | usvg::LineJoin::MiterClip => usvg::tiny_skia_path::LineJoin::Miter,
                usvg::LineJoin::Round => usvg::tiny_skia_path::LineJoin::Round,
                usvg::LineJoin::Bevel => usvg::tiny_skia_path::LineJoin::Bevel,
            },
            dash: None,
        })
    }
}

fn push_shape(path: &usvg::tiny_skia_path::Path, opacity: f32, even_odd: bool, out: &mut String) {
    use usvg::tiny_skia_path::PathSegment::*;
    let mut s = String::new();
    for seg in path.segments() {
        match seg {
            MoveTo(p) => write!(s, "M({:?},{:?}),", p.x, p.y),
            LineTo(p) => write!(s, "L({:?},{:?}),", p.x, p.y),
            QuadTo(a, p) => write!(s, "Q({:?},{:?},{:?},{:?}),", a.x, a.y, p.x, p.y),
            CubicTo(a, b, p) => write!(s, "C({:?},{:?},{:?},{:?},{:?},{:?}),", a.x, a.y, b.x, b.y, p.x, p.y),
            Close => write!(s, "Z,"),
        }
        .unwrap();
    }
    writeln!(out, "        Shape {{ opacity: {opacity:?}, even_odd: {even_odd}, segs: &[{s}] }},").unwrap();
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=icons");
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("icons");
    let mut found: Vec<(String, PathBuf)> = std::fs::read_dir(&dir)
        .expect("icons/")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with("-symbolic.svg"))
        .map(|p| (p.file_name().unwrap().to_string_lossy().trim_end_matches(".svg").to_string(), p))
        .collect();
    // sorted by the full icon name: icons.rs looks them up with a binary search
    found.sort();
    let missing: Vec<&&str> = NAMES.iter().filter(|n| !found.iter().any(|(f, _)| *f == format!("{n}-symbolic"))).collect();
    if !missing.is_empty() {
        println!("cargo:warning=missing icons: {missing:?}");
    }

    let mut out = String::from("pub static ICONS: &[(&str, f32, &[Shape])] = &[\n");
    for (name, file) in &found {
        let Ok(data) = std::fs::read(file) else { continue };
        let Ok(tree) = usvg::Tree::from_data(&data, &usvg::Options::default()) else { continue };
        writeln!(out, "    (\"{name}\", {:?}, &[", tree.size().width()).unwrap();
        flatten(tree.root(), 1.0, &mut out);
        out.push_str("    ]),\n");
    }
    out.push_str("];\n");
    let dest = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("icons.rs");
    std::fs::write(dest, out).unwrap();
}
