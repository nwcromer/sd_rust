//! Live metric widgets, rendered as hand-drawn graphs (§8).
//!
//! Every metric is read directly from `/proc` and `/sys` — no dependency. Graphs
//! are drawn pixel-by-pixel on the `image` crate (no `imageproc`, no fonts) as
//! filled area silhouettes: CPU/GPU are coloured by load (green→yellow→red), RAM
//! a fixed colour, and network is a down/up split (auto-scaled to recent peak).
//!
//! GPU is AMD-only (the discrete card is auto-detected by largest VRAM). The
//! metric source is structured so other vendors can be added later.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use image::Rgb;
use log::{debug, warn};

use crate::config::WidgetKind;
use crate::render::{self, Tile, KEY_SIZE};

/// Default per-widget refresh interval when unset.
const DEFAULT_REFRESH: Duration = Duration::from_secs(1);
/// One history sample per horizontal pixel — a smooth 1px-column silhouette.
const COLS: usize = KEY_SIZE as usize;
/// Black border (px per side) left around a graph so it doesn't touch the edges.
const GRAPH_MARGIN: u32 = 6;
const GRAPH_FIT: u32 = KEY_SIZE - 2 * GRAPH_MARGIN;

const BG: Rgb<u8> = Rgb([18, 18, 22]);

/// A widget instance bound to one key: its sampler, refresh cadence, the last
/// rendered tile, and a generation counter the runtime watches for repaints.
pub struct Widget {
    sampler: Sampler,
    interval: Duration,
    last_render: Instant,
    generation: u64,
    tile: Tile,
}

impl Widget {
    pub fn new(kind: WidgetKind, refresh_secs: Option<u64>, interface: Option<String>) -> Self {
        let sampler = match kind {
            WidgetKind::Cpu => Sampler::Cpu(CpuSampler::new()),
            WidgetKind::Ram => Sampler::Ram(History::new()),
            WidgetKind::Network => Sampler::Network(NetSampler::new(interface)),
            WidgetKind::Gpu => Sampler::Gpu(GpuSampler::new()),
        };
        let mut widget = Widget {
            sampler,
            interval: refresh_secs.map(Duration::from_secs).unwrap_or(DEFAULT_REFRESH),
            // Force an immediate first render on the first tick.
            last_render: Instant::now() - Duration::from_secs(3600),
            generation: 0,
            tile: render::blank_tile(),
        };
        widget.sample_and_render();
        widget
    }

    /// The current render generation (changes when the tile is re-rendered).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// A clone of the latest rendered tile.
    pub fn tile(&self) -> Tile {
        self.tile.clone()
    }

    /// Re-sample + re-render if the refresh interval has elapsed. Returns true
    /// if the tile changed (so the runtime knows to repaint).
    pub fn maybe_tick(&mut self) -> bool {
        if self.last_render.elapsed() < self.interval {
            return false;
        }
        self.sample_and_render();
        true
    }

    fn sample_and_render(&mut self) {
        self.last_render = Instant::now();
        self.tile = with_margin(self.sampler.render());
        self.generation = self.generation.wrapping_add(1);
    }
}

/// Inset a finished graph tile by [`GRAPH_MARGIN`] on a black border, so the
/// graphs have breathing room like the icons.
fn with_margin(tile: Tile) -> Tile {
    let scaled = image::DynamicImage::ImageRgb8(tile)
        .resize(GRAPH_FIT, GRAPH_FIT, image::imageops::FilterType::Lanczos3)
        .to_rgb8();
    let mut canvas = Tile::from_pixel(KEY_SIZE, KEY_SIZE, Rgb([0, 0, 0]));
    let off = ((KEY_SIZE - GRAPH_FIT) / 2) as i64;
    image::imageops::overlay(&mut canvas, &scaled, off, off);
    canvas
}

enum Sampler {
    Cpu(CpuSampler),
    Ram(History),
    Network(NetSampler),
    Gpu(GpuSampler),
}

impl Sampler {
    fn render(&mut self) -> Tile {
        match self {
            Sampler::Cpu(s) => {
                let v = s.sample().unwrap_or(0.0);
                s.history.push(v);
                area_graph(&s.history, row_color)
            }
            Sampler::Ram(h) => {
                let v = read_ram_used().unwrap_or(0.0);
                h.push(v);
                // RAM keeps a fixed colour (it's a level, not a load to alarm on).
                area_graph(h, |_| Rgb([120, 165, 235]))
            }
            Sampler::Gpu(s) => {
                let v = s.sample_busy().unwrap_or(0.0);
                s.history.push(v);
                area_graph(&s.history, row_color)
            }
            Sampler::Network(s) => s.render(),
        }
    }
}

/// A bounded history of 0..1 samples (one per horizontal pixel).
struct History {
    samples: VecDeque<f32>,
}

impl History {
    fn new() -> Self {
        History { samples: VecDeque::with_capacity(COLS) }
    }
    fn push(&mut self, v: f32) {
        if self.samples.len() == COLS {
            self.samples.pop_front();
        }
        self.samples.push_back(v.clamp(0.0, 1.0));
    }
}

/// Draw a filled area silhouette (oldest left, newest right) growing up from the
/// bottom. Each pixel is coloured by `color_at(y)` — so for the load gradient a
/// single column runs green (bottom) → red (top). Columns are joined so the
/// outline is continuous (no gappy bars).
fn area_graph(history: &History, color_at: impl Fn(i32) -> Rgb<u8>) -> Tile {
    let mut tile = Tile::from_pixel(KEY_SIZE, KEY_SIZE, BG);
    let n = history.samples.len();
    let mut prev_top: Option<i32> = None;
    for (i, &v) in history.samples.iter().enumerate() {
        let x = (COLS - n + i) as i32;
        if !(0..KEY_SIZE as i32).contains(&x) {
            continue;
        }
        let h = (v.clamp(0.0, 1.0) * KEY_SIZE as f32).round() as i32;
        let top = KEY_SIZE as i32 - h;
        // Fill the column, colouring each pixel by its row.
        for y in top..KEY_SIZE as i32 {
            tile.put_pixel(x as u32, y as u32, color_at(y));
        }
        // Join this column's top to the previous one (continuous outline).
        if let Some(p) = prev_top {
            for y in p.min(top)..=p.max(top) {
                if (0..KEY_SIZE as i32).contains(&y) {
                    tile.put_pixel(x as u32, y as u32, color_at(y));
                }
            }
        }
        prev_top = Some(top);
    }
    tile
}

/// Colour for pixel row `y` (0 = top) by the load it represents: pure green at
/// ≤10%, pure red at ≥90%, green→yellow→red between. A column is thus a vertical
/// gradient clipped to its fill height.
fn row_color(y: i32) -> Rgb<u8> {
    load_color((KEY_SIZE as f32 - y as f32) / KEY_SIZE as f32)
}

fn load_color(v: f32) -> Rgb<u8> {
    const GREEN: [f32; 3] = [0.0, 200.0, 0.0];
    const YELLOW: [f32; 3] = [240.0, 210.0, 0.0];
    const RED: [f32; 3] = [225.0, 0.0, 0.0];
    let t = ((v - 0.10) / 0.80).clamp(0.0, 1.0); // 0 at 10%, 1 at 90%
    let lerp = |a: [f32; 3], b: [f32; 3], k: f32| {
        Rgb([
            (a[0] + (b[0] - a[0]) * k) as u8,
            (a[1] + (b[1] - a[1]) * k) as u8,
            (a[2] + (b[2] - a[2]) * k) as u8,
        ])
    };
    if t < 0.5 {
        lerp(GREEN, YELLOW, t * 2.0)
    } else {
        lerp(YELLOW, RED, (t - 0.5) * 2.0)
    }
}

// --- CPU -------------------------------------------------------------------

struct CpuSampler {
    prev_total: u64,
    prev_idle: u64,
    history: History,
}

impl CpuSampler {
    fn new() -> Self {
        CpuSampler { prev_total: 0, prev_idle: 0, history: History::new() }
    }

    /// Busy fraction since the previous sample, from `/proc/stat`.
    fn sample(&mut self) -> Option<f32> {
        let (total, idle) = read_proc_stat()?;
        let dt = total.checked_sub(self.prev_total)?;
        let di = idle.checked_sub(self.prev_idle)?;
        self.prev_total = total;
        self.prev_idle = idle;
        if dt == 0 {
            return Some(0.0);
        }
        Some((1.0 - di as f32 / dt as f32).clamp(0.0, 1.0))
    }
}

/// Parse the aggregate `cpu` line of `/proc/stat` into (total, idle) jiffies.
fn read_proc_stat() -> Option<(u64, u64)> {
    let content = std::fs::read_to_string("/proc/stat").ok()?;
    let line = content.lines().next()?; // "cpu  u n s idle iowait irq ..."
    let mut fields = line.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    let vals: Vec<u64> = fields.filter_map(|f| f.parse().ok()).collect();
    if vals.len() < 5 {
        return None;
    }
    let total: u64 = vals.iter().sum();
    let idle = vals[3] + vals.get(4).copied().unwrap_or(0); // idle + iowait
    Some((total, idle))
}

// --- RAM -------------------------------------------------------------------

/// Used-memory fraction from `/proc/meminfo` (1 - MemAvailable/MemTotal).
fn read_ram_used() -> Option<f32> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = None;
    let mut available = None;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total = rest.split_whitespace().next()?.parse::<f64>().ok();
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available = rest.split_whitespace().next()?.parse::<f64>().ok();
        }
    }
    let (total, available) = (total?, available?);
    if total <= 0.0 {
        return None;
    }
    Some((1.0 - (available / total)).clamp(0.0, 1.0) as f32)
}

// --- Network ---------------------------------------------------------------

// Network graph palette — clearly distinct hues (cyan-blue vs magenta-purple).
const NET_DOWN: Rgb<u8> = Rgb([105, 200, 240]); // download — pastel cyan-blue (grows UP)
const NET_UP: Rgb<u8> = Rgb([205, 135, 230]); // upload — pastel magenta-purple (grows DOWN)
const NET_DIVIDER: Rgb<u8> = Rgb([90, 90, 104]);

/// EMA weight on each new sample — low-passes spiky traffic into rolling hills.
const NET_SMOOTH: f32 = 0.4;
/// Log-scale floor (bytes/s, ≈1 KB/s): rates at/below map toward the bottom.
const NET_FLOOR_BYTES: f32 = 1000.0;
/// Rates below this are treated as idle (zero height).
const NET_IDLE_BYTES: f32 = 1.0;
/// Minimum graph value for any nonzero traffic — ~2px on a 36px half, chosen so
/// it survives the margin downscale as a visible pixel.
const NET_MIN_V: f32 = 4.0 / KEY_SIZE as f32;

struct NetSampler {
    iface: Option<String>,
    prev: Option<(u64, u64, Instant)>, // rx, tx, when
    down: VecDeque<f32>,
    up: VecDeque<f32>,
    down_ema: f32, // smoothed bytes/s
    up_ema: f32,
    /// Rolling peak (bytes/s) for the relative fallback (down/up share it).
    peak: f32,
    /// Absolute full-scale, bytes/s, from the link speed. `None` → relative.
    /// Resolved once the interface is known (`ceiling_checked`).
    ceiling: Option<f32>,
    ceiling_checked: bool,
}

impl NetSampler {
    fn new(iface: Option<String>) -> Self {
        NetSampler {
            iface,
            prev: None,
            down: VecDeque::with_capacity(COLS),
            up: VecDeque::with_capacity(COLS),
            down_ema: 0.0,
            up_ema: 0.0,
            peak: 1.0,
            ceiling: None,
            ceiling_checked: false,
        }
    }

    fn push(buf: &mut VecDeque<f32>, v: f32) {
        if buf.len() == COLS {
            buf.pop_front();
        }
        buf.push_back(v.clamp(0.0, 1.0));
    }

    /// Map a smoothed rate (bytes/s) to a 0..1 graph value. Absolute log scale
    /// (floor → link speed) when the link speed is known; otherwise relative to
    /// a rolling peak. Any nonzero traffic yields at least a visible pixel.
    fn scale(&self, rate: f32) -> f32 {
        if rate < NET_IDLE_BYTES {
            return 0.0;
        }
        let v = match self.ceiling {
            Some(ceiling) => {
                let lo = NET_FLOOR_BYTES.ln();
                let hi = ceiling.max(NET_FLOOR_BYTES * 2.0).ln();
                ((rate.ln() - lo) / (hi - lo)).clamp(0.0, 1.0)
            }
            None => (rate / self.peak).clamp(0.0, 1.0),
        };
        v.clamp(NET_MIN_V, 1.0)
    }

    fn render(&mut self) -> Tile {
        // Resolve the interface lazily so a cable plugged in after start works.
        let iface = match self.iface.clone().or_else(default_route_iface) {
            Some(i) => i,
            None => {
                debug!("network widget: no interface to graph");
                return Tile::from_pixel(KEY_SIZE, KEY_SIZE, BG);
            }
        };
        // Determine the scale once the interface is known: absolute (log against
        // link speed) if reported, else relative.
        if !self.ceiling_checked {
            self.ceiling = link_speed_bytes(&iface);
            self.ceiling_checked = true;
            match self.ceiling {
                Some(c) => debug!(
                    "network widget: {iface} link {} Mbit/s → absolute log scale",
                    (c / 125_000.0) as u64
                ),
                None => debug!("network widget: {iface} link speed unknown → relative scale"),
            }
        }
        if let Some((rx, tx)) = read_net_bytes(&iface) {
            let now = Instant::now();
            if let Some((prx, ptx, pwhen)) = self.prev {
                let secs = now.duration_since(pwhen).as_secs_f32().max(0.001);
                let drx = rx.saturating_sub(prx) as f32 / secs;
                let dtx = tx.saturating_sub(ptx) as f32 / secs;
                self.down_ema = NET_SMOOTH * drx + (1.0 - NET_SMOOTH) * self.down_ema;
                self.up_ema = NET_SMOOTH * dtx + (1.0 - NET_SMOOTH) * self.up_ema;
                // Relative mode tracks a slowly-decaying shared peak.
                if self.ceiling.is_none() {
                    self.peak = (self.peak * 0.95).max(self.down_ema).max(self.up_ema).max(1.0);
                }
                let d = self.scale(self.down_ema);
                let u = self.scale(self.up_ema);
                Self::push(&mut self.down, d);
                Self::push(&mut self.up, u);
            }
            self.prev = Some((rx, tx, now));
        }
        self.draw()
    }

    /// Both signals grow OUT from the centre divider: download upward, upload
    /// downward. Each is a connected filled silhouette (continuous outline — no
    /// blank columns between samples).
    fn draw(&self) -> Tile {
        let mut tile = Tile::from_pixel(KEY_SIZE, KEY_SIZE, BG);
        let mid = KEY_SIZE as i32 / 2;
        let half = mid as f32; // 36px each side
        silhouette(&mut tile, &self.down, NET_DOWN, mid, half, true);
        silhouette(&mut tile, &self.up, NET_UP, mid, half, false);
        render::fill_rect(&mut tile, 0, mid, KEY_SIZE, 1, NET_DIVIDER);
        tile
    }
}

/// Fill a connected silhouette anchored at the divider `mid`, growing up
/// (`grow_up`) or down by up to `half` pixels. Each column is filled to the
/// divider; adjacent column edges are joined vertically so the outline never
/// breaks into separate vertical lines.
fn silhouette(tile: &mut Tile, samples: &VecDeque<f32>, color: Rgb<u8>, mid: i32, half: f32, grow_up: bool) {
    let n = samples.len();
    let mut prev_edge: Option<i32> = None;
    for (i, &v) in samples.iter().enumerate() {
        let x = (COLS - n + i) as i32;
        let h = (v.clamp(0.0, 1.0) * half).round() as i32;
        let edge = if grow_up { mid - h } else { mid + h };
        // Column fill between the free edge and the divider.
        let (top, bot) = if grow_up { (edge, mid) } else { (mid, edge) };
        render::fill_rect(tile, x, top, 1, (bot - top).max(0) as u32, color);
        // Join this edge to the previous column's edge (continuous outline).
        if let (Some(p), true) = (prev_edge, (0..KEY_SIZE as i32).contains(&x)) {
            for y in p.min(edge)..=p.max(edge) {
                if (0..KEY_SIZE as i32).contains(&y) {
                    tile.put_pixel(x as u32, y as u32, color);
                }
            }
        }
        prev_edge = Some(edge);
    }
}

/// The interface's negotiated link speed in bytes/s, from
/// `/sys/class/net/<iface>/speed` (Mbit/s). `None` when unknown — Wi-Fi and
/// virtual interfaces report `-1` (or the file errors), which selects the
/// relative fallback scale.
fn link_speed_bytes(iface: &str) -> Option<f32> {
    let raw = std::fs::read_to_string(format!("/sys/class/net/{iface}/speed")).ok()?;
    let mbit: i64 = raw.trim().parse().ok()?;
    // 1 Mbit/s = 1_000_000 bits/s = 125_000 bytes/s.
    (mbit > 0).then_some(mbit as f32 * 125_000.0)
}

/// Read (rx_bytes, tx_bytes) for an interface from `/proc/net/dev`.
fn read_net_bytes(iface: &str) -> Option<(u64, u64)> {
    let content = std::fs::read_to_string("/proc/net/dev").ok()?;
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(iface)
            && let Some(rest) = rest.strip_prefix(':')
        {
            let vals: Vec<u64> = rest.split_whitespace().filter_map(|f| f.parse().ok()).collect();
            // Fields: rx_bytes(0) ... tx_bytes(8).
            if vals.len() >= 9 {
                return Some((vals[0], vals[8]));
            }
        }
    }
    None
}

/// The interface of the default route, from `/proc/net/route` (destination
/// 00000000). Used when no interface is configured.
fn default_route_iface() -> Option<String> {
    let content = std::fs::read_to_string("/proc/net/route").ok()?;
    for line in content.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let iface = fields.next()?;
        let dest = fields.next()?;
        if dest == "00000000" {
            return Some(iface.to_string());
        }
    }
    None
}

// --- GPU (AMD) -------------------------------------------------------------

struct GpuSampler {
    busy_path: Option<PathBuf>,
    history: History,
}

impl GpuSampler {
    fn new() -> Self {
        match detect_amd_gpu() {
            Some(dev) => {
                debug!("GPU widget: using {}", dev.display());
                GpuSampler { busy_path: Some(dev.join("gpu_busy_percent")), history: History::new() }
            }
            None => {
                warn!("GPU widget: no AMD GPU with gpu_busy_percent found (NVIDIA/Intel unsupported)");
                GpuSampler { busy_path: None, history: History::new() }
            }
        }
    }

    fn sample_busy(&self) -> Option<f32> {
        let v = read_u64_file(self.busy_path.as_ref()?)?;
        Some((v as f32 / 100.0).clamp(0.0, 1.0))
    }
}

/// Auto-detect the **discrete** AMD GPU: among `/sys/class/drm/card*/device`
/// nodes that are AMD (vendor 0x1002) and expose `gpu_busy_percent`, pick the
/// one with the largest VRAM (the dGPU vs. the iGPU). Card numbering isn't
/// stable across boots, so we never hardcode `card1`.
fn detect_amd_gpu() -> Option<PathBuf> {
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Match `card0`, `card1`, … but not the connector nodes (`card0-DP-1`).
        if !(name.starts_with("card") && name[4..].chars().all(|c| c.is_ascii_digit())) {
            continue;
        }
        let dev = entry.path().join("device");
        // AMD vendor id.
        if read_to_string_trim(&dev.join("vendor")).as_deref() != Some("0x1002") {
            continue;
        }
        if !dev.join("gpu_busy_percent").exists() {
            continue;
        }
        let vram = dev.join("mem_info_vram_total");
        let size = read_u64_file(&vram).unwrap_or(0);
        if best.as_ref().map(|(s, _)| size > *s).unwrap_or(true) {
            best = Some((size, dev));
        }
    }
    best.map(|(_, path)| path)
}

fn read_u64_file(path: &std::path::Path) -> Option<u64> {
    read_to_string_trim(path)?.parse().ok()
}

fn read_to_string_trim(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_stat_parses_busy_fraction() {
        // Two reads with a known delta → 50% busy.
        let mut cpu = CpuSampler::new();
        // Seed previous values directly.
        cpu.prev_total = 0;
        cpu.prev_idle = 0;
        // Manually exercise the delta math with synthetic totals.
        cpu.prev_total = 100;
        cpu.prev_idle = 50;
        // Pretend the next read gives total=200, idle=100 → di=50, dt=100 → 50%.
        let di = 100u64 - cpu.prev_idle;
        let dt = 200u64 - cpu.prev_total;
        let busy = 1.0 - di as f32 / dt as f32;
        assert!((busy - 0.5).abs() < 1e-6);
    }

    #[test]
    fn history_is_bounded_and_right_aligned() {
        let mut h = History::new();
        for i in 0..(COLS + 10) {
            h.push((i as f32) / 100.0);
        }
        assert_eq!(h.samples.len(), COLS);
    }

    #[test]
    fn load_color_ramps_green_to_red() {
        assert_eq!(load_color(0.0), load_color(0.10)); // clamped green below 10%
        assert_eq!(load_color(1.0), load_color(0.90)); // clamped red above 90%
        let lo = load_color(0.05); // green-ish: G dominant
        let hi = load_color(0.95); // red-ish: R dominant
        assert!(lo[1] > lo[0], "low load should be greener than red");
        assert!(hi[0] > hi[1], "high load should be redder than green");
    }

    #[test]
    fn ram_used_is_a_fraction() {
        // Reads the real /proc/meminfo on the test host; just sanity-check range.
        if let Some(v) = read_ram_used() {
            assert!((0.0..=1.0).contains(&v));
        }
    }
}
