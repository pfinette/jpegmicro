# JPEGmicro

ARM-native macOS app, published at github.com/pfinette/jpegmicro. Drop JPEGs or folders on the window. Each photo is re-encoded with MozJPEG at the lowest quality that DSSIM still scores as visually identical. The new file replaces the original in place, and the original goes to the Trash. It replaces JPEGmini, which only runs under Rosetta.

Distributed as an ad-hoc-signed, unnotarized .app zip on GitHub Releases. Own code is MIT. See README for install steps and the license notice.

## Status

- macOS only. It was first written on Linux; the Linux code paths are gone.
- `cargo build --release`, `cargo clippy --all-targets` and `cargo test` are clean.
- The CLI, metadata and speed are verified on macOS. The GUI items below are not.

## Commands

```sh
./bundle.sh                 # release build -> target/JPEGmicro.app (icon, Info.plist, ad-hoc codesign)
./bundle.sh --install       # also copies to /Applications
cargo run --release --bin jpegmicro-cli -- --dry-run <files or folders>
cargo run --release --bin jpegmicro -- <paths>   # GUI; paths on argv are processed at launch
```

The first build compiles MozJPEG from C (mozjpeg-sys). No nasm is needed on ARM.

## Layout

| Path | Contents |
|---|---|
| `src/lib.rs` | Pipeline: `collect_jpegs`, `process_file`, `analyze`, `replace_file`, `worker_pool` |
| `src/main.rs` | eframe/egui app: palettes and fonts, settings bar, empty state, file rows with size bars, footer totals, persisted prefs |
| `src/bin/cli.rs` | CLI with `--strict/--balanced/--aggressive`, `--min-saving N`, `--dry-run` |
| `bundle.sh` | Builds the .app; makes the .icns with `sips` + `iconutil` |
| `assets/icon.png` | 1024 px icon source (blue rounded square with "μ" and "JPEG") |
| `about.toml`, `about.hbs` | cargo-about config. `cargo about generate about.hbs -o THIRD_PARTY_LICENSES` after any dependency change |

## Stack

| Crate | Version | Notes |
|---|---|---|
| eframe / egui | 0.37 | New API: `App::ui(&mut self, ui, frame)`, `egui::Panel::top/bottom(id).show(ui, ..)`, `CentralPanel::default().show(ui, ..)`. `persistence` feature on. |
| mozjpeg | 0.10 | Errors arrive as **panics**. All codec work is wrapped in `catch_unwind`, and `panic = "unwind"` is set in the release profile. |
| dssim-core | 3.5 | AGPL-3.0. Release binaries are therefore AGPL-3.0 as a whole; README and THIRD_PARTY_LICENSES say so. Replacing it with a permissive metric (e.g. ssimulacra2) would mean recalibrating the thresholds. |
| rayon | 1 | File-level parallelism through `worker_pool()`, capped at 6 threads to bound memory |
| trash | 5 | macOS uses `DeleteMethod::NsFileManager`, which avoids Finder's Automation permission prompt |
| xattr | 1 | Copies extended attributes such as Finder tags. Best effort. |

## Pipeline (`analyze` + `replace_file`)

1. Accept `.jpg`/`.jpeg` files that start with `FF D8`. Walk folders recursively. Skip hidden entries and symlinks.
2. Read all markers. A COM marker starting with `JPEGmicro v1` means the file was already processed, so skip it. An MPF APP2 marker means secondary images such as an HDR gain map. MozJPEG would drop them and leave a dangling MPF header, so skip the file.
3. Skip CMYK/YCCK. Grayscale is encoded as grayscale.
4. Decode to RGB without applying EXIF orientation, because the orientation tag is copied back unchanged.
5. Find the lowest quality in 40–95 whose DSSIM is at or below the threshold (`lowest_passing`). The search starts at `Strictness::seed_q()` (Strict 92, Balanced 90, Aggressive 83), gallops outward until it brackets the threshold, then bisects. It finds the same answer as plain bisection, usually in 2–3 passes instead of 6, and at most 12. The seeds are where each mode lands on iPhone JPEGs. While at most half the cores have a file (`IN_FLIGHT`), each round tests two qualities at once on a scoped thread and splits the bracket into thirds. A single photo usually finishes in one round. Full batches fall back to one probe per round, because file-level parallelism already fills the cores. The second probe uses `std::thread::scope` rather than `rayon::join`, because a pool worker waiting in `join` can pick up another file and raise memory.
6. Every encode copies markers APP1–APP13, APP15 and COM. APP0 (JFIF) and APP14 (Adobe) are dropped because the encoder rewrites them. Chroma subsampling and pixel density match the original. A `JPEGmicro v1 q=NN` COM marker is added.
7. If the result isn't at least `min_saving` smaller (default 5%), report `NoGain` and leave the file untouched.
8. Safe swap:
   1. Write `.<name>.jpegmicro-tmp` beside the original and fsync it.
   2. Re-decode it to verify.
   3. Copy the permissions and xattrs.
   4. Set the modified, accessed and created times from the original.
   5. Move the original to the Trash.
   6. Rename the temp file into place.

   On any error the temp file is removed and the original stays untouched.

## Design (`src/main.rs`)

Hardware skin, after Klevgrand plugins, Ableton Live and fm1-sampler: the window is a device with a faceplate, a recessed screen and a few physical controls.

- Title strip (in the title bar, beside the traffic lights): spaced JPEGMICRO wordmark, a dim "Originals go to the Trash" legend and a status LCD (READY, WORKING 3/12, DONE 12/12). Dragging the strip moves the window; double-clicking zooms.
- Screen: one track per file. Each track's LED meter is as long as the original file, relative to the largest in the list. Kept bytes light grey segments; removed bytes light orange. Statuses are uppercase LCD text; details go in tooltips; right-click offers Show in Finder.
- Control row: a three-position STRICTNESS switch, a MIN SAVING arc knob (drag up or down, arrow keys when focused, double-click resets to 5%), an orange SAVED readout and a CLEAR button. Custom controls report accessibility roles (radio button, slider, button).
- Orange `#FF6A1F` is the only accent and always means saved bytes or the active setting.
- Type: DIN Alternate Bold for the wordmark and faceplate labels, SF Mono on screens, SF Pro for tooltips and menus. All load from the system at startup, with egui's fonts as fallback.

| Token | Light face | Dark face |
|---|---|---|
| Face / groove | `#E7E5DF` / `#C9C5BC` | `#2B2B2A` / `#444442` |
| Ink / dim | `#1B1B1A` / `#6F6C66` | `#ECEAE4` / `#9A978F` |
| Screen / edge | `#1A1B1D` / `#9C9890` | `#17181A` / `#0E0E0F` |
| Screen text / dim | `#E6E4DE` / `#828488` | same |
| Segment off / kept | `#2C2E32` / `#9A9DA3` | same |

- macOS conventions: overlay scrollbars, a 0.6 s tooltip delay, labels that can't be selected, and ⌘W to close.
- egui stops repainting a window that macOS reports as covered, so screenshots of a background window show stale progress. For the same reason a Dock bounce when a batch finishes wouldn't fire reliably, so there is none.

## Settings

| Strictness | Max DSSIM |
|---|---|
| Strict | 0.0005 |
| Balanced | 0.0010 |
| Aggressive (default) | 0.0020 |

These were checked by eye on sample photos at 200% zoom, and even Aggressive looked identical. Recalibrate them on real photos.

## Test results (Linux, MozJPEG)

- EXIF date, GPS, make/model, orientation, ICC profile, 4:4:4 subsampling and mtime all survived (checked with exiftool).
- A second run skipped every file. Quality-60 and already-compressed files came back as NoGain. CMYK was skipped.
- Balanced saved 7–34% on normal photos and up to 77% on a quality-98 file. Aggressive saved roughly twice as much.
- A 24 MP photo took about 48 s on a slow 2-vCPU VM. Per pass: about 1.7 s for the mozjpeg encode, dominated by trellis quantization, and about 2.7 s for DSSIM create plus compare. Turning off `optimize_scans` didn't speed up the encode.

## Test results (macOS, M5)

Test set: 16 iPhone 14 Pro JPEGs (12 MP, 4:2:0, q≈93, GPS, Photos xattrs). The 24 MP file is two of them stitched side by side, because no real 24 MP file was available.

- **Metadata.** exiftool shows no differences apart from the new COM tag and progressive encoding. Birth time, mtime, permissions, a Finder tag, all Photos `assetsd` xattrs, and `mdls` date and location survived. macOS adds `com.apple.provenance` to the new file.
- **Edge cases.** Grayscale stays grayscale. 4:2:2, 4:4:4 and 300 dpi JFIF density are preserved. CMYK and MPF files are skipped. A second run skips everything.
- **Trash.** The CLI moved the originals with no error and no prompt, but the Trash contents weren't checked, because the shell can't read `~/.Trash`.

| | MozJPEG + bisect (original) | MozJPEG + seeded search (current) |
|---|---|---|
| 12 MP, one file, Aggressive | ~12 s | 1.7 s |
| 24 MP, one file, Aggressive | ~21 s | 3.7 s |
| 2–3 photos, Aggressive | — | 7–8 s, set by the slowest photo |
| 16 photos, Strict | 15.0%, 54 s | 15.0%, 23 s |
| 16 photos, Balanced | 30.0%, 51 s | 30.0%, 20 s |
| 16 photos, Aggressive | 54.8%, 43 s | 54.8%, 23–26 s |

- Per pass at 12 MP, the MozJPEG encode takes 0.8–2.1 s on one thread (trellis is about a third of it) and DSSIM scoring takes about 0.3 s.
- Peak RSS is about 2.7 GB for a single paired 12 MP file and 5.6 GB for 24 MP, and DSSIM accounts for most of it. A 16-photo batch peaked at about 10 GB.
- Batches vary by ±15% from run to run, so A/B them by alternating runs.
- Pairing every round, even in batches, made a full batch about 30% slower through contention. Pairing only while at most a quarter of the cores have a file was no better than the half rule on 2–3 photo drops.
- **jpegli was tried and rejected.** `zenjpeg` 0.8 (pure-Rust jpegli) encodes 4–5× faster and matched MozJPEG at Balanced (30.7%). At Aggressive it saved only 48.7%, against 54.8%, because its adaptive quantization targets Butteraugli, which DSSIM doesn't reward. Turning off trellis during the search was also rejected, because trellis adds distortion and the chosen quality then failed the threshold.

## Unverified on macOS

1. **Drag-and-drop** onto the window through `raw.dropped_files`. Check folders as well as files.
2. **Trash.** Check that the original appears in the Trash and that no permission prompt appears in the GUI.
3. **Photos and Preview** show the same date and location after processing.
4. **bundle.sh.** Check the icon in the Dock and that the app launches from /Applications.
5. **Real 24 MP camera files** for both speed and savings.

## Ideas if it's too slow

- Score on a downscaled copy or on high-detail tiles. Both are less strict, so recalibrate the thresholds afterwards.

## Out of scope

- Dropping files on the Dock icon. Launching the app and dropping onto its window is enough.
- HEIC, PNG, resizing and format conversion.
- FastFrame, notarization and auto-updates.

## Known quirks

- The footer's saved % uses the size of every finished file as the denominator, including skipped files.
- Settings changed mid-run apply only to the next drop.
- MPF files (iPhone HDR gain maps, Ultra HDR) are skipped. Supporting them would mean re-appending the secondary images after EOI and rewriting the MPF offsets.
