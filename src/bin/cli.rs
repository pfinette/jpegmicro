use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use jpegmicro::{Outcome, Settings, Strictness, collect_jpegs, format_size, process_file, worker_pool};
use rayon::prelude::*;

const USAGE: &str = "\
Usage: jpegmicro-cli [options] <files or folders>...

Options:
  --strict | --balanced | --aggressive   How much difference to allow (default: aggressive)
  --min-saving <percent>                 Replace only if at least this much smaller (default: 5)
  --dry-run                              Report what would happen; change nothing
  -h, --help                             Show this help";

fn main() -> ExitCode {
    let mut strictness = Strictness::Aggressive;
    let mut min_saving = 5.0;
    let mut dry_run = false;
    let mut inputs = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--strict" => strictness = Strictness::Strict,
            "--balanced" => strictness = Strictness::Balanced,
            "--aggressive" => strictness = Strictness::Aggressive,
            "--dry-run" => dry_run = true,
            "--min-saving" => match args.next().and_then(|v| v.parse().ok()) {
                Some(v) => min_saving = v,
                None => return usage_error("--min-saving needs a number"),
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            s if s.starts_with('-') => return usage_error(&format!("unknown option {s}")),
            _ => inputs.push(PathBuf::from(arg)),
        }
    }
    if inputs.is_empty() {
        return usage_error("no files given");
    }

    let mut settings = Settings::new(strictness, min_saving);
    settings.dry_run = dry_run;

    let files = collect_jpegs(&inputs);
    let started = Instant::now();
    let results: Vec<_> = worker_pool().install(|| {
        files
            .par_iter()
            .map(|path| {
                let t = Instant::now();
                let r = process_file(path, &settings);
                (path, r, t.elapsed())
            })
            .collect()
    });

    let (mut before, mut saved, mut errors) = (0u64, 0u64, 0);
    for (path, result, took) in &results {
        let name = path.display();
        let secs = took.as_secs_f32();
        match result {
            Ok(o) => {
                before += o.size_before();
                saved += o.bytes_saved();
                println!("{}  [{secs:.1}s]  {name}", describe(o));
            }
            Err(e) => {
                errors += 1;
                println!("ERROR {e}  {name}");
            }
        }
    }

    let pct = if before > 0 { saved as f64 * 100.0 / before as f64 } else { 0.0 };
    println!(
        "\n{} files, {} {} ({pct:.1}%) in {:.1}s{}",
        files.len(),
        format_size(saved),
        if dry_run { "would be saved" } else { "saved" },
        started.elapsed().as_secs_f32(),
        if errors > 0 { format!(", {errors} errors") } else { String::new() },
    );

    if errors > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

fn describe(o: &Outcome) -> String {
    match o {
        Outcome::Compressed { before, after, quality, dssim } => format!(
            "{} -> {} (-{:.0}%) q={quality} dssim={dssim:.5}",
            format_size(*before),
            format_size(*after),
            (before - after) as f64 * 100.0 / *before as f64
        ),
        Outcome::AlreadyProcessed { .. } => "already processed".into(),
        Outcome::NoGain { size, best } => match best {
            Some(b) => format!("no gain ({} -> {})", format_size(*size), format_size(*b)),
            None => "no gain (no quality passed)".into(),
        },
        Outcome::Skipped { reason, .. } => format!("skipped: {reason}"),
    }
}

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("error: {msg}\n\n{USAGE}");
    ExitCode::from(2)
}
