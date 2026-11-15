//! JPEGmicro core: re-encode a JPEG with MozJPEG at the lowest quality that
//! DSSIM still scores as visually identical, keep its metadata, and swap it in
//! place with the original moved to the Trash.

use std::fs;
use std::io::{self, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use dssim_core::{Dssim, DssimImage};
use mozjpeg::{ALL_MARKERS, ColorSpace, Compress, Decompress, Marker, PixelDensity};
use rgb::RGB8;
use serde::{Deserialize, Serialize};

/// Prefix of the COM marker written into every file JPEGmicro produces.
pub const TAG_PREFIX: &[u8] = b"JPEGmicro v1";

/// Quality range the search covers.
const Q_MIN: u8 = 40;
const Q_MAX: u8 = 95;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Strictness {
    Strict,
    Balanced,
    Aggressive,
}

impl Strictness {
    pub const ALL: [Strictness; 3] = [Strictness::Strict, Strictness::Balanced, Strictness::Aggressive];

    /// Highest DSSIM score (0 = identical) a re-encode may have.
    pub fn max_dssim(self) -> f64 {
        match self {
            Strictness::Strict => 0.0005,
            Strictness::Balanced => 0.0010,
            Strictness::Aggressive => 0.0020,
        }
    }

    /// First quality the search tries: where this strictness lands on typical camera JPEGs.
    pub fn seed_q(self) -> u8 {
        match self {
            Strictness::Strict => 92,
            Strictness::Balanced => 90,
            Strictness::Aggressive => 83,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Strictness::Strict => "Strict",
            Strictness::Balanced => "Balanced",
            Strictness::Aggressive => "Aggressive",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub strictness: Strictness,

    /// Minimum size reduction, as a fraction (0.05 = 5%), for a file to be replaced.
    pub min_saving: f64,

    /// Analyze only; never touch files on disk.
    pub dry_run: bool,
}

impl Settings {
    pub fn new(strictness: Strictness, min_saving_percent: f64) -> Self {
        Self { strictness, min_saving: min_saving_percent / 100.0, dry_run: false }
    }
}

#[derive(Clone, Debug)]
pub enum Outcome {
    /// Replaced (or, in a dry run, would have been).
    Compressed { before: u64, after: u64, quality: u8, dssim: f64 },

    /// Carries the JPEGmicro marker from an earlier run.
    AlreadyProcessed { size: u64 },

    /// Best candidate didn't clear the minimum saving.
    NoGain { size: u64, best: Option<u64> },

    /// Not something JPEGmicro handles, such as CMYK.
    Skipped { size: u64, reason: String },
}

impl Outcome {
    pub fn bytes_saved(&self) -> u64 {
        match self {
            Outcome::Compressed { before, after, .. } => before - after,
            _ => 0,
        }
    }

    pub fn size_before(&self) -> u64 {
        match self {
            Outcome::Compressed { before, .. } => *before,
            Outcome::AlreadyProcessed { size } | Outcome::NoGain { size, .. } | Outcome::Skipped { size, .. } => *size,
        }
    }
}

/// Result of analyzing JPEG bytes in memory.
pub struct Analysis {
    pub outcome: Outcome,

    /// New file contents when `outcome` is `Compressed`.
    pub output: Option<Vec<u8>>,
}

/// Expand files and folders into a sorted list of JPEG files.
/// Folders are walked recursively; hidden entries and symlinks are ignored.
pub fn collect_jpegs(inputs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in inputs {
        walk(p, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

fn walk(path: &Path, out: &mut Vec<PathBuf>) {
    let Ok(meta) = fs::symlink_metadata(path) else { return };
    if meta.file_type().is_symlink() {
        return;
    }
    if meta.is_dir() {
        let Ok(entries) = fs::read_dir(path) else { return };
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            walk(&entry.path(), out);
        }
    } else if meta.is_file() && is_jpeg_name(path) {
        out.push(path.to_path_buf());
    }
}

fn is_jpeg_name(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"))
}

/// Analyze one file and, unless `dry_run`, replace it.
pub fn process_file(path: &Path, settings: &Settings) -> Result<Outcome, String> {
    let bytes = fs::read(path).map_err(|e| format!("read failed: {e}"))?;
    let analysis = analyze(&bytes, settings)?;
    if let (Some(output), false) = (&analysis.output, settings.dry_run) {
        replace_file(path, output).map_err(|e| format!("replace failed: {e}"))?;
    }
    Ok(analysis.outcome)
}

/// Pure in-memory step: decide what to do with these JPEG bytes.
pub fn analyze(bytes: &[u8], settings: &Settings) -> Result<Analysis, String> {
    let size = bytes.len() as u64;
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return Ok(skipped(size, "not a JPEG"));
    }

    // MozJPEG signals decode and encode errors by panicking.
    let result = panic::catch_unwind(AssertUnwindSafe(|| analyze_inner(bytes, settings)));
    match result {
        Ok(r) => r,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown codec error".into());
            Err(format!("JPEG error: {msg}"))
        }
    }
}

fn skipped(size: u64, reason: &str) -> Analysis {
    Analysis { outcome: Outcome::Skipped { size, reason: reason.into() }, output: None }
}

/// Decoded source image plus everything needed to re-encode it faithfully.
struct Source {
    width: usize,
    height: usize,
    gray: bool,

    /// Interleaved samples: RGB, or one byte per pixel when `gray`.
    pixels: Vec<u8>,
    markers: Vec<(Marker, Vec<u8>)>,

    /// Chroma pixel size relative to luma, e.g. (2, 2) for 4:2:0.
    chroma: (u8, u8),
    density: Option<PixelDensity>,
}

/// Files being analyzed right now, across all threads.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

struct InFlight;

impl InFlight {
    fn enter() -> Self {
        IN_FLIGHT.fetch_add(1, Ordering::Relaxed);
        InFlight
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
    }
}

fn analyze_inner(bytes: &[u8], settings: &Settings) -> Result<Analysis, String> {
    let _in_flight = InFlight::enter();
    let size = bytes.len() as u64;
    let mut d = Decompress::with_markers(ALL_MARKERS)
        .from_mem(bytes)
        .map_err(|e| format!("decode failed: {e}"))?;

    let mut markers = Vec::new();
    for m in d.markers() {
        if m.marker == Marker::COM && m.data.starts_with(TAG_PREFIX) {
            return Ok(Analysis { outcome: Outcome::AlreadyProcessed { size }, output: None });
        }
        // MozJPEG can't carry secondary images, so a gain map would be lost.
        if m.marker == Marker::APP(2) && m.data.starts_with(b"MPF\0") {
            return Ok(skipped(size, "multi-picture (HDR gain map)"));
        }
        if keep_marker(m.marker) {
            markers.push((m.marker, m.data.to_vec()));
        }
    }

    let gray = match d.color_space() {
        ColorSpace::JCS_YCbCr | ColorSpace::JCS_RGB => false,
        ColorSpace::JCS_GRAYSCALE => true,
        _ => return Ok(skipped(size, "CMYK")),
    };

    let comps = d.components();
    let chroma = if gray || comps.len() < 3 {
        (1, 1)
    } else {
        let px = |l: i32, c: i32| if c > 0 && l / c >= 1 && l / c <= 2 { (l / c) as u8 } else { 2 };
        (px(comps[0].h_samp_factor, comps[1].h_samp_factor), px(comps[0].v_samp_factor, comps[1].v_samp_factor))
    };
    let density = d.pixel_density();
    let (width, height) = d.size();
    let pixels = read_pixels(d, gray)?;
    let src = Source { width, height, gray, pixels, markers, chroma, density };

    let dssim = Dssim::new();
    let reference = dssim_image(&dssim, &src.pixels, src.gray, width, height)
        .ok_or("image too small to compare")?;

    // A file gets a second core only while at most half the cores have a file.
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let paired = || IN_FLIGHT.load(Ordering::Relaxed) * 2 <= cores;

    // Probes can run two at a time, so keep the lowest passing candidate.
    let best: Mutex<Option<(u8, Vec<u8>, f64)>> = Mutex::new(None);
    lowest_passing(Q_MIN, Q_MAX, settings.strictness.seed_q(), paired, |q| {
        let candidate = encode(&src, q)?;
        let score = score(&dssim, &reference, &candidate, src.gray)?;
        let ok = score <= settings.strictness.max_dssim();
        if ok {
            let mut best = best.lock().unwrap();
            if best.as_ref().is_none_or(|(b, ..)| q < *b) {
                *best = Some((q, candidate, score));
            }
        }
        Ok(ok)
    })?;

    let Some((quality, output, dssim_score)) = best.into_inner().unwrap() else {
        return Ok(Analysis { outcome: Outcome::NoGain { size, best: None }, output: None });
    };

    let after = output.len() as u64;
    let limit = (size as f64 * (1.0 - settings.min_saving)) as u64;
    if after > limit {
        return Ok(Analysis { outcome: Outcome::NoGain { size, best: Some(after) }, output: None });
    }

    Ok(Analysis {
        outcome: Outcome::Compressed { before: size, after, quality, dssim: dssim_score },
        output: Some(output),
    })
}

/// Lowest quality in `lo..=hi` that `passes`, assuming passing is monotonic in quality.
/// Starts at `seed`, steps outward until the threshold is bracketed, then bisects it.
/// Rounds where `paired()` is true test two qualities at once and split the bracket into thirds.
fn lowest_passing(
    lo: u8,
    hi: u8,
    seed: u8,
    paired: impl Fn() -> bool,
    passes: impl Fn(u8) -> Result<bool, String> + Sync,
) -> Result<Option<u8>, String> {
    let (lo, hi, seed) = (lo as i32, hi as i32, seed as i32);

    // Highest quality known to fail and lowest known to pass, both exclusive of the range.
    let (mut fail, mut pass) = (lo - 1, hi + 1);

    // 0 in the seed round, then 1, 2, 4…
    let mut step = 0;
    loop {
        let two = paired();
        let (a, b) = if pass > hi && fail < lo {
            (if two { seed - 1 } else { seed }, seed)
        } else if pass > hi {
            (fail + step, if two { fail + 2 * step } else { fail + step })
        } else if fail < lo {
            (if two { pass - 2 * step } else { pass - step }, pass - step)
        } else if two {
            let w = pass - fail;
            (fail + (w / 3).max(1), fail + (2 * w / 3).max(1))
        } else {
            ((fail + pass) / 2, (fail + pass) / 2)
        };
        let (a, b) = (a.clamp(fail + 1, pass - 1), b.clamp(fail + 1, pass - 1));

        // A scoped thread rather than rayon::join, so a waiting pool worker can't pick up another file and raise memory.
        let (ra, rb) = std::thread::scope(|s| {
            let other = (a != b).then(|| s.spawn(|| passes(b as u8)));
            let ra = passes(a as u8);
            (ra, other.map(|h| h.join().unwrap_or_else(|p| panic::resume_unwind(p))))
        });
        for (q, r) in [(a, Some(ra)), (b, rb)] {
            match r.transpose()? {
                Some(true) => pass = pass.min(q),
                Some(false) => fail = fail.max(q),
                None => {}
            }
        }

        if fail + 1 >= pass {
            break;
        }
        step = (step * 2).max(1);
    }
    Ok((pass <= hi).then_some(pass as u8))
}

/// APP1–APP13 and APP15 carry EXIF, XMP, ICC, IPTC and maker data.
/// APP0 (JFIF) is rewritten by the encoder; APP14 (Adobe) describes the old color transform.
fn keep_marker(m: Marker) -> bool {
    match m {
        Marker::APP(n) => n != 0 && n != 14,
        Marker::COM => true,
    }
}

fn encode(src: &Source, quality: u8) -> Result<Vec<u8>, String> {
    let mut c = Compress::new(if src.gray { ColorSpace::JCS_GRAYSCALE } else { ColorSpace::JCS_RGB });
    c.set_size(src.width, src.height);
    c.set_quality(quality as f32);
    if !src.gray {
        c.set_chroma_sampling_pixel_sizes(src.chroma, src.chroma);
    }
    if let Some(density) = &src.density {
        c.set_pixel_density(PixelDensity { unit: density.unit, x: density.x, y: density.y });
    }

    let mut s = c
        .start_compress(Vec::with_capacity(src.pixels.len() / 8))
        .map_err(|e| e.to_string())?;
    for (marker, data) in &src.markers {
        s.write_marker(*marker, data);
    }
    let tag = format!("{} q={quality}", String::from_utf8_lossy(TAG_PREFIX));
    s.write_marker(Marker::COM, tag.as_bytes());
    s.write_scanlines(&src.pixels).map_err(|e| e.to_string())?;
    s.finish().map_err(|e| e.to_string())
}

fn dssim_image(dssim: &Dssim, pixels: &[u8], gray: bool, w: usize, h: usize) -> Option<DssimImage<f32>> {
    if gray {
        let rgb: Vec<RGB8> = pixels.iter().map(|&v| RGB8::new(v, v, v)).collect();
        dssim.create_image_rgb(&rgb, w, h)
    } else {
        let rgb: &[RGB8] = rgb::FromSlice::as_rgb(pixels);
        dssim.create_image_rgb(rgb, w, h)
    }
}

/// Interleaved RGB bytes, or one byte per pixel when `gray`.
fn read_pixels(d: Decompress<&[u8]>, gray: bool) -> Result<Vec<u8>, String> {
    let mut img = if gray { d.grayscale() } else { d.rgb() }.map_err(|e| e.to_string())?;
    let px = img.read_scanlines::<u8>().map_err(|e| e.to_string())?;
    img.finish().map_err(|e| e.to_string())?;
    Ok(px)
}

fn score(dssim: &Dssim, reference: &DssimImage<f32>, jpeg: &[u8], gray: bool) -> Result<f64, String> {
    let d = Decompress::new_mem(jpeg).map_err(|e| e.to_string())?;
    let (w, h) = d.size();
    let pixels = read_pixels(d, gray)?;
    let candidate = dssim_image(dssim, &pixels, gray, w, h).ok_or("compare failed")?;
    let (val, _) = dssim.compare(reference, candidate);
    Ok(val.into())
}

/// Swap `new_bytes` in for `path`: write a temp file beside it, carry over
/// permissions, extended attributes and timestamps, check it decodes, move the
/// original to the Trash, then rename the temp file into place.
pub fn replace_file(path: &Path, new_bytes: &[u8]) -> io::Result<()> {
    let meta = fs::metadata(path)?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().ok_or_else(|| io::Error::other("no file name"))?;
    let tmp = dir.join(format!(".{}.jpegmicro-tmp", name.to_string_lossy()));

    let result = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(new_bytes)?;
        f.sync_all()?;
        drop(f);

        verify_jpeg(&tmp, new_bytes.len())?;
        fs::set_permissions(&tmp, meta.permissions())?;
        copy_xattrs(path, &tmp);

        // Times go last so nothing above bumps them.
        let f = fs::OpenOptions::new().write(true).open(&tmp)?;
        f.set_times(file_times(&meta)?)?;
        drop(f);

        move_to_trash(path)?;
        fs::rename(&tmp, path)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn verify_jpeg(path: &Path, expected_len: usize) -> io::Result<()> {
    let written = fs::read(path)?;
    if written.len() != expected_len {
        return Err(io::Error::other("temp file size mismatch"));
    }
    let ok = panic::catch_unwind(AssertUnwindSafe(|| {
        Decompress::new_mem(&written).map(|d| d.width() > 0 && d.height() > 0).unwrap_or(false)
    }))
    .unwrap_or(false);
    if ok { Ok(()) } else { Err(io::Error::other("re-encoded file failed to decode")) }
}

fn file_times(meta: &fs::Metadata) -> io::Result<fs::FileTimes> {
    let times = fs::FileTimes::new().set_modified(meta.modified()?).set_accessed(meta.accessed()?);

    #[cfg(target_os = "macos")]
    let times = {
        use std::os::macos::fs::FileTimesExt;
        times.set_created(meta.created()?)
    };

    Ok(times)
}

fn copy_xattrs(from: &Path, to: &Path) {
    let Ok(names) = xattr::list(from) else { return };
    for name in names {
        if let Ok(Some(value)) = xattr::get(from, &name) {
            let _ = xattr::set(to, &name, &value);
        }
    }
}

fn move_to_trash(path: &Path) -> io::Result<()> {
    #[allow(unused_mut)]
    let mut ctx = trash::TrashContext::default();

    // NSFileManager needs no Automation permission, unlike the Finder method.
    #[cfg(target_os = "macos")]
    {
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        ctx.set_delete_method(DeleteMethod::NsFileManager);
    }

    ctx.delete(path).map_err(|e| io::Error::other(format!("couldn't move original to Trash: {e}")))
}

/// Human-readable byte size, e.g. "3.4 MB".
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[unit]) }
}

/// Thread pool for processing files. Scoring takes about 2 GB per 12 MP photo
/// and 4 GB per 24 MP photo, so concurrency is capped to keep memory in check.
pub fn worker_pool() -> rayon::ThreadPool {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    rayon::ThreadPoolBuilder::new()
        .num_threads(cores.clamp(1, 6))
        .build()
        .expect("thread pool")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_finds_every_threshold() {
        for two in [false, true] {
            for s in Strictness::ALL {
                for t in 40..=96u8 {
                    let seed = s.seed_q();
                    let probes = AtomicUsize::new(0);
                    let got = lowest_passing(40, 95, seed, || two, |q| {
                        probes.fetch_add(1, Ordering::Relaxed);
                        Ok(q >= t)
                    })
                    .unwrap();
                    let probes = probes.into_inner();
                    assert_eq!(got, (t <= 95).then_some(t), "{s:?} paired={two} threshold {t}");
                    assert!(probes <= if two { 18 } else { 12 }, "{s:?} paired={two} threshold {t}: {probes} probes");

                    // The most common answer takes two probes either way.
                    if t == seed {
                        assert_eq!(probes, 2);
                    }
                }
            }
        }
    }

    #[test]
    fn search_reports_probe_errors() {
        let got = lowest_passing(40, 95, 83, || true, |q| if q == 82 { Err("boom".into()) } else { Ok(true) });
        assert_eq!(got, Err("boom".into()));
    }
}
