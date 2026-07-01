//! Image rendering primitives for keys.
//!
//! Everything a key shows is a 72×72 RGB tile. Icons are decoded with the
//! `image` crate and fit (aspect-preserving, centered on black) to 72×72 so the
//! Stream Deck crate's internal `resize_exact` doesn't stretch them. Widget
//! graphs (see `widgets.rs`) are hand-drawn directly onto these tiles — no
//! drawing crate, no fonts (§7/§8).

use std::path::Path;

use anyhow::{Context, Result};
use image::{imageops, DynamicImage, Rgb, Rgba, RgbImage, RgbaImage};

/// Stream Deck MK.2 key dimension (square), verified from the crate's `Kind`
/// image format table.
pub const KEY_SIZE: u32 = 72;

/// Uniform margin (px per side) left around trimmed (transparent) icons so they
/// all have consistent padding regardless of the artwork's own border.
const ICON_MARGIN: u32 = 8;
const ICON_FIT: u32 = KEY_SIZE - 2 * ICON_MARGIN;

pub type Tile = RgbImage;

/// A fully black (off) tile — what an unconfigured or blanked key shows.
pub fn blank_tile() -> Tile {
    RgbImage::from_pixel(KEY_SIZE, KEY_SIZE, Rgb([0, 0, 0]))
}

/// Decode an icon file and fit it to a 72×72 tile (aspect-preserving, centered
/// on a black background). Handles raster formats via the `image` crate and
/// **SVG** via `resvg` (so KDE/theme icons, which are usually SVG, work without
/// pre-rasterizing). Returns an error if the file can't be read/decoded.
pub fn load_icon(path: &Path) -> Result<Tile> {
    let is_svg = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("svg") || e.eq_ignore_ascii_case("svgz"));
    if is_svg {
        rasterize_svg(path)
    } else {
        let img = image::open(path)
            .with_context(|| format!("failed to load icon: {}", path.display()))?;
        Ok(fit_to_key(&img))
    }
}

/// Render an SVG straight to a 72×72 tile. resvg/tiny_skia already anti-alias,
/// so there's no supersampling — rendering at the target size is sharpest.
/// tiny_skia produces premultiplied RGBA, which composites over a black
/// background by simply taking the RGB channels (premultiplied `c` over black
/// = `c`).
fn rasterize_svg(path: &Path) -> Result<Tile> {
    /// Render resolution (the SVG is downscaled to ≤56px after trimming, so this
    /// gives headroom for a sharp result).
    const RENDER: u32 = 192;

    let data = std::fs::read(path).with_context(|| format!("reading SVG: {}", path.display()))?;
    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(&data, &opt)
        .with_context(|| format!("parsing SVG: {}", path.display()))?;

    // Render the SVG at native aspect into a pixmap (keeping its own padding as
    // transparency), then hand the RGBA to the shared content-aware fit so SVG
    // icons get trimmed + margined exactly like raster ones.
    let size = tree.size();
    let scale = (RENDER as f32 / size.width()).min(RENDER as f32 / size.height());
    let w = (size.width() * scale).ceil().max(1.0) as u32;
    let h = (size.height() * scale).ceil().max(1.0) as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h).context("failed to allocate SVG pixmap")?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    // tiny_skia gives premultiplied RGBA; un-premultiply to straight alpha so the
    // content-aware fit (which uses straight-alpha compositing) is correct.
    let px = pixmap.data();
    let mut rgba = RgbaImage::new(w, h);
    for (i, pixel) in rgba.pixels_mut().enumerate() {
        let o = i * 4;
        let a = px[o + 3];
        *pixel = if a == 0 {
            Rgba([0, 0, 0, 0])
        } else {
            let un = |c: u8| ((c as u16 * 255 + a as u16 / 2) / a as u16).min(255) as u8;
            Rgba([un(px[o]), un(px[o + 1]), un(px[o + 2]), a])
        };
    }
    Ok(fit_to_key(&DynamicImage::ImageRgba8(rgba)))
}

/// Fit any image into a centered 72×72 tile without distortion.
pub fn fit_to_key(img: &DynamicImage) -> Tile {
    let rgba = img.to_rgba8();
    // Transparent icons (PNG/SVG): trim to their actual content and re-fit with a
    // uniform margin, so every icon has consistent padding regardless of the
    // artwork's own border — and edge-to-edge art (e.g. the Steam circle) isn't
    // clipped by the key bezel. Opaque icons (no transparency to trim by, e.g. a
    // JPG with a solid background) fill the key as-is.
    if fully_opaque(&rgba) {
        fit_rgba(&rgba, KEY_SIZE)
    } else if let Some((x, y, w, h)) = content_bbox(&rgba) {
        let content = imageops::crop_imm(&rgba, x, y, w, h).to_image();
        fit_rgba(&content, ICON_FIT)
    } else {
        blank_tile() // fully transparent → nothing to show
    }
}

/// Resize `content` to fit (aspect-preserving) within a `box_size` square,
/// centre it on a black 72×72 tile, and composite its alpha over the black.
fn fit_rgba(content: &RgbaImage, box_size: u32) -> Tile {
    let scaled = DynamicImage::ImageRgba8(content.clone())
        .resize(box_size, box_size, imageops::FilterType::Lanczos3)
        .to_rgba8();
    let mut canvas = RgbaImage::from_pixel(KEY_SIZE, KEY_SIZE, Rgba([0, 0, 0, 255]));
    let x = (KEY_SIZE - scaled.width()) as i64 / 2;
    let y = (KEY_SIZE - scaled.height()) as i64 / 2;
    imageops::overlay(&mut canvas, &scaled, x, y);
    DynamicImage::ImageRgba8(canvas).to_rgb8()
}

/// True if every pixel is (near-)opaque — i.e. there's no transparency to trim.
fn fully_opaque(img: &RgbaImage) -> bool {
    img.pixels().all(|p| p[3] >= 250)
}

/// Bounding box `(x, y, w, h)` of pixels with meaningful opacity; `None` if the
/// image is fully transparent.
fn content_bbox(img: &RgbaImage) -> Option<(u32, u32, u32, u32)> {
    const ALPHA_MIN: u8 = 16;
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    let mut found = false;
    for (x, y, p) in img.enumerate_pixels() {
        if p[3] >= ALPHA_MIN {
            found = true;
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    found.then_some((x0, y0, x1 - x0 + 1, y1 - y0 + 1))
}

/// Convert a finished tile into the `DynamicImage` the device layer uploads.
pub fn tile_to_image(tile: Tile) -> DynamicImage {
    DynamicImage::ImageRgb8(tile)
}

// ---------------------------------------------------------------------------
// Drawing primitives (shared by the bundled error icon and the widgets).
// ---------------------------------------------------------------------------

/// Fill a rectangle (clipped to the tile bounds). `x`/`y` may be negative.
pub fn fill_rect(tile: &mut Tile, x: i32, y: i32, w: u32, h: u32, color: Rgb<u8>) {
    let x0 = x.max(0) as u32;
    let y0 = y.max(0) as u32;
    let x1 = ((x + w as i32).max(0) as u32).min(KEY_SIZE);
    let y1 = ((y + h as i32).max(0) as u32).min(KEY_SIZE);
    for py in y0..y1 {
        for px in x0..x1 {
            tile.put_pixel(px, py, color);
        }
    }
}

/// The bundled default failure-feedback icon (§9): a red tile with a white
/// exclamation mark, drawn from rectangles so v1 needs no font/text dependency
/// and no binary asset. Overridable via `error_icon` in the config.
pub fn default_error_icon() -> Tile {
    let mut tile = RgbImage::from_pixel(KEY_SIZE, KEY_SIZE, Rgb([180, 30, 30]));
    let white = Rgb([255, 255, 255]);
    // Exclamation bar: a centered vertical bar...
    let bar_w = 10;
    let cx = (KEY_SIZE / 2) as i32 - bar_w / 2;
    fill_rect(&mut tile, cx, 14, bar_w as u32, 30, white);
    // ...and the dot beneath it.
    fill_rect(&mut tile, cx, 50, bar_w as u32, 10, white);
    tile
}
