# JPEGmicro

Drop JPEGs or folders on the window. Each photo is re-encoded with MozJPEG at the lowest quality that DSSIM still scores as visually identical. The new file replaces the original in place, and the original goes to the Trash.

## Build

Needs Rust (`rustup`) and the Xcode command-line tools.

```sh
./bundle.sh            # builds target/JPEGmicro.app
./bundle.sh --install  # also copies it to /Applications
```

The first build compiles MozJPEG from source and takes a few minutes.

## What it keeps

- EXIF (capture date, GPS, camera, orientation), XMP, IPTC and the ICC color profile
- Chroma subsampling of the original (4:2:0, 4:2:2, 4:4:4) and its JFIF pixel density
- Permissions, extended attributes (Finder tags) and the created and modified dates

## Rules

- Files JPEGmicro already processed carry a `JPEGmicro v1` comment marker and are skipped.
- A file is replaced only if the result is at least the minimum saving smaller (default 5%).
- CMYK JPEGs and multi-picture JPEGs (HDR gain maps) are skipped. HEIC, PNG and other formats are ignored.
- Hidden files and symlinks are ignored; folders are searched recursively.

## Strictness

| Setting | Max DSSIM | Use |
|---|---|---|
| Strict | 0.0005 | Archival; smallest savings |
| Balanced | 0.0010 | Smaller savings; safe middle ground |
| Aggressive | 0.0020 | Default. Biggest savings; still hard to tell apart at 100% zoom |

Quality is searched between 40 and 95.

## Command line

The app bundle includes the CLI:

```sh
target/JPEGmicro.app/Contents/MacOS/jpegmicro-cli --dry-run ~/Pictures/some-folder
```

Run `jpegmicro-cli --help` for options. `--dry-run` reports savings without touching files.

## Layout

| Path | Contents |
|---|---|
| `src/lib.rs` | Pipeline: decode, quality search, metadata copy, safe swap |
| `src/main.rs` | egui app |
| `src/bin/cli.rs` | CLI |
| `bundle.sh` | Builds and signs the .app |
| `assets/icon.png` | App icon source |
