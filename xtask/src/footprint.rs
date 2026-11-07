//! `cargo xtask footprint [--bench] [--out FILE]`: the numbers a release records.
//!
//! A gate on shared CI runners produces flaky failures, not information, so these are
//! *recorded* per release rather than enforced: the `no_std` build size of the feature sets a
//! firmware would actually pick, and — with `--bench` — the criterion medians of
//! `benches/hot_paths.rs`. Each number carries the toolchain and target it was measured with,
//! because a compiler upgrade moves them as much as a code change does.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

const TARGET: &str = "thumbv7em-none-eabihf";

/// The feature sets measured: what a firmware for one version and one role would enable.
const FEATURE_SETS: &[(&str, &str)] = &[
    ("1.6, station", "v1_6,station"),
    ("2.0.1, station", "v2_0_1,station"),
    ("2.1, station", "v2_1,station"),
    (
        "all versions, both roles, signed messages",
        "v1_6,v2_0_1,v2_1,station,csms,signed-messages,jws-es256",
    ),
];

pub fn run(root: &Path, args: &[String]) -> Result<()> {
    let bench = args.iter().any(|a| a == "--bench");
    let out = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1));

    let rustc = capture(Command::new("rustc").arg("--version"))?;
    let mut report = String::new();
    let _ = writeln!(report, "## Footprint\n");
    let _ = writeln!(report, "Measured with `{}`.\n", rustc.trim());
    let size_tool = llvm_size();
    let _ = writeln!(
        report,
        "`no_std` release build of the `ocpp-kit` rlib for `{TARGET}` (`opt-level = \"s\"`): the \
         sections of its object files, by `llvm-size`. An upper bound on what a firmware links — \
         the linker keeps only what is reached, and generic code is instantiated downstream.\n"
    );
    if size_tool.is_some() {
        let _ = writeln!(
            report,
            "| Features | text | data | bss |\n|---|---:|---:|---:|"
        );
    } else {
        let _ = writeln!(
            report,
            "(`llvm-size` not found — `rustup component add llvm-tools` — so whole rlib bytes, \
             metadata included.)\n\n| Features | rlib bytes |\n|---|---:|"
        );
    }

    let target_dir = root.join("target").join("footprint");
    for (label, features) in FEATURE_SETS {
        let status = Command::new("cargo")
            .current_dir(root)
            .env("CARGO_PROFILE_RELEASE_OPT_LEVEL", "s")
            .args([
                "build",
                "--release",
                "-p",
                "ocpp-kit",
                "--no-default-features",
            ])
            .args(["--features", features, "--target", TARGET])
            .arg("--target-dir")
            .arg(&target_dir)
            .status()
            .context("running cargo build")?;
        if !status.success() {
            bail!("the no_std build failed for features {features}");
        }
        let rlib = target_dir
            .join(TARGET)
            .join("release")
            .join("libocpp_kit.rlib");
        if let Some(tool) = &size_tool {
            let (text, data, bss) = sections(tool, &rlib)?;
            let _ = writeln!(report, "| {label} | {text} | {data} | {bss} |");
        } else {
            let bytes = std::fs::metadata(&rlib)
                .with_context(|| format!("reading {}", rlib.display()))?
                .len();
            let _ = writeln!(report, "| {label} | {bytes} |");
        }
    }

    if bench {
        let status = Command::new("cargo")
            .current_dir(root)
            .args(["bench", "--bench", "hot_paths", "--features", "full"])
            .status()
            .context("running cargo bench")?;
        if !status.success() {
            bail!("cargo bench failed");
        }
        let _ = writeln!(
            report,
            "\nHot paths (`benches/hot_paths.rs`, criterion median on this runner):\n"
        );
        let _ = writeln!(report, "| Benchmark | median |\n|---|---:|");
        let criterion = root.join("target").join("criterion");
        let mut rows = Vec::new();
        collect_estimates(&criterion, &criterion, &mut rows)?;
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, nanos) in rows {
            let _ = writeln!(report, "| `{name}` | {} |", human(nanos));
        }
    }

    print!("{report}");
    if let Some(out) = out {
        std::fs::write(out, &report).with_context(|| format!("writing {out}"))?;
    }
    Ok(())
}

/// Finds every `new/estimates.json` criterion wrote and reads its median, in nanoseconds.
fn collect_estimates(base: &Path, dir: &Path, rows: &mut Vec<(String, f64)>) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        let estimates = path.join("new").join("estimates.json");
        if estimates.is_file() {
            let json: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&estimates)?)?;
            if let Some(median) = json
                .pointer("/median/point_estimate")
                .and_then(serde_json::Value::as_f64)
            {
                let name = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                rows.push((name, median));
            }
        } else if path.file_name().is_some_and(|n| n != "report") {
            collect_estimates(base, &path, rows)?;
        }
    }
    Ok(())
}

fn human(nanos: f64) -> String {
    if nanos >= 1_000_000.0 {
        format!("{:.2} ms", nanos / 1_000_000.0)
    } else if nanos >= 1_000.0 {
        format!("{:.2} µs", nanos / 1_000.0)
    } else {
        format!("{nanos:.0} ns")
    }
}

/// `llvm-size` from the toolchain's `llvm-tools` component, if installed.
fn llvm_size() -> Option<std::path::PathBuf> {
    let sysroot = capture(Command::new("rustc").args(["--print", "sysroot"])).ok()?;
    let host = capture(Command::new("rustc").args(["--print", "host-tuple"])).ok()?;
    let tool = Path::new(sysroot.trim())
        .join("lib/rustlib")
        .join(host.trim())
        .join("bin/llvm-size");
    tool.is_file().then_some(tool)
}

/// The `(TOTALS)` row of `llvm-size -B -t`: text, data, bss over every object in the rlib.
fn sections(tool: &Path, rlib: &Path) -> Result<(u64, u64, u64)> {
    let out = capture(Command::new(tool).args(["-B", "-t"]).arg(rlib))?;
    let totals = out
        .lines()
        .find(|line| line.contains("(TOTALS)"))
        .context("llvm-size printed no totals")?;
    let numbers: Vec<u64> = totals
        .split_whitespace()
        .take(3)
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .context("parsing llvm-size totals")?;
    Ok((numbers[0], numbers[1], numbers[2]))
}

fn capture(command: &mut Command) -> Result<String> {
    let output = command.output().context("running a tool")?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
