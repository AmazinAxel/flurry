// Image decoding (png, jpeg) into premultiplied pixmaps, gaussian blur, and box-fit scaling.

use tiny_skia::{IntSize, Pixmap};

pub fn decode_png(bytes: &[u8]) -> Option<Pixmap> {
    let mut decoder = png::Decoder::new(bytes);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf.chunks(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf.chunks(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    from_rgba(rgba, info.width, info.height)
}

fn jpeg_decoder(bytes: &[u8]) -> zune_jpeg::JpegDecoder<&[u8]> {
    let opts = zune_jpeg::zune_core::options::DecoderOptions::default()
        .jpeg_set_out_colorspace(zune_jpeg::zune_core::colorspace::ColorSpace::RGBA);
    zune_jpeg::JpegDecoder::new_with_options(bytes, opts)
}

pub fn decode_jpeg(bytes: &[u8]) -> Option<Pixmap> {
    let mut d = jpeg_decoder(bytes);
    let px = d.decode().ok()?;
    let (w, h) = d.dimensions()?;
    from_rgba(px, w as u32, h as u32)
}

/// Sniffs the format from the header.
pub fn decode(bytes: &[u8]) -> Option<Pixmap> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => decode_png(bytes),
        [0xff, 0xd8, ..] => decode_jpeg(bytes),
        _ => None,
    }
}

/// Decodes into the buffer `alloc(w, h, fill)` provides, as premultiplied RGBA; `fill` reports
/// success. JPEGs (opaque, so already premultiplied) decode straight in, with no intermediate copy.
pub fn decode_into<T>(bytes: &[u8], alloc: impl FnOnce(u32, u32, &dyn Fn(&mut [u8]) -> bool) -> Option<T>) -> Option<T> {
    if bytes.starts_with(&[0xff, 0xd8]) {
        let mut d = jpeg_decoder(bytes);
        d.decode_headers().ok()?;
        let (w, h) = d.dimensions()?;
        let d = std::cell::RefCell::new(d);
        return alloc(w as u32, h as u32, &|dst| d.borrow_mut().decode_into(dst).is_ok());
    }
    let pix = decode(bytes)?;
    alloc(pix.width(), pix.height(), &|dst| {
        dst.copy_from_slice(pix.data());
        true
    })
}

fn from_rgba(mut rgba: Vec<u8>, w: u32, h: u32) -> Option<Pixmap> {
    for p in rgba.as_chunks_mut::<4>().0 {
        let a = p[3] as u16;
        if a != 255 {
            for c in &mut p[..3] {
                *c = ((*c as u16 * a + 127) / 255) as u8;
            }
        }
    }
    Pixmap::from_vec(rgba, IntSize::from_wh(w, h)?)
}

/// Average Rec.601 luma of a 16x9 nearest-neighbour sample (wallpaper.tsx `decode`).
pub fn is_light(p: tiny_skia::PixmapRef) -> bool {
    let mut sum = 0.0;
    for y in 0..9 {
        for x in 0..16 {
            // GdkPixbuf NEAREST samples at the center of each destination pixel
            let sx = ((x as f32 + 0.5) * p.width() as f32 / 16.0) as u32;
            let sy = ((y as f32 + 0.5) * p.height() as f32 / 9.0) as u32;
            let c = p.pixel(sx.min(p.width() - 1), sy.min(p.height() - 1)).unwrap();
            sum += 0.299 * c.red() as f32 + 0.587 * c.green() as f32 + 0.114 * c.blue() as f32;
        }
    }
    sum / 144.0 > 150.0
}

/// Gaussian blur in place (sigma = radius / 2, as GSK does), approximated by three box blurs per
/// axis (O(1) per pixel regardless of radius), over all 4 premultiplied channels.
pub fn blur(p: &mut Pixmap, radius: f32) {
    let sigma = radius / 2.0;
    if sigma < 0.3 {
        return;
    }
    let boxes = box_sizes(sigma);
    let (w, h) = (p.width() as usize, p.height() as usize);
    let mut a: Vec<f32> = p.data().iter().map(|&v| v as f32).collect();
    let mut b = vec![0f32; a.len()];
    for r in boxes {
        box_pass(&a, &mut b, w, h, r, 4, w * 4, 4); // horizontal
        box_pass(&b, &mut a, h, w, r, w * 4, 4, 4); // vertical
    }
    let data = p.data_mut();
    for (i, px) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let alpha = a[i * 4 + 3].round().clamp(0.0, 255.0) as u8;
        for c in 0..3 {
            px[c] = (a[i * 4 + c].round().clamp(0.0, 255.0) as u8).min(alpha);
        }
        px[3] = alpha;
    }
}

/// `blur` of the alpha channel only (a quarter of the work), color left black: for shadows,
/// which are tinted from alpha afterwards.
pub fn blur_alpha(p: &mut Pixmap, radius: f32) {
    let sigma = radius / 2.0;
    if sigma < 0.3 {
        return;
    }
    let (w, h) = (p.width() as usize, p.height() as usize);
    let mut a: Vec<f32> = p.data().iter().skip(3).step_by(4).map(|&v| v as f32).collect();
    let mut b = vec![0f32; a.len()];
    for r in box_sizes(sigma) {
        box_pass(&a, &mut b, w, h, r, 1, w, 1);
        box_pass(&b, &mut a, h, w, r, w, 1, 1);
    }
    for (px, v) in p.data_mut().as_chunks_mut::<4>().0.iter_mut().zip(a) {
        *px = [0, 0, 0, v.round().clamp(0.0, 255.0) as u8];
    }
}

/// Box radii whose 3-pass sum approximates a gaussian with `sigma`.
fn box_sizes(sigma: f32) -> [usize; 3] {
    let ideal = (12.0 * sigma * sigma / 3.0 + 1.0).sqrt();
    let mut wl = ideal.floor() as i32;
    if wl % 2 == 0 {
        wl -= 1;
    }
    let wu = wl + 2;
    let m = ((12.0 * sigma * sigma - 3.0 * (wl * wl) as f32 - 12.0 * wl as f32 - 9.0) / (-4.0 * wl as f32 - 4.0)).round() as i32;
    let mut out = [0usize; 3];
    for (i, o) in out.iter_mut().enumerate() {
        *o = (((if (i as i32) < m { wl } else { wu }) - 1) / 2).max(0) as usize;
    }
    out
}

/// One running-sum box pass along lines of `len` samples; `step` between samples, `line` between lines.
#[allow(clippy::too_many_arguments)]
fn box_pass(src: &[f32], dst: &mut [f32], len: usize, lines: usize, r: usize, step: usize, line: usize, channels: usize) {
    let norm = 1.0 / (2 * r + 1) as f32;
    for l in 0..lines {
        for c in 0..channels {
            let base = l * line + c;
            let at = |i: isize| -> f32 { if i < 0 || i >= len as isize { 0.0 } else { src[base + i as usize * step] } };
            let mut acc: f32 = (-(r as isize)..=r as isize).map(at).sum();
            for i in 0..len {
                dst[base + i * step] = acc * norm;
                acc += at(i as isize + r as isize + 1) - at(i as isize - r as isize);
            }
        }
    }
}

/// Source rect (x, y, w, h) that covers a `dw`x`dh` box with an image (Gtk.ContentFit.COVER).
pub fn cover(iw: f32, ih: f32, dw: f32, dh: f32) -> (f32, f32, f32, f32) {
    let s = (dw / iw).max(dh / ih);
    let (w, h) = (dw / s, dh / s);
    ((iw - w) / 2.0, (ih - h) / 2.0, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_crops_the_long_side() {
        assert_eq!(cover(200.0, 100.0, 100.0, 100.0), (50.0, 0.0, 100.0, 100.0));
        assert_eq!(cover(100.0, 200.0, 100.0, 100.0), (0.0, 50.0, 100.0, 100.0));
    }

    #[test]
    fn blur_preserves_mass() {
        let mut p = Pixmap::new(21, 21).unwrap();
        p.data_mut()[(10 * 21 + 10) * 4 + 3] = 255;
        blur(&mut p, 4.0);
        let total: u32 = p.data().chunks(4).map(|c| c[3] as u32).sum();
        assert!((200..=300).contains(&total), "{total}");
    }
}
