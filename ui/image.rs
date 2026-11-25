// Image decoding (png, jpeg) into premultiplied pixmaps, png encoding, gaussian blur, and box-fit scaling.

use tiny_skia::{IntSize, Pixmap};

// Pixel counts past which images are refused, per use: a few-KB file can claim gigapixels
// (decompression bomb).
/// Wallpapers: up to 8K.
pub const WALLPAPER_PIXELS: u64 = 8192 * 4096;
/// Thumbnails (clipboard images).
pub const THUMB_PIXELS: u64 = 4096 * 4096;
/// Color glyph bitmaps (a font's emoji are ~136px).
const GLYPH_PIXELS: u64 = 1024 * 1024;

fn too_big(w: usize, h: usize, max: u64) -> bool {
    (w as u64).checked_mul(h as u64).is_none_or(|n| n > max)
}

type Cursor<'a> = zune_jpeg::zune_core::bytestream::ZCursor<&'a [u8]>;
type PngDecoder<'a> = zune_png::PngDecoder<Cursor<'a>>;

/// Palette and tRNS expanded, 16-bit cut to 8: Luma, LumaA, RGB or RGBA.
fn png_reader(bytes: &[u8], max: u64) -> Option<PngDecoder<'_>> {
    let opts = zune_jpeg::zune_core::options::DecoderOptions::default()
        .png_set_strip_to_8bit(true)
        .set_max_width(1 << 15)
        .set_max_height(1 << 15);
    let mut d = zune_png::PngDecoder::new_with_options(Cursor::new(bytes), opts);
    d.decode_headers().ok()?;
    let (w, h) = d.dimensions()?;
    (!too_big(w, h, max)).then_some(d)
}

fn png_dims(d: &PngDecoder) -> (u32, u32) {
    d.dimensions().map_or((0, 0), |(w, h)| (w as u32, h as u32))
}

/// Decodes the frame into the front of `dst` (sized w*h*4), then expands it in place, back to
/// front, to premultiplied RGBA: no intermediate buffer.
fn png_frame(d: &mut PngDecoder, dst: &mut [u8]) -> Option<()> {
    let (w, h) = d.dimensions()?;
    let n = w * h;
    let ch = d.colorspace()?.num_components();
    if dst.len() != n * 4 {
        return None;
    }
    d.decode_into(dst).ok()?;
    match ch {
        4 => expand::<4>(dst, n, |p| [p[0], p[1], p[2], p[3]]),
        2 => expand::<2>(dst, n, |p| [p[0], p[0], p[0], p[1]]),
        3 => expand::<3>(dst, n, |p| [p[0], p[1], p[2], 255]),
        1 => expand::<1>(dst, n, |p| [p[0], p[0], p[0], 255]),
        _ => return None,
    }
    Some(())
}

/// `n` pixels of `CH` bytes at the front of `dst` widened to 4, last first so nothing unread is overwritten.
fn expand<const CH: usize>(dst: &mut [u8], n: usize, f: impl Fn(&[u8]) -> [u8; 4]) {
    for i in (0..n).rev() {
        let px = premultiply(f(&dst[i * CH..i * CH + CH]));
        dst[i * 4..i * 4 + 4].copy_from_slice(&px);
    }
}

/// A PNG of `w`×`h` 8-bit pixels of `CH` channels (3: RGB, 4: RGBA), `row(y, out)` writing each
/// row: Up-filtered (flat UI turns into zero runs) and compressed by fdeflate as it goes, with no
/// whole-image intermediate buffer.
pub fn encode_png<const CH: usize>(w: u32, h: u32, mut row: impl FnMut(u32, &mut [u8])) -> Vec<u8> {
    let stride = w as usize * CH;
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = [0u8; 13];
    ihdr[..4].copy_from_slice(&w.to_be_bytes());
    ihdr[4..8].copy_from_slice(&h.to_be_bytes());
    ihdr[8] = 8;
    ihdr[9] = if CH == 4 { 6 } else { 2 };
    png_chunk(&mut out, b"IHDR", &ihdr);
    let (mut cur, mut prev, mut line) = (vec![0u8; stride], vec![0u8; stride], vec![2u8; stride + 1]);
    let mut z = fdeflate::Compressor::new(Vec::with_capacity(stride * h as usize / 8)).expect("Vec writes can't fail");
    for y in 0..h {
        row(y, &mut cur);
        line[1..].iter_mut().zip(cur.iter().zip(&prev)).for_each(|(o, (c, p))| *o = c.wrapping_sub(*p));
        z.write_data(&line).expect("Vec writes can't fail");
        std::mem::swap(&mut cur, &mut prev);
    }
    png_chunk(&mut out, b"IDAT", &z.finish().expect("Vec writes can't fail"));
    png_chunk(&mut out, b"IEND", &[]);
    out
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32fast::hash(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// A color glyph's bitmap.
pub fn decode_png(bytes: &[u8]) -> Option<Pixmap> {
    let (buf, w, h) = png_pixels(bytes, GLYPH_PIXELS)?;
    Pixmap::from_vec(buf, IntSize::from_wh(w, h)?)
}

fn png_pixels(bytes: &[u8], max: u64) -> Option<(Vec<u8>, u32, u32)> {
    let mut reader = png_reader(bytes, max)?;
    let (w, h) = png_dims(&reader);
    let mut buf = vec![0; w as usize * h as usize * 4];
    png_frame(&mut reader, &mut buf)?;
    Some((buf, w, h))
}

fn jpeg_decoder(bytes: &[u8]) -> zune_jpeg::JpegDecoder<Cursor<'_>> {
    let opts = zune_jpeg::zune_core::options::DecoderOptions::default()
        .jpeg_set_out_colorspace(zune_jpeg::zune_core::colorspace::ColorSpace::RGBA)
        .set_max_width(1 << 15)
        .set_max_height(1 << 15);
    zune_jpeg::JpegDecoder::new_with_options(Cursor::new(bytes), opts)
}

/// Header read and size checked; also the EXIF orientation (1: upright).
fn jpeg_dims(d: &mut zune_jpeg::JpegDecoder<Cursor<'_>>, max: u64) -> Option<(u32, u32, u8)> {
    d.decode_headers().ok()?;
    let (w, h) = d.dimensions()?;
    let o = d.exif().map_or(1, |e| exif_orientation(e));
    (!too_big(w, h, max)).then_some((w as u32, h as u32, o))
}

/// Decodes into the front of `dst` (sized w*h*4), then widens it to RGBA in place: grayscale
/// JPEGs come out as Luma whatever colorspace was asked for. Opaque, so already premultiplied.
fn jpeg_frame(d: &mut zune_jpeg::JpegDecoder<Cursor<'_>>, dst: &mut [u8]) -> Option<()> {
    let (w, h) = d.dimensions()?;
    let n = w * h;
    let ch = d.output_colorspace()?.num_components();
    let dst = dst.get_mut(..n * 4)?;
    d.decode_into(dst).ok()?;
    match ch {
        4 => {}
        3 => expand::<3>(dst, n, |p| [p[0], p[1], p[2], 255]),
        2 => expand::<2>(dst, n, |p| [p[0], p[0], p[0], p[1]]),
        1 => expand::<1>(dst, n, |p| [p[0], p[0], p[0], 255]),
        _ => return None,
    }
    Some(())
}

/// The orientation tag (0x0112) of IFD0 in EXIF data starting at its TIFF header; 1 when absent.
fn exif_orientation(e: &[u8]) -> u8 {
    let le = match e.get(..2) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return 1,
    };
    let u16_at = |i: usize| e.get(i..i + 2).map(|b| if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) });
    let u32_at = |i: usize| e.get(i..i + 4).map(|b| if le { u32::from_le_bytes([b[0], b[1], b[2], b[3]]) } else { u32::from_be_bytes([b[0], b[1], b[2], b[3]]) });
    let Some(ifd) = u32_at(4).map(|o| o as usize) else { return 1 };
    let n = u16_at(ifd).unwrap_or(0) as usize;
    for i in 0..n.min(256) {
        let at = ifd + 2 + i * 12;
        if u16_at(at) == Some(0x0112) {
            // SHORT, value in the first two bytes of the entry's value field
            return u16_at(at + 8).filter(|o| (1..=8).contains(o)).unwrap_or(1) as u8;
        }
    }
    1
}

/// RGBA pixels turned upright for EXIF `orientation` (5-8 swap width and height).
fn orient(px: Vec<u8>, w: u32, h: u32, orientation: u8) -> (Vec<u8>, u32, u32) {
    if !(2..=8).contains(&orientation) {
        return (px, w, h);
    }
    let (w, h) = (w as usize, h as usize);
    let (ow, oh) = if orientation >= 5 { (h, w) } else { (w, h) };
    let mut out = vec![0u8; px.len()];
    for y in 0..oh {
        for x in 0..ow {
            let (sx, sy) = match orientation {
                2 => (w - 1 - x, y),
                3 => (w - 1 - x, h - 1 - y),
                4 => (x, h - 1 - y),
                5 => (y, x),
                6 => (y, h - 1 - x),
                7 => (w - 1 - y, h - 1 - x),
                _ => (w - 1 - y, x),
            };
            out[(y * ow + x) * 4..(y * ow + x) * 4 + 4].copy_from_slice(&px[(sy * w + sx) * 4..(sy * w + sx) * 4 + 4]);
        }
    }
    (out, ow as u32, oh as u32)
}

/// Upright premultiplied RGBA pixels and their size; at most `max` pixels.
fn decode_pixels(bytes: &[u8], max: u64) -> Option<(Vec<u8>, u32, u32)> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => png_pixels(bytes, max),
        [0xff, 0xd8, ..] => {
            let mut d = jpeg_decoder(bytes);
            let (w, h, o) = jpeg_dims(&mut d, max)?;
            let mut px = vec![0; w as usize * h as usize * 4];
            jpeg_frame(&mut d, &mut px)?;
            Some(orient(px, w, h, o))
        }
        _ => None,
    }
}

/// The upright size (EXIF orientation 5-8 swaps it) from the header alone, of any size.
pub fn dims(bytes: &[u8]) -> Option<(u32, u32)> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] if bytes.len() >= 24 => Some((u32::from_be_bytes(bytes[16..20].try_into().ok()?), u32::from_be_bytes(bytes[20..24].try_into().ok()?))),
        [0xff, 0xd8, ..] => {
            let (w, h, o) = jpeg_dims(&mut jpeg_decoder(bytes), u64::MAX)?;
            Some(if o >= 5 { (h, w) } else { (w, h) })
        }
        _ => None,
    }
}

/// Decodes upright as premultiplied RGBA (at most `max` pixels) into the `w`x`h` buffer `alloc`
/// provides, shrunk to the size `fit` picks for the upright size when that is smaller. Straight
/// into the buffer, with no intermediate copy, when nothing needs transforming.
pub fn decode_into<T: AsMut<[u8]>>(
    bytes: &[u8], max: u64, fit: impl Fn(u32, u32) -> (u32, u32), alloc: impl FnOnce(u32, u32) -> Option<T>,
) -> Option<T> {
    let target = |w: u32, h: u32| {
        let (tw, th) = fit(w, h);
        (tw.clamp(1, w), th.clamp(1, h))
    };
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => {
            let mut reader = png_reader(bytes, max)?;
            let (w, h) = png_dims(&reader);
            if target(w, h) == (w, h) {
                let mut t = alloc(w, h)?;
                png_frame(&mut reader, t.as_mut())?;
                return Some(t);
            }
        }
        [0xff, 0xd8, ..] => {
            let mut d = jpeg_decoder(bytes);
            let (w, h, o) = jpeg_dims(&mut d, max)?;
            if o == 1 && target(w, h) == (w, h) {
                let mut t = alloc(w, h)?;
                jpeg_frame(&mut d, t.as_mut())?;
                return Some(t);
            }
        }
        _ => return None,
    };
    let (px, w, h) = decode_pixels(bytes, max)?;
    let (tw, th) = target(w, h);
    let mut t = alloc(tw, th)?;
    let out = t.as_mut();
    if (tw, th) == (w, h) {
        out.get_mut(..px.len())?.copy_from_slice(&px);
    } else {
        downscale(&px, w as usize, h as usize, out, tw as usize, th as usize);
    }
    Some(t)
}

/// RGBA `src` (`sw`x`sh`) shrunk into `dst` (`dw`x`dh`, no bigger): every output pixel averages
/// the source pixels it covers (a box filter: no aliasing at any factor, unlike bicubic).
pub fn downscale(src: &[u8], sw: usize, sh: usize, dst: &mut [u8], dw: usize, dh: usize) {
    if dw == 0 || dh == 0 || dw > sw || dh > sh || src.len() < sw * sh * 4 || dst.len() < dw * dh * 4 {
        return;
    }
    let xs: Vec<usize> = (0..=dw).map(|x| x * sw / dw).collect();
    let q = sw / dw;
    // an output row's source rows summed per column first (a straight add over whole rows, which
    // vectorizes), then each output pixel's span of columns once
    let mut cols = vec![0u32; sw * 4];
    for y in 0..dh {
        let (y0, y1) = (y * sh / dh, (y + 1) * sh / dh);
        cols.fill(0);
        for row in src[y0 * sw * 4..y1 * sw * 4].chunks_exact(sw * 4) {
            cols.iter_mut().zip(row).for_each(|(c, &v)| *c += v as u32);
        }
        let rows = y1 - y0;
        // spans are q or q + 1 columns wide: a reciprocal per pixel count, not a division per channel
        let magic = [q, q + 1].map(|s| recip(s * rows));
        let exact = (q + 1) * rows < 1 << 16;
        let cols4 = cols.as_chunks::<4>().0;
        let out = &mut dst[y * dw * 4..(y + 1) * dw * 4];
        for (x, o) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let a = cols4[xs[x]..xs[x + 1]].iter().fold([0u32; 4], |a, c| [a[0] + c[0], a[1] + c[1], a[2] + c[2], a[3] + c[3]]);
            let span = xs[x + 1] - xs[x];
            let (n, m) = ((span * rows) as u32, magic[span - q]);
            for c in 0..4 {
                let v = a[c] + n / 2;
                o[c] = if exact { div_recip(v, m) as u8 } else { (v / n) as u8 };
            }
        }
    }
}

/// 2^40 / n rounded up: `div_recip(v, recip(n))` is v / n for every v < 256n while n <= 2^16 (the
/// estimate's error, under 256n / 2^40, stays below 1 / n).
#[inline(always)]
fn recip(n: usize) -> u64 {
    (1u64 << 40) / n as u64 + 1
}

#[inline(always)]
fn div_recip(v: u32, m: u64) -> u32 {
    ((v as u64 * m) >> 40) as u32
}

/// Coverage along one axis of `len` px of the span [start, end) blurred like `blur` by `radius`:
/// how a blurred box fades out at its edges.
pub fn blur_profile(len: usize, start: f32, end: f32, radius: f32) -> Vec<f32> {
    let mut a: Vec<f32> = (0..len).map(|i| if (start..end).contains(&(i as f32 + 0.5)) { 1.0 } else { 0.0 }).collect();
    let sigma = radius / 2.0;
    if sigma < 0.3 || len == 0 {
        return a;
    }
    let mut b = vec![0f32; len];
    for r in box_sizes(sigma) {
        vpass(&a, &mut b, len, r, 1);
        std::mem::swap(&mut a, &mut b);
    }
    a
}

fn premultiply(p: [u8; 4]) -> [u8; 4] {
    let a = p[3] as u16;
    if a == 255 {
        return p;
    }
    let m = |c: u8| ((c as u16 * a + 127) / 255) as u8;
    [m(p[0]), m(p[1]), m(p[2]), p[3]]
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

/// `blur` of the alpha channel only (a quarter of the work), each pixel then `tint`ed from its
/// blurred alpha: for shadows (colored from alpha in the same pass).
pub fn blur_alpha(p: &mut Pixmap, radius: f32, tint: impl Fn(u8) -> [u8; 4]) {
    let (w, h) = (p.width() as usize, p.height() as usize);
    let src: Vec<f32> = p.data().iter().skip(3).step_by(4).map(|&v| v as f32).collect();
    let Some(a) = gaussian(src, w, h, 1, radius) else {
        p.data_mut().as_chunks_mut::<4>().0.iter_mut().for_each(|px| *px = tint(px[3]));
        return;
    };
    for (px, v) in p.data_mut().as_chunks_mut::<4>().0.iter_mut().zip(a) {
        *px = tint((v + 0.5) as u8);
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
        // a block row's source rows summed first (whole rows, vectorized), then its columns in
        // blocks; the samples are whole numbers, so the f32 sums are exact in any order
        let mut sum = vec![0f32; w * ch];
        for (sy, rows) in src.chunks(f * w * ch).enumerate() {
            sum.fill(0.0);
            for row in rows.chunks_exact(w * ch) {
                sum.iter_mut().zip(row).for_each(|(d, s)| *d += s);
            }
            let dst = &mut small[sy * sw * ch..(sy + 1) * sw * ch];
            for (d, cols) in dst.chunks_exact_mut(ch).zip(sum.chunks(f * ch)) {
                for px in cols.chunks_exact(ch) {
                    d.iter_mut().zip(px).for_each(|(d, s)| *d += s);
                }
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
        drop(src);
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
    let mut a = vec![0f32; src.len()];
    transpose(&src, &mut a, w, h, ch);
    drop(src);
    let mut b = vec![0f32; a.len()];
    for r in boxes {
        vpass(&a, &mut b, w, r, h * ch);
        std::mem::swap(&mut a, &mut b);
    }
    transpose(&a, &mut b, h, w, ch);
    std::mem::swap(&mut a, &mut b);
    for r in boxes {
        vpass(&a, &mut b, h, r, w * ch);
        std::mem::swap(&mut a, &mut b);
    }
    a
}

/// `w`x`h` pixels of `ch` channels -> `h`x`w` into `out`, in cache-sized tiles.
fn transpose(src: &[f32], out: &mut [f32], w: usize, h: usize, ch: usize) {
    const T: usize = 32;
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
    fn refuses_bombs() {
        assert!(!too_big(8192, 4096, WALLPAPER_PIXELS));
        assert!(too_big(8193, 4096, WALLPAPER_PIXELS));
        assert!(too_big(usize::MAX, 2, WALLPAPER_PIXELS));
    }

    #[test]
    fn exif_orientation_and_rotation() {
        // little endian, IFD0 at 8 with one entry: orientation SHORT 6
        let e = [b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0, 0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0];
        assert_eq!(exif_orientation(&e), 6);
        assert_eq!(exif_orientation(b"MM"), 1);
        // 2x1 [a b] rotated clockwise is 1x2 [a; b]
        let (px, w, h) = orient(vec![1, 1, 1, 1, 2, 2, 2, 2], 2, 1, 6);
        assert_eq!((w, h), (1, 2));
        assert_eq!(px, vec![1, 1, 1, 1, 2, 2, 2, 2]);
        let (px, ..) = orient(vec![1, 1, 1, 1, 2, 2, 2, 2], 2, 1, 8);
        assert_eq!(px, vec![2, 2, 2, 2, 1, 1, 1, 1]);
    }

    #[test]
    fn downscale_averages() {
        let src = [0, 0, 0, 255, 200, 200, 200, 255, 100, 100, 100, 255, 100, 100, 100, 255];
        let mut dst = [0u8; 4];
        downscale(&src, 2, 2, &mut dst, 1, 1);
        assert_eq!(dst, [100, 100, 100, 255]);
    }

    #[test]
    fn reciprocal_division_is_exact() {
        for n in 1..1u32 << 16 {
            let m = recip(n as usize);
            // the quotient's steps: each multiple of n, the value just below it, and the largest v
            for v in (0..256 * n).step_by(n as usize).flat_map(|k| [k.saturating_sub(1), k, k + n / 2]).chain([256 * n - 1]) {
                assert_eq!(div_recip(v, m), v / n, "{v} / {n}");
            }
        }
    }

    #[test]
    fn downscale_matches_per_pixel_box() {
        let (sw, sh) = (37, 23);
        let src: Vec<u8> = (0..sw * sh * 4).map(|i| (i * 7919 % 251) as u8).collect();
        for (dw, dh) in [(37, 23), (10, 7), (5, 23), (1, 1), (36, 22)] {
            let mut got = vec![0u8; dw * dh * 4];
            downscale(&src, sw, sh, &mut got, dw, dh);
            for y in 0..dh {
                for x in 0..dw {
                    let (x0, x1, y0, y1) = (x * sw / dw, (x + 1) * sw / dw, y * sh / dh, (y + 1) * sh / dh);
                    let n = ((x1 - x0) * (y1 - y0)) as u32;
                    for c in 0..4 {
                        let s: u32 = (y0..y1).flat_map(|sy| (x0..x1).map(move |sx| (sy, sx))).map(|(sy, sx)| src[(sy * sw + sx) * 4 + c] as u32).sum();
                        assert_eq!(got[(y * dw + x) * 4 + c], ((s + n / 2) / n) as u8, "{dw}x{dh} at {x},{y}");
                    }
                }
            }
        }
    }

    #[test]
    fn expands_in_place_premultiplied() {
        // two gray+alpha pixels at the front, widened to RGBA
        let mut buf = [200, 255, 100, 0, 0, 0, 0, 0];
        expand::<2>(&mut buf, 2, |p| [p[0], p[0], p[0], p[1]]);
        assert_eq!(buf, [200, 200, 200, 255, 0, 0, 0, 0]);
    }

    #[test]
    fn blur_preserves_mass() {
        let mut p = Pixmap::new(21, 21).unwrap();
        p.data_mut()[(10 * 21 + 10) * 4 + 3] = 255;
        blur(&mut p, 4.0);
        let total: u32 = p.data().chunks(4).map(|c| c[3] as u32).sum();
        assert!((200..=300).contains(&total), "{total}");
    }

    #[test]
    fn png_round_trips() {
        let px: [[u8; 4]; 6] = [[255, 0, 0, 255], [0, 255, 0, 128], [0, 0, 255, 0], [10, 20, 30, 255], [10, 20, 30, 255], [200, 100, 50, 64]];
        let png = encode_png::<4>(3, 2, |y, row| row.copy_from_slice(px[y as usize * 3..][..3].as_flattened()));
        let (got, w, h) = decode_pixels(&png, 100).unwrap();
        assert_eq!((w, h), (3, 2));
        assert_eq!(got, px.map(premultiply).as_flattened());
        let png = encode_png::<3>(3, 2, |y, row| row.copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, y as u8]));
        let (got, _, _) = decode_pixels(&png, 100).unwrap();
        assert_eq!(got, [1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 0, 255, 1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 1, 255]);
        assert!(decode_pixels(&png, 5).is_none(), "over the pixel cap");
    }

    /// Hand-built PNGs: what the decoder must expand (palette + tRNS, 16-bit gray).
    fn raw_png(ihdr: [u8; 5], extra: &[(&[u8; 4], &[u8])], rows: &[u8], w: u32, h: u32) -> Vec<u8> {
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut hdr = [0u8; 13];
        hdr[..4].copy_from_slice(&w.to_be_bytes());
        hdr[4..8].copy_from_slice(&h.to_be_bytes());
        hdr[8..].copy_from_slice(&ihdr);
        png_chunk(&mut out, b"IHDR", &hdr);
        for (k, d) in extra {
            png_chunk(&mut out, k, d);
        }
        png_chunk(&mut out, b"IDAT", &fdeflate::compress_to_vec(rows));
        png_chunk(&mut out, b"IEND", &[]);
        out
    }

    #[test]
    fn png_expands_palette_and_16_bit() {
        let pal = raw_png([8, 3, 0, 0, 0], &[(b"PLTE", &[255, 0, 0, 0, 0, 255]), (b"tRNS", &[0])], &[0, 0, 1], 2, 1);
        assert_eq!(decode_pixels(&pal, 100).unwrap().0, [0, 0, 0, 0, 0, 0, 255, 255]);
        let gray = raw_png([16, 0, 0, 0, 0], &[], &[0, 0x80, 0xff, 0xff, 0x00], 2, 1);
        assert_eq!(decode_pixels(&gray, 100).unwrap().0, [128, 128, 128, 255, 255, 255, 255, 255]);
    }
}
