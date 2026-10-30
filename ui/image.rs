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

/// Gaussian blur in place (sigma = radius / 2, as GSK does), approximated by three box blurs per
/// axis (O(1) per pixel regardless of radius), over all 4 premultiplied channels.
pub fn blur(p: &mut Pixmap, radius: f32) {
    let (w, h) = (p.width() as usize, p.height() as usize);
    let src: Vec<f32> = p.data().iter().map(|&v| v as f32).collect();
    let Some(a) = gaussian(src, w, h, 4, radius) else { return };
    for (px, v) in p.data_mut().as_chunks_mut::<4>().0.iter_mut().zip(a.as_chunks::<4>().0) {
        let alpha = (v[3] + 0.5) as u8; // saturating; round() is a libm call per sample here
        for c in 0..3 {
            px[c] = ((v[c] + 0.5) as u8).min(alpha);
        }
        px[3] = alpha;
    }
}

/// `blur` of the alpha channel only (a quarter of the work), color left black: for shadows,
/// which are tinted from alpha afterwards.
pub fn blur_alpha(p: &mut Pixmap, radius: f32) {
    let (w, h) = (p.width() as usize, p.height() as usize);
    let src: Vec<f32> = p.data().iter().skip(3).step_by(4).map(|&v| v as f32).collect();
    let Some(a) = gaussian(src, w, h, 1, radius) else { return };
    for (px, v) in p.data_mut().as_chunks_mut::<4>().0.iter_mut().zip(a) {
        *px = [0, 0, 0, (v + 0.5) as u8];
    }
}

/// Three box passes per axis. Box blurs commute, so all horizontal passes run on the transposed
/// image as column passes: every pass then walks memory row by row and vectorizes.
fn gaussian(src: Vec<f32>, w: usize, h: usize, ch: usize, radius: f32) -> Option<Vec<f32>> {
    let sigma = radius / 2.0;
    if sigma < 0.3 || w == 0 || h == 0 {
        return None;
    }
    // wide blurs (glows, big shadows) at reduced resolution: the result is smooth enough that
    // bilinear upsampling is invisible, and the work drops by f²
    let f = ((sigma / 4.0) as usize).clamp(1, 8);
    if f > 1 {
        let (sw, sh) = (w.div_ceil(f), h.div_ceil(f));
        let mut small = vec![0f32; sw * sh * ch];
        for (y, row) in src.chunks_exact(w * ch).enumerate() {
            let dst = &mut small[(y / f) * sw * ch..(y / f + 1) * sw * ch];
            for (x, px) in row.chunks_exact(ch).enumerate() {
                dst[(x / f) * ch..(x / f + 1) * ch].iter_mut().zip(px).for_each(|(d, s)| *d += s);
            }
        }
        // edge blocks may be partial: average over what they cover
        for (sy, row) in small.chunks_exact_mut(sw * ch).enumerate() {
            let rows = (h - sy * f).min(f);
            for (sx, px) in row.chunks_exact_mut(ch).enumerate() {
                let n = 1.0 / ((w - sx * f).min(f) * rows) as f32;
                px.iter_mut().for_each(|v| *v *= n);
            }
        }
        let small = blur_full(small, sw, sh, ch, sigma / f as f32);
        // bilinear: per output row, the two source rows mixed once, then sampled along x
        let taps: Vec<(usize, usize, f32)> = (0..w)
            .map(|x| {
                let fx = ((x as f32 + 0.5) / f as f32 - 0.5).max(0.0);
                let x0 = (fx as usize).min(sw - 1);
                (x0 * ch, (x0 + 1).min(sw - 1) * ch, fx - x0 as f32)
            })
            .collect();
        let mut out = vec![0f32; w * h * ch];
        let mut mixed = vec![0f32; sw * ch];
        for (y, orow) in out.chunks_exact_mut(w * ch).enumerate() {
            let fy = ((y as f32 + 0.5) / f as f32 - 0.5).max(0.0);
            let y0 = (fy as usize).min(sh - 1);
            let ty = fy - y0 as f32;
            let (r0, r1) = (&small[y0 * sw * ch..(y0 + 1) * sw * ch], &small[(y0 + 1).min(sh - 1) * sw * ch..((y0 + 1).min(sh - 1) + 1) * sw * ch]);
            mixed.iter_mut().zip(r0.iter().zip(r1)).for_each(|(m, (a, b))| *m = a + (b - a) * ty);
            for (px, &(a, b, tx)) in orow.chunks_exact_mut(ch).zip(&taps) {
                for c in 0..ch {
                    px[c] = mixed[a + c] + (mixed[b + c] - mixed[a + c]) * tx;
                }
            }
        }
        return Some(out);
    }
    Some(blur_full(src, w, h, ch, sigma))
}

fn blur_full(src: Vec<f32>, w: usize, h: usize, ch: usize, sigma: f32) -> Vec<f32> {
    let boxes = box_sizes(sigma);
    let mut a = transpose(&src, w, h, ch);
    let mut b = vec![0f32; a.len()];
    for r in boxes {
        vpass(&a, &mut b, w, r, h * ch);
        std::mem::swap(&mut a, &mut b);
    }
    let mut a = transpose(&a, h, w, ch);
    for r in boxes {
        vpass(&a, &mut b, h, r, w * ch);
        std::mem::swap(&mut a, &mut b);
    }
    a
}

/// `w`x`h` pixels of `ch` channels -> `h`x`w`, in cache-sized tiles.
fn transpose(src: &[f32], w: usize, h: usize, ch: usize) -> Vec<f32> {
    const T: usize = 32;
    let mut out = vec![0f32; src.len()];
    for ty in (0..h).step_by(T) {
        for tx in (0..w).step_by(T) {
            for y in ty..(ty + T).min(h) {
                for x in tx..(tx + T).min(w) {
                    let (s, d) = ((y * w + x) * ch, (x * h + y) * ch);
                    out[d..d + ch].copy_from_slice(&src[s..s + ch]);
                }
            }
        }
    }
    out
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

/// One running-sum box pass down the columns, a whole row of `row` samples at a time.
fn vpass(src: &[f32], dst: &mut [f32], h: usize, r: usize, row: usize) {
    let norm = 1.0 / (2 * r + 1) as f32;
    let mut acc = vec![0f32; row];
    for y in 0..=r.min(h - 1) {
        acc.iter_mut().zip(&src[y * row..(y + 1) * row]).for_each(|(a, s)| *a += s);
    }
    for y in 0..h {
        dst[y * row..(y + 1) * row].iter_mut().zip(&acc).for_each(|(d, a)| *d = a * norm);
        if y + r + 1 < h {
            let add = &src[(y + r + 1) * row..(y + r + 2) * row];
            acc.iter_mut().zip(add).for_each(|(a, s)| *a += s);
        }
        if y >= r {
            let sub = &src[(y - r) * row..(y - r + 1) * row];
            acc.iter_mut().zip(sub).for_each(|(a, s)| *a -= s);
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

