//! `cargo orbvane package [--target arm64|x64|all]... [--out <file.vsix>] [<extension folder>]`
//! `cargo orbvane registry [<extensions.json>] --download-base <url> [--previous <index.json>] [--out <folder>]`

use std::path::PathBuf;

use cargo_orbvane::{build, crate_info, pack, Arch};

const USAGE: &str = "Usage: cargo orbvane package [options] [<extension folder>]
       cargo orbvane registry [<extensions.json>] --download-base <url> [--previous <index.json>] [--out <folder>]

Builds the extension's program in release mode and packs it into a .vsix
that Orbvane installs (Extensions: Install from VSIX...).

Options:
  --target <arch>   arm64, x64 or all (repeatable; default: this Mac's)
  -o, --out <file>  where to write the package (default: <name>-<version>.vsix
                    in the extension's folder)
  -h, --help        show this

`registry` is what the extension registry's CI runs: it builds the extensions listed
in extensions.json from source (for arm64 and x64) and writes the packages and
index.json to --out (default dist).";

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    // Cargo runs `cargo-orbvane orbvane package ...`; also allow running it directly.
    let mut args = std::env::args().skip(1).peekable();
    if args.peek().map(String::as_str) == Some("orbvane") {
        args.next();
    }
    match args.next().as_deref() {
        Some("package") => {}
        Some("registry") => return cargo_orbvane::registry::run(args),
        Some("-h" | "--help") | None => {
            println!("{USAGE}");
            return Ok(());
        }
        Some(other) => return Err(format!("unknown command `{other}`\n\n{USAGE}")),
    }
    let (mut archs, mut out, mut dir) = (Vec::new(), None, None);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--target" => match args.next().as_deref() {
                Some("all") => archs.extend([Arch::Arm64, Arch::X64]),
                Some(a) => archs.push(Arch::parse(a).ok_or_else(|| format!("unknown target `{a}` (arm64, x64 or all)"))?),
                None => return Err("--target needs a value".into()),
            },
            "-o" | "--out" => out = Some(PathBuf::from(args.next().ok_or("--out needs a file")?)),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            a if a.starts_with('-') => return Err(format!("unknown option `{a}`\n\n{USAGE}")),
            a => dir = Some(PathBuf::from(a)),
        }
    }
    if archs.is_empty() {
        archs.push(Arch::host());
    }
    archs.dedup();
    let dir = dir.unwrap_or_else(|| PathBuf::from("."));
    let krate = crate_info(&dir)?;
    let mut programs = Vec::new();
    for arch in archs {
        programs.push((arch, build(&dir, &krate, arch)?));
    }
    let vsix = pack(&dir, Some(&krate.bin), &programs, out.as_deref())?;
    let size = std::fs::metadata(&vsix).map(|m| m.len()).unwrap_or(0);
    println!("Packaged {} ({:.1} MB, {})", vsix.display(), size as f64 / 1e6, programs.iter().map(|(a, _)| a.name()).collect::<Vec<_>>().join(", "));
    Ok(())
}
