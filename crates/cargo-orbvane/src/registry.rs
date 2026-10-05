//! `cargo orbvane registry`: what the registry's CI runs. The registry is a git repository
//! whose `extensions.json` lists submitted extensions by id, each a repository and a commit:
//!
//! ```json
//! { "extensions": { "orbvane.todo-tree": { "repository": "https://github.com/...", "rev": "<40-hex commit>", "path": "optional/subfolder" } } }
//! ```
//!
//! Each new or changed entry is cloned at that commit, built from source for every Mac
//! architecture and packed (like `cargo orbvane package`); the package, README, packaged
//! `package.json` and icon go to `<out>/<id>-<version>/`, to be uploaded as the release
//! `<id>-<version>` (so `<download base>/<id>-<version>/<file>` serves them). `<out>/index.json`
//! is the new index (see `extensions::catalog`). Entries whose commit hasn't changed are copied
//! from the previous index without building.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

use crate::{build, crate_info, pack, packaged_manifest, Arch};

/// An entry of `extensions.json`.
#[derive(Clone, Debug, PartialEq)]
pub struct Submission {
    pub id: String,
    pub repository: String,
    pub rev: String,
    pub path: Option<String>,
}

pub fn read_submissions(text: &str) -> Result<Vec<Submission>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("extensions.json: {e}"))?;
    let map = v["extensions"].as_object().ok_or("extensions.json needs an \"extensions\" object")?;
    let mut out = Vec::new();
    for (id, e) in map {
        let field = |k: &str| e[k].as_str().map(String::from).ok_or_else(|| format!("{id}: needs \"{k}\""));
        let rev = field("rev")?;
        if rev.len() != 40 || !rev.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("{id}: \"rev\" must be a full commit hash, so the build is pinned to it"));
        }
        if id.split('.').count() != 2 || id.chars().any(|c| c.is_ascii_uppercase()) {
            return Err(format!("{id}: ids are lowercase \"publisher.name\""));
        }
        let path = e["path"].as_str().map(String::from).filter(|p| !p.is_empty());
        if path.as_deref().is_some_and(|p| p.starts_with('/') || p.split('/').any(|c| c == "..")) {
            return Err(format!("{id}: \"path\" must be a folder inside the repository"));
        }
        out.push(Submission { id: id.clone(), repository: field("repository")?, rev, path });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Builds what changed since `previous` (an index) and returns the new index.
pub fn build_registry(submissions: &[Submission], previous: Option<&Value>, out: &Path, download_base: &str, archs: &[Arch]) -> Result<Value, String> {
    std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let old: Vec<&Value> = previous.and_then(|p| p["extensions"].as_array()).into_iter().flatten().collect();
    let old_entry = |id: &str| old.iter().find(|e| entry_id(e) == id).copied();
    let mut entries = Vec::new();
    let mut failures = Vec::new();
    for s in submissions {
        match old_entry(&s.id) {
            Some(e) if e["rev"].as_str() == Some(&s.rev) => entries.push(e.clone()),
            prev => match build_one(s, prev, out, download_base, archs) {
                Ok(e) => {
                    eprintln!("built {} {}", s.id, e["version"].as_str().unwrap_or(""));
                    entries.push(e);
                }
                Err(err) => failures.push(format!("{}: {err}", s.id)),
            },
        }
    }
    if !failures.is_empty() {
        return Err(failures.join("\n"));
    }
    let index = json!({ "version": 1, "extensions": entries });
    std::fs::write(out.join("index.json"), serde_json::to_string_pretty(&index).unwrap() + "\n").map_err(|e| e.to_string())?;
    Ok(index)
}

fn entry_id(e: &Value) -> String {
    format!("{}.{}", e["namespace"].as_str().unwrap_or(""), e["name"].as_str().unwrap_or("")).to_lowercase()
}

fn git(args: &[&str], dir: Option<&Path>) -> Result<(), String> {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.arg("-C").arg(d);
    }
    let out = cmd.args(args).output().map_err(|e| format!("couldn't run git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(())
}

fn build_one(s: &Submission, prev: Option<&Value>, out: &Path, download_base: &str, archs: &[Arch]) -> Result<Value, String> {
    let work = std::env::temp_dir().join(format!("orbvane-registry-{}-{}", std::process::id(), s.id));
    let _ = std::fs::remove_dir_all(&work);
    let result = (|| {
        git(&["clone", "--quiet", &s.repository, &work.to_string_lossy()], None)?;
        git(&["checkout", "--quiet", "--detach", &s.rev], Some(&work))?;
        let dir = match &s.path {
            Some(p) => work.join(p),
            None => work.clone(),
        };
        let original: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).map_err(|e| format!("package.json: {e}"))?).map_err(|e| format!("package.json: {e}"))?;
        let found = format!("{}.{}", original["publisher"].as_str().unwrap_or(""), original["name"].as_str().unwrap_or("")).to_lowercase();
        if found != s.id {
            return Err(format!("its package.json is {found}, not {}", s.id));
        }
        if original["engines"]["orbvane"].as_str().is_none() {
            return Err("package.json needs \"engines\": { \"orbvane\": \"^x.y.z\" } (this registry is for extensions written for Orbvane)".into());
        }
        let version = original["version"].as_str().unwrap_or("").to_string();
        if let Some(prev) = prev.and_then(|p| p["version"].as_str()) {
            if !extensions::is_newer(&version, prev) {
                return Err(format!("version {version} isn't newer than the published {prev}; bump it"));
            }
        }
        // A program to build, or only contributions.
        let (bin, programs) = if dir.join("Cargo.toml").exists() {
            let krate = crate_info(&dir)?;
            let programs = archs.iter().map(|&a| build(&dir, &krate, a).map(|p| (a, p))).collect::<Result<Vec<_>, _>>()?;
            (Some(krate.bin), programs)
        } else {
            (None, Vec::new())
        };
        let tag = format!("{}-{version}", s.id);
        let dest = out.join(&tag);
        std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
        let vsix = pack(&dir, bin.as_deref(), &programs, Some(&dest.join(format!("{tag}.vsix"))))?;
        let manifest = packaged_manifest(&dir, bin.as_deref().filter(|_| !programs.is_empty()))?;
        std::fs::write(dest.join("package.json"), serde_json::to_string_pretty(&manifest).unwrap()).map_err(|e| e.to_string())?;
        let url = |file: &str| format!("{}/{tag}/{file}", download_base.trim_end_matches('/'));
        let mut files = json!({
            "download": url(&format!("{tag}.vsix")),
            "sha256": extensions::gallery::sha256_of(&vsix)?,
            "manifest": url("package.json"),
        });
        if let Some(readme) = ["README.md", "readme.md", "Readme.md"].iter().map(|n| dir.join(n)).find(|p| p.exists()) {
            std::fs::copy(&readme, dest.join("README.md")).map_err(|e| e.to_string())?;
            files["readme"] = json!(url("README.md"));
        }
        if let Some(icon) = manifest["icon"].as_str().map(|i| dir.join(i)).filter(|p| p.exists()) {
            let name = format!("icon.{}", icon.extension().and_then(|e| e.to_str()).unwrap_or("png"));
            std::fs::copy(&icon, dest.join(&name)).map_err(|e| e.to_string())?;
            files["icon"] = json!(url(&name));
        }
        let text = |k: &str| manifest[k].clone();
        let publisher = manifest["publisher"].as_str().unwrap_or("");
        Ok(json!({
            "namespace": publisher,
            "name": manifest["name"],
            "version": version,
            "displayName": manifest["displayName"].as_str().or(manifest["name"].as_str()),
            "description": text("description"),
            "namespaceDisplayName": publisher,
            "categories": text("categories"),
            "keywords": text("keywords"),
            "license": text("license"),
            "repository": s.repository,
            "rev": s.rev,
            "path": s.path,
            "engines": text("engines"),
            "timestamp": now_iso(),
            "files": files,
        }))
    })();
    let _ = std::fs::remove_dir_all(&work);
    result
}

/// The current time as `2026-10-04T12:34:56Z`.
fn now_iso() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Runs `cargo orbvane registry` (arguments after `registry`).
pub fn run(args: impl Iterator<Item = String>) -> Result<(), String> {
    let (mut list, mut previous, mut out, mut base, mut archs) = (None, None, PathBuf::from("dist"), None, Vec::new());
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--previous" => previous = Some(PathBuf::from(args.next().ok_or("--previous needs a file")?)),
            "--out" => out = PathBuf::from(args.next().ok_or("--out needs a folder")?),
            "--download-base" => base = Some(args.next().ok_or("--download-base needs a URL")?),
            "--target" => match args.next().as_deref() {
                Some("all") => archs.extend([Arch::Arm64, Arch::X64]),
                Some(a) => archs.push(Arch::parse(a).ok_or_else(|| format!("unknown target `{a}`"))?),
                None => return Err("--target needs a value".into()),
            },
            a if a.starts_with('-') => return Err(format!("unknown option `{a}`")),
            a => list = Some(PathBuf::from(a)),
        }
    }
    let list = list.unwrap_or_else(|| PathBuf::from("extensions.json"));
    let base = base.ok_or("--download-base is required (where the releases' files are served)")?;
    if archs.is_empty() {
        archs = vec![Arch::Arm64, Arch::X64];
    }
    archs.dedup();
    let submissions = read_submissions(&std::fs::read_to_string(&list).map_err(|e| format!("{}: {e}", list.display()))?)?;
    let previous: Option<Value> = match previous.filter(|p| p.exists()) {
        Some(p) => Some(serde_json::from_str(&std::fs::read_to_string(&p).map_err(|e| e.to_string())?).map_err(|e| format!("{}: {e}", p.display()))?),
        None => None,
    };
    let index = build_registry(&submissions, previous.as_ref(), &out, &base, &archs)?;
    println!("Wrote {} ({} extensions)", out.join("index.json").display(), index["extensions"].as_array().map_or(0, Vec::len));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn reads_submissions() {
        let rev = "a".repeat(40);
        let ok = format!(r#"{{ "extensions": {{ "me.b": {{ "repository": "r", "rev": "{rev}" }}, "me.a": {{ "repository": "r", "rev": "{rev}", "path": "x" }} }} }}"#);
        let s = read_submissions(&ok).unwrap();
        assert_eq!((s[0].id.as_str(), s[0].path.as_deref()), ("me.a", Some("x")));
        assert!(read_submissions(r#"{ "extensions": { "me.a": { "repository": "r", "rev": "main" } } }"#).unwrap_err().contains("full commit"));
        let up = format!(r#"{{ "extensions": {{ "Me.A": {{ "repository": "r", "rev": "{rev}" }} }} }}"#);
        assert!(read_submissions(&up).is_err());
        let escape = format!(r#"{{ "extensions": {{ "me.a": {{ "repository": "r", "rev": "{rev}", "path": "../x" }} }} }}"#);
        assert!(read_submissions(&escape).is_err());
        assert_eq!(now_iso().len(), 20);
    }

    /// A contribution-only extension in a local git repository: built into an index, then
    /// skipped while its commit stays, and refused when a new commit keeps the version.
    #[test]
    fn builds_an_index() {
        let root = std::env::temp_dir().join(format!("orbvane-registry-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        let ext = repo.join("themes/dusk");
        std::fs::create_dir_all(&ext).unwrap();
        let manifest = |v: &str| format!(r#"{{ "name": "dusk", "publisher": "me", "version": "{v}", "displayName": "Dusk", "description": "A theme.", "categories": ["Themes"], "engines": {{ "orbvane": "^0.1.0" }}, "icon": "icon.png" }}"#);
        std::fs::write(ext.join("package.json"), manifest("1.0.0")).unwrap();
        std::fs::write(ext.join("README.md"), "# Dusk").unwrap();
        std::fs::write(ext.join("icon.png"), "png").unwrap();
        run_git(&repo, &["init", "-q"]);
        run_git(&repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "x"]);
        run_git(&repo, &["add", "."]);
        run_git(&repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "dusk"]);
        let rev = run_git(&repo, &["rev-parse", "HEAD"]);
        let sub = Submission { id: "me.dusk".into(), repository: repo.to_string_lossy().into(), rev: rev.clone(), path: Some("themes/dusk".into()) };

        let out = root.join("dist");
        let index = build_registry(std::slice::from_ref(&sub), None, &out, "https://example.com/releases/download", &[Arch::host()]).unwrap();
        let e = &index["extensions"][0];
        assert_eq!((e["namespace"].as_str(), e["version"].as_str(), e["rev"].as_str()), (Some("me"), Some("1.0.0"), Some(rev.as_str())));
        assert_eq!(e["files"]["download"], "https://example.com/releases/download/me.dusk-1.0.0/me.dusk-1.0.0.vsix");
        assert_eq!(e["files"]["readme"], "https://example.com/releases/download/me.dusk-1.0.0/README.md");
        for f in ["me.dusk-1.0.0.vsix", "README.md", "package.json", "icon.png"] {
            assert!(out.join("me.dusk-1.0.0").join(f).exists(), "{f}");
        }
        assert_eq!(e["files"]["sha256"].as_str().unwrap(), extensions::gallery::sha256_of(&out.join("me.dusk-1.0.0/me.dusk-1.0.0.vsix")).unwrap());
        // What the editor reads.
        let listed = extensions::catalog::parse(&index).unwrap();
        assert_eq!((listed[0].id().as_str(), listed[0].categories.clone()), ("me.dusk", vec!["Themes".to_string()]));

        // Same commit: copied, not rebuilt.
        let out2 = root.join("dist2");
        let again = build_registry(std::slice::from_ref(&sub), Some(&index), &out2, "https://example.com/releases/download", &[Arch::host()]).unwrap();
        assert_eq!(again["extensions"][0], index["extensions"][0]);
        assert!(!out2.join("me.dusk-1.0.0").exists());

        // A new commit with the same version is refused; with a new version it's built.
        std::fs::write(ext.join("README.md"), "# Dusk!").unwrap();
        run_git(&repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-am", "readme"]);
        let sub2 = Submission { rev: run_git(&repo, &["rev-parse", "HEAD"]), ..sub.clone() };
        let err = build_registry(std::slice::from_ref(&sub2), Some(&index), &out2, "https://x", &[Arch::host()]).unwrap_err();
        assert!(err.contains("isn't newer"), "{err}");
        std::fs::write(ext.join("package.json"), manifest("1.1.0")).unwrap();
        run_git(&repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-am", "1.1.0"]);
        let sub3 = Submission { rev: run_git(&repo, &["rev-parse", "HEAD"]), ..sub.clone() };
        let newer = build_registry(&[sub3], Some(&index), &out2, "https://x", &[Arch::host()]).unwrap();
        assert_eq!(newer["extensions"][0]["version"], "1.1.0");

        // Ids must match the package.json, and the extension must be for Orbvane.
        let wrong = Submission { id: "me.other".into(), ..sub.clone() };
        assert!(build_registry(&[wrong], None, &root.join("dist3"), "https://x", &[Arch::host()]).unwrap_err().contains("not me.other"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
