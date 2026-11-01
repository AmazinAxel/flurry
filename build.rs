// Bakes the symbolic icons flurry draws into path data (no SVG parsing at runtime).
// Sources are the SVGs vendored in icons/ (see icons/README.md).

use std::fmt::Write;
use std::path::PathBuf;

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

    let mut out = String::from("pub static ICONS: &[(&str, f32, &[Shape])] = &[\n");
    for (name, file) in &found {
        let tree = match std::fs::read(file).map_err(|e| e.to_string()).and_then(|d| usvg::Tree::from_data(&d, &usvg::Options::default()).map_err(|e| e.to_string())) {
            Ok(t) => t,
            Err(e) => {
                println!("cargo:warning=icon {name} not baked: {e}");
                continue;
            }
        };
        writeln!(out, "    (\"{name}\", {:?}, &[", tree.size().width()).unwrap();
        flatten(tree.root(), 1.0, &mut out);
        out.push_str("    ]),\n");
    }
    out.push_str("];\n");
    let dest = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("icons.rs");
    std::fs::write(dest, out).unwrap();
    fonts_check();
}

/// Text renders nothing without a font table: package.nix writes it into settings.rs, the dev shell
/// passes it as FLURRY_FONTS. Said here once instead of at runtime.
fn fonts_check() {
    println!("cargo:rerun-if-changed=settings.rs");
    println!("cargo:rerun-if-env-changed=FLURRY_FONTS");
    let settings = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("settings.rs");
    let empty_table = std::fs::read_to_string(settings).is_ok_and(|s| s.lines().any(|l| l.starts_with("pub const FONTS:") && l.trim_end().ends_with("= &[];")));
    if empty_table && std::env::var("FLURRY_FONTS").unwrap_or_default().trim().is_empty() {
        println!("cargo:warning=no fonts: settings.rs FONTS is empty and FLURRY_FONTS is unset (use nix-shell / nix develop); text will not render");
    }
}
