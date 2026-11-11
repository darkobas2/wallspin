//! Clock overlay rendered in-process (replaces Variety's per-minute ImageMagick runs).
//!
//! Wallpaper change: decode once, cover-resize to the screen canvas, keep the result only
//! as a compressed JPEG (~2 MB). Each minute: decode that small base, draw text, encode,
//! write a new file. Every buffer is dropped right after use.
use crate::config::ClockConfig;
use ab_glyph::{Font, FontArc, PxScale, ScaleFont, point};
use anyhow::{Context, Result, bail};
use image::{ImageReader, Limits, RgbImage, codecs::jpeg::JpegEncoder};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// Size of the image GNOME will show 1:1, and where the primary monitor sits in it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub canvas: (u32, u32),
    pub primary: Rect,
}

/// Live monitor arrangement: bounding-box canvas and every monitor in canvas
/// coordinates, primary first.
#[derive(Debug, Clone, PartialEq)]
pub struct Screens {
    pub canvas: (u32, u32),
    pub rects: Vec<Rect>,
}

type Props = HashMap<String, zbus::OwnedVariant>;
type Spec = (String, String, String, String);
type Mode = (String, i32, i32, f64, f64, Vec<f64>, Props);
type Monitor = (Spec, Vec<Mode>, Props);
type Logical = (i32, i32, f64, u32, bool, Vec<Spec>, Props);

/// Ask Mutter for the live monitor layout (works on Wayland and X11).
pub async fn detect_screens(conn: &zbus::Connection) -> Result<Screens> {
    let reply = conn
        .call_method(
            Some("org.gnome.Mutter.DisplayConfig"),
            "/org/gnome/Mutter/DisplayConfig",
            Some("org.gnome.Mutter.DisplayConfig"),
            "GetCurrentState",
            &(),
        )
        .await?;
    let (_serial, monitors, logical, props): (u32, Vec<Monitor>, Vec<Logical>, Props) = reply.body().deserialize()?;
    // layout-mode 1 = logical (positions in scaled px), 2 = physical.
    let logical_layout = props.get("layout-mode").and_then(|v| v.downcast_ref::<u32>().ok()).unwrap_or(1) == 1;

    let mut rects: Vec<(Rect, bool)> = Vec::new();
    for (x, y, scale, transform, primary, specs, _) in &logical {
        let Some(spec) = specs.first() else { continue };
        let Some((_, modes, _)) = monitors.iter().find(|m| &m.0 == spec) else { continue };
        let current = |m: &&Mode| m.6.get("is-current").and_then(|v| v.downcast_ref::<bool>().ok()) == Some(true);
        let Some(mode) = modes.iter().find(current) else { continue };
        let (mut w, mut h) = (mode.1 as f64, mode.2 as f64);
        if logical_layout && *scale > 0.0 {
            w /= scale;
            h /= scale;
        }
        if transform % 2 == 1 {
            std::mem::swap(&mut w, &mut h);
        }
        rects.push((Rect { x: *x, y: *y, w: w.round() as u32, h: h.round() as u32 }, *primary));
    }
    if rects.is_empty() {
        bail!("no active monitors reported");
    }
    rects.sort_by_key(|r| (!r.1, r.0.x, r.0.y)); // primary first, then left-to-right
    let x0 = rects.iter().map(|r| r.0.x).min().unwrap();
    let y0 = rects.iter().map(|r| r.0.y).min().unwrap();
    let x1 = rects.iter().map(|r| r.0.x + r.0.w as i32).max().unwrap();
    let y1 = rects.iter().map(|r| r.0.y + r.0.h as i32).max().unwrap();
    Ok(Screens {
        canvas: ((x1 - x0) as u32, (y1 - y0) as u32),
        rects: rects.into_iter().map(|(r, _)| Rect { x: r.x - x0, y: r.y - y0, ..r }).collect(),
    })
}

#[derive(Clone)]
pub struct Fonts {
    time: FontArc,
    date: FontArc,
}

/// Resolve a fontconfig pattern (e.g. "Roboto Condensed:style=Light") to a font.
fn load_font(pattern: &str) -> Result<FontArc> {
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}", pattern]).output()?;
    let file = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if file.is_empty() {
        bail!("fc-match found nothing for {pattern:?}");
    }
    let bytes = std::fs::read(&file).with_context(|| format!("reading font {file}"))?;
    FontArc::try_from_vec(bytes).with_context(|| format!("parsing font {file}"))
}

impl Fonts {
    pub fn load(cfg: &ClockConfig) -> Result<Self> {
        Ok(Self { time: load_font(&cfg.font)?, date: load_font(&cfg.date_font)? })
    }
}

/// Composed screen-sized wallpaper kept as JPEG bytes (for per-minute clock redraws).
pub struct Base {
    pub layout: Layout,
    jpeg: Arc<Vec<u8>>,
}

/// Images above this many pixels are refused (8K is ~33 MP).
pub const MAX_PIXELS: u64 = 80_000_000;

/// Decode `src` and cover-resize it to exactly `w`x`h` (center crop), keeping peak memory low:
/// JPEGs are decoded directly at reduced size (DCT scaling 1/2..1/8), and RGBA sources are
/// resized as RGBA and only converted to RGB at screen size.
fn load_cover(src: &Path, w: u32, h: u32) -> Result<RgbImage> {
    use fast_image_resize::{PixelType, images::Image};
    let (sw, sh) = imagesize::size(src).map(|d| (d.width as u64, d.height as u64)).unwrap_or((0, 0));
    if sw * sh > MAX_PIXELS {
        bail!("{} is {sw}x{sh}; too large", src.display());
    }
    let (buf, iw, ih, pt) = match decode_jpeg_scaled(src, w, h) {
        Some(r) => r?,
        None => decode_generic(src)?,
    };
    let src_img = Image::from_vec_u8(iw, ih, buf, pt)?;
    let mut dst = Image::new(w, h, pt);
    let opts = fast_image_resize::ResizeOptions::new()
        .resize_alg(fast_image_resize::ResizeAlg::Convolution(fast_image_resize::FilterType::Lanczos3))
        .fit_into_destination(Some((0.5, 0.5)));
    fast_image_resize::Resizer::new().resize(&src_img, &mut dst, &opts)?;
    drop(src_img); // the big decode — free it before anything else
    let raw = dst.into_vec();
    let rgb = if pt == PixelType::U8x4 {
        raw.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect()
    } else {
        raw
    };
    RgbImage::from_raw(w, h, rgb).context("resize buffer")
}

/// JPEG via DCT-domain downscaling to the smallest size that still covers `w`x`h`.
/// None = not a (plain RGB) JPEG; use the generic path.
fn decode_jpeg_scaled(src: &Path, w: u32, h: u32) -> Option<Result<(Vec<u8>, u32, u32, fast_image_resize::PixelType)>> {
    let file = std::fs::File::open(src).ok()?;
    let mut d = jpeg_decoder::Decoder::new(std::io::BufReader::new(file));
    d.read_info().ok()?;
    let info = d.info()?;
    if info.pixel_format != jpeg_decoder::PixelFormat::RGB24 {
        return None;
    }
    let (iw, ih) = (info.width as f64, info.height as f64);
    let s = (w as f64 / iw).max(h as f64 / ih).min(1.0); // cover scale, never upscale here
    let (rw, rh) = ((iw * s).ceil() as u16, (ih * s).ceil() as u16);
    Some((|| {
        let (ow, oh) = d.scale(rw.max(1), rh.max(1))?;
        let px = d.decode()?;
        Ok((px, ow as u32, oh as u32, fast_image_resize::PixelType::U8x3))
    })())
}

fn decode_generic(src: &Path) -> Result<(Vec<u8>, u32, u32, fast_image_resize::PixelType)> {
    use fast_image_resize::PixelType;
    let mut limits = Limits::default();
    limits.max_image_width = Some(20_000);
    limits.max_image_height = Some(20_000);
    limits.max_alloc = Some(512 * 1024 * 1024);
    let mut reader = ImageReader::open(src)?.with_guessed_format()?;
    reader.limits(limits);
    let img = reader.decode().with_context(|| format!("decoding {}", src.display()))?;
    let (iw, ih) = (img.width(), img.height());
    Ok(match img {
        image::DynamicImage::ImageRgb8(i) => (i.into_raw(), iw, ih, PixelType::U8x3),
        image::DynamicImage::ImageRgba8(i) => (i.into_raw(), iw, ih, PixelType::U8x4),
        other => (other.into_rgb8().into_raw(), iw, ih, PixelType::U8x3),
    })
}

/// Paint `images[i]` into `rects[i]` (images repeat if fewer than rects).
/// Returns the base (compressed, kept for clock ticks) and the raw canvas.
pub fn compose(images: &[PathBuf], canvas: (u32, u32), rects: &[Rect], layout: Layout) -> Result<(Base, RgbImage)> {
    anyhow::ensure!(!images.is_empty(), "no images");
    let mut out = RgbImage::new(canvas.0, canvas.1);
    for (i, r) in rects.iter().enumerate() {
        let tile = load_cover(&images[i % images.len()], r.w, r.h)?;
        image::imageops::replace(&mut out, &tile, r.x as i64, r.y as i64);
    }
    let jpeg = encode(&out, 95)?;
    Ok((Base { layout, jpeg: Arc::new(jpeg) }, out))
}

fn encode(img: &RgbImage, quality: u8) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(img.len() / 8);
    JpegEncoder::new_with_quality(&mut buf, quality).encode_image(img)?;
    Ok(buf)
}

impl Base {
    pub fn decode(&self) -> Result<RgbImage> {
        Ok(image::load_from_memory_with_format(&self.jpeg, image::ImageFormat::Jpeg)?.into_rgb8())
    }
}

/// Optionally draw time + date onto `img` (screen-sized), encode, rewrite `out` in place.
pub fn render_to(mut img: RgbImage, layout: Layout, clock: Option<(&Fonts, &ClockConfig)>, out: &Path) -> Result<()> {
    if let Some((fonts, cfg)) = clock {
        draw_clock(&mut img, layout, fonts, cfg);
    }
    let bytes = encode(&img, 90)?;
    drop(img);
    // In place on purpose (no tmp+rename): close() yields the CHANGES_DONE_HINT that makes
    // GNOME Shell purge its cached texture for this URI and reload.
    std::fs::write(out, &bytes)?;
    Ok(())
}

fn draw_clock(img: &mut RgbImage, layout: Layout, fonts: &Fonts, cfg: &ClockConfig) {
    let now = chrono::Local::now();
    let time = now.format(&cfg.time_format).to_string();
    let date = now.format(&cfg.date_format).to_string();

    // Variety: ImageMagick at 100 dpi, offsets designed for 1080p; scale with the primary height.
    let s = layout.primary.h as f32 / 1080.0;
    let time_px = cfg.size * 100.0 / 72.0 * s;
    let date_px = cfg.date_size * 100.0 / 72.0 * s;
    let margin = 60.0 * s;
    let p = layout.primary;
    let (left, right) = (p.x as f32 + margin, (p.x + p.w as i32) as f32 - margin);
    let right_align = cfg.position.ends_with("right");
    let (time_base, date_base) = if cfg.position.starts_with("top") {
        let t = p.y as f32 + margin + time_px * 0.8;
        (t, t + date_px * 1.3)
    } else {
        let bottom = (p.y + p.h as i32) as f32;
        (bottom - 110.0 * s, bottom - 60.0 * s)
    };

    for (text, font, px, base) in [(&time, &fonts.time, time_px, time_base), (&date, &fonts.date, date_px, date_base)] {
        let w = text_width(font, px, text);
        let x = if right_align { right - w } else { left };
        if cfg.shadow {
            draw_text(img, font, px, text, x + 2.0 * s, base + 2.0 * s, [0, 0, 0], 0x44 as f32 / 255.0);
        }
        draw_text(img, font, px, text, x, base, [255, 255, 255], 1.0);
    }
}

fn text_width(font: &FontArc, px: f32, s: &str) -> f32 {
    let sf = font.as_scaled(PxScale::from(px));
    let mut w = 0.0;
    let mut prev = None;
    for c in s.chars() {
        let id = sf.glyph_id(c);
        if let Some(p) = prev {
            w += sf.kern(p, id);
        }
        w += sf.h_advance(id);
        prev = Some(id);
    }
    w
}

fn draw_text(img: &mut RgbImage, font: &FontArc, px: f32, text: &str, x: f32, baseline: f32, color: [u8; 3], alpha: f32) {
    let (iw, ih) = (img.width() as i32, img.height() as i32);
    for g in layout_glyphs(font, px, text, x, baseline) {
        let Some(outline) = font.outline_glyph(g) else { continue };
        let b = outline.px_bounds();
        outline.draw(|gx, gy, cov| {
            let (px_x, px_y) = (b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32);
            if px_x < 0 || px_y < 0 || px_x >= iw || px_y >= ih {
                return;
            }
            let a = (cov * alpha).clamp(0.0, 1.0);
            let pix = img.get_pixel_mut(px_x as u32, px_y as u32);
            for i in 0..3 {
                pix.0[i] = (pix.0[i] as f32 * (1.0 - a) + color[i] as f32 * a).round() as u8;
            }
        });
    }
}

/// Glyphs for `s` positioned from `x` on `baseline`, with kerning.
fn layout_glyphs(font: &FontArc, px: f32, s: &str, x: f32, baseline: f32) -> Vec<ab_glyph::Glyph> {
    let scale = PxScale::from(px);
    let sf = font.as_scaled(scale);
    let mut caret = x;
    let mut prev = None;
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        let id = sf.glyph_id(c);
        if let Some(p) = prev {
            caret += sf.kern(p, id);
        }
        out.push(id.with_scale_and_position(scale, point(caret, baseline)));
        caret += sf.h_advance(id);
        prev = Some(id);
    }
    out
}
