//! `cargo orbvane package`: builds an extension's program in release mode for each Mac
//! architecture and packs it with its `package.json` and resources into a `.vsix` package (a zip
//! with the extension under `extension/`). The packaged `package.json` gets `orbvane.main` =
//! `bin/${arch}/<program>`, which the editor expands to the running Mac's architecture.
//!
//! Everything in the extension's folder goes in except what `.orbvaneignore` (gitignore syntax)
//! leaves out; the sources, `target/` and Cargo's files are left out unless re-included.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

pub mod registry;

/// A Mac architecture, named as in `${arch}`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Arch {
    Arm64,
    X64,
}

impl Arch {
    pub fn parse(s: &str) -> Option<Arch> {
        match s {
            "arm64" | "aarch64" | "aarch64-apple-darwin" => Some(Arch::Arm64),
            "x64" | "x86_64" | "x86_64-apple-darwin" => Some(Arch::X64),
            _ => None,
        }
    }

    pub fn host() -> Arch {
        if cfg!(target_arch = "aarch64") { Arch::Arm64 } else { Arch::X64 }
    }

    pub fn name(self) -> &'static str {
        match self {
            Arch::Arm64 => "arm64",
            Arch::X64 => "x64",
        }
    }

    pub fn triple(self) -> &'static str {
        match self {
            Arch::Arm64 => "aarch64-apple-darwin",
            Arch::X64 => "x86_64-apple-darwin",
        }
    }
}

/// Left out unless `.orbvaneignore` re-includes them (`!src/data.json`).
const DEFAULT_IGNORE: &str = "/target/\n/src/\n/tests/\n/benches/\n/examples/\nCargo.toml\nCargo.lock\n.git*\n.orbvaneignore\n*.vsix\n.DS_Store\n";

/// The extension's Cargo package: its program's name and where Cargo builds it.
pub struct Crate {
    pub bin: String,
    pub target_dir: PathBuf,
}

/// Reads `dir/Cargo.toml` through `cargo metadata`.
pub fn crate_info(dir: &Path) -> Result<Crate, String> {
    let manifest = dir.join("Cargo.toml");
    let out = Command::new(cargo())
        .args(["metadata", "--no-deps", "--format-version", "1", "--manifest-path"])
        .arg(&manifest)
        .output()
        .map_err(|e| format!("couldn't run cargo: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let meta: Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    let manifest = manifest.canonicalize().map_err(|e| format!("{}: {e}", manifest.display()))?;
    let package = meta["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["manifest_path"].as_str().is_some_and(|m| Path::new(m) == manifest))
        .ok_or("cargo metadata didn't list the extension's package")?;
    let bin = package["targets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["kind"].as_array().is_some_and(|k| k.iter().any(|k| k == "bin")))
        .and_then(|t| t["name"].as_str())
        .ok_or_else(|| format!("{} has no binary target", manifest.display()))?;
    let target_dir = meta["target_directory"].as_str().ok_or("cargo metadata has no target directory")?;
    Ok(Crate { bin: bin.to_string(), target_dir: target_dir.into() })
}

/// `cargo build --release` for `arch`; returns the program.
pub fn build(dir: &Path, krate: &Crate, arch: Arch) -> Result<PathBuf, String> {
    let status = Command::new(cargo())
        .args(["build", "--release", "--bin", &krate.bin, "--target", arch.triple(), "--manifest-path"])
        .arg(dir.join("Cargo.toml"))
        .status()
        .map_err(|e| format!("couldn't run cargo: {e}"))?;
    if !status.success() {
        return Err(format!("building for {} failed (is the target installed? rustup target add {})", arch.name(), arch.triple()));
    }
    Ok(krate.target_dir.join(arch.triple()).join("release").join(&krate.bin))
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

/// The extension's `package.json` as it is packaged: pointing at the programs (`bin`, when it
/// has any).
pub fn packaged_manifest(dir: &Path, bin: Option<&str>) -> Result<Value, String> {
    let text = std::fs::read_to_string(dir.join("package.json")).map_err(|e| format!("{}: {e}", dir.join("package.json").display()))?;
    let mut manifest: Value = serde_json::from_str(&text).map_err(|e| format!("package.json: {e}"))?;
    set_program(&mut manifest, bin);
    Ok(manifest)
}

/// Packs the extension in `dir` with `programs` (one per architecture, named `bin`; none for an
/// extension that only contributes) into `out` (default `<dir>/<name>-<version>.vsix`). Returns
/// the package's path.
pub fn pack(dir: &Path, bin: Option<&str>, programs: &[(Arch, PathBuf)], out: Option<&Path>) -> Result<PathBuf, String> {
    let manifest = packaged_manifest(dir, bin.filter(|_| !programs.is_empty()))?;
    let field = |m: &Value, k: &str| m[k].as_str().filter(|s| !s.is_empty()).map(String::from).ok_or(format!("package.json needs a \"{k}\""));
    let (name, version) = (field(&manifest, "name")?, field(&manifest, "version")?);
    field(&manifest, "publisher")?;
    let out = match out {
        Some(o) => o.to_path_buf(),
        None => dir.join(format!("{name}-{version}.vsix")),
    };
    let out = std::path::absolute(&out).map_err(|e| e.to_string())?;

    let stage = std::env::temp_dir().join(format!("cargo-orbvane-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    let result = (|| {
        let ext = stage.join("extension");
        std::fs::create_dir_all(&ext).map_err(|e| e.to_string())?;
        let ignore = search::Gitignore::parse(&format!("{DEFAULT_IGNORE}{}", std::fs::read_to_string(dir.join(".orbvaneignore")).unwrap_or_default()));
        let mut files = Vec::new();
        collect(dir, dir, &ignore, &mut files)?;
        for rel in &files {
            if rel == "package.json" {
                continue;
            }
            let to = ext.join(rel);
            std::fs::create_dir_all(to.parent().unwrap()).map_err(|e| e.to_string())?;
            std::fs::copy(dir.join(rel), &to).map_err(|e| format!("{rel}: {e}"))?;
        }
        for (arch, program) in programs {
            let to = ext.join("bin").join(arch.name()).join(bin.unwrap_or("main"));
            std::fs::create_dir_all(to.parent().unwrap()).map_err(|e| e.to_string())?;
            std::fs::copy(program, &to).map_err(|e| format!("{}: {e}", program.display()))?;
        }
        let json = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
        std::fs::write(ext.join("package.json"), json + "\n").map_err(|e| e.to_string())?;

        let _ = std::fs::remove_file(&out);
        let zipped = Command::new("/usr/bin/zip").args(["-q", "-r", "-X"]).arg(&out).arg(".").current_dir(&stage).output().map_err(|e| format!("couldn't run zip: {e}"))?;
        if !zipped.status.success() {
            return Err(String::from_utf8_lossy(&zipped.stderr).trim().to_string());
        }
        Ok(out)
    })();
    let _ = std::fs::remove_dir_all(&stage);
    result
}

/// Points the manifest at the packaged programs. A `main` that isn't JavaScript was the
/// development build (`target/debug/...`), so it goes.
fn set_program(manifest: &mut Value, bin: Option<&str>) {
    let Value::Object(m) = manifest else { return };
    if m.get("main").and_then(Value::as_str).is_some_and(|main| !main.ends_with(".js")) {
        m.remove("main");
    }
    let Some(bin) = bin else { return };
    let orbvane = m.entry("orbvane").or_insert_with(|| Value::Object(Default::default()));
    if let Value::Object(r) = orbvane {
        r.insert("main".into(), Value::String(format!("bin/${{arch}}/{bin}")));
    }
}

/// The files to pack, relative to `root` with `/`, sorted.
fn collect(root: &Path, dir: &Path, ignore: &search::Gitignore, out: &mut Vec<String>) -> Result<(), String> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
        if path.is_dir() {
            // Not pruned (like `vsce`, a file can be re-included from an ignored folder), except
            // what can be big.
            if !matches!(rel.as_str(), "target" | ".git" | "node_modules") {
                collect(root, &path, ignore, out)?;
            }
        } else if !ignored(ignore, &rel) {
            out.push(rel);
        }
    }
    Ok(())
}

/// A rule for the file itself wins; else the nearest folder's.
fn ignored(ignore: &search::Gitignore, rel: &str) -> bool {
    if let Some(v) = ignore.check(rel, false) {
        return v;
    }
    let mut dir = rel;
    while let Some((parent, _)) = dir.rsplit_once('/') {
        if let Some(v) = ignore.check(parent, true) {
            return v;
        }
        dir = parent;
    }
    false
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_an_extension() {
        let tmp = std::env::temp_dir().join(format!("cargo-orbvane-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let dir = tmp.join("ext");
        for (path, text) in [
            ("package.json", r#"{ "name": "demo", "publisher": "me", "version": "1.2.3", "main": "../target/debug/demo", "contributes": {} }"#),
            ("Cargo.toml", "[package]\nname = \"demo\"\n"),
            ("src/main.rs", "fn main() {}"),
            ("src/data.json", "{}"),
            ("README.md", "# Demo"),
            ("resources/icon.svg", "<svg/>"),
            ("notes/draft.md", "x"),
            ("target/debug/demo", "debug build"),
            (".orbvaneignore", "notes/\n!src/data.json\n"),
        ] {
            std::fs::create_dir_all(dir.join(path).parent().unwrap()).unwrap();
            std::fs::write(dir.join(path), text).unwrap();
        }
        let program = tmp.join("demo");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();

        let ignore = search::Gitignore::parse(&format!("{DEFAULT_IGNORE}{}", std::fs::read_to_string(dir.join(".orbvaneignore")).unwrap()));
        let mut files = Vec::new();
        collect(&dir, &dir, &ignore, &mut files).unwrap();
        assert_eq!(files, ["README.md", "package.json", "resources/icon.svg", "src/data.json"]);

        let vsix = pack(&dir, Some("demo"), &[(Arch::host(), program)], None).unwrap();
        assert_eq!(vsix, dir.join("demo-1.2.3.vsix"));

        // The editor installs it, and finds the program for this Mac.
        let mut registry = extensions::Registry::scan(&tmp.join("installed"));
        let id = registry.install_vsix(&vsix).unwrap();
        assert_eq!(id, "me.demo");
        let e = registry.get(&id).unwrap();
        assert_eq!(e.manifest["main"], Value::Null);
        assert_eq!(e.program(), Some(e.path.join(format!("bin/{}/demo", Arch::host().name()))));
        assert!(e.program().unwrap().exists());
        assert!(e.path.join("resources/icon.svg").exists() && !e.path.join("src/main.rs").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
