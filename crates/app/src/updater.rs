//! Updates of the app itself. `latest` asks GitHub for the newest release; `download` fetches
//! its disk image and checks it against the SHA-256 GitHub publishes; `prepare` mounts the image
//! and copies the app out, after checking that it's signed by the same team (with the same
//! bundle id) as the running app, so a tampered image is refused; `swap` puts it in place of the
//! running app (asking for an administrator's password when its folder isn't writable) and
//! `relaunch` starts it once we've quit. Everything here blocks: the workbench runs it on
//! threads (`workbench/updates.rs`).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

/// The latest release of the app (GitHub's API); `ORBVANE_UPDATE_URL` points elsewhere.
const LATEST: &str = "https://api.github.com/repos/sbaruwal/orbvane/releases/latest";
pub const BUNDLE_ID: &str = "dev.orbvane.ide";
const APP: &str = "Orbvane.app";
/// Next to the running app: the update waiting to be swapped in, and the app it replaces.
const STAGED: &str = ".Orbvane-update.app";
const OLD: &str = ".Orbvane-old.app";

pub fn latest_url() -> String {
    std::env::var("ORBVANE_UPDATE_URL").unwrap_or_else(|_| LATEST.to_string())
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    /// "0.1.1" (the tag without its "v").
    pub version: String,
    /// The disk image.
    pub url: String,
    /// Its SHA-256 in hex, when the release lists one.
    pub sha256: Option<String>,
    /// The release's web page (its notes).
    pub page: String,
}

impl Release {
    /// A release as GitHub's API describes it; its disk image is the asset `Orbvane-<version>.dmg`.
    pub fn parse(v: &Value) -> Result<Release, String> {
        let tag = v["tag_name"].as_str().ok_or("The release has no version")?;
        let version = tag.trim_start_matches('v').to_string();
        let name = format!("Orbvane-{version}.dmg");
        let asset = v["assets"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|a| a["name"].as_str() == Some(name.as_str()))
            .ok_or_else(|| format!("Release {tag} has no {name}"))?;
        let url = asset["browser_download_url"].as_str().ok_or_else(|| format!("{name} has no download address"))?.to_string();
        let sha256 = asset["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).map(str::to_lowercase);
        Ok(Release { version, url, sha256, page: v["html_url"].as_str().unwrap_or_default().to_string() })
    }
}

/// The latest release.
pub fn latest(url: &str) -> Result<Release, String> {
    let bytes = extensions::gallery::get(url)?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| format!("{url}: {e}"))?;
    Release::parse(&v)
}

/// Whether version `a` is newer than `b` ("0.1.10" > "0.1.9"; anything after a '-' is ignored).
pub fn is_newer(a: &str, b: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.split('-').next().unwrap_or("").split('.').map(|p| p.parse().unwrap_or(0)).collect()
    }
    let (a, b) = (parts(a), parts(b));
    let n = a.len().max(b.len());
    let pad = |v: &[u64]| (0..n).map(|i| v.get(i).copied().unwrap_or(0)).collect::<Vec<_>>();
    pad(&a) > pad(&b)
}

/// The app bundle the running program is in (None for a bare binary, as with `cargo run`).
pub fn running_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let macos = exe.parent()?;
    let app = macos.parent()?.parent()?;
    (macos.file_name()? == "MacOS" && app.extension()? == "app").then(|| app.to_path_buf())
}

/// The team that signed `app` (None when it's unsigned or signed ad hoc).
pub fn team_of(app: &Path) -> Option<String> {
    let out = Command::new("/usr/bin/codesign").args(["-dv", "--verbose=2"]).arg(app).output().ok()?;
    let text = String::from_utf8_lossy(&out.stderr);
    text.lines().find_map(|l| l.strip_prefix("TeamIdentifier=")).map(str::trim).filter(|t| !t.is_empty() && *t != "not set").map(String::from)
}

/// The code requirement an update must meet: Apple-issued certificate, our bundle id, `team`.
pub fn requirement(team: &str) -> String {
    format!("anchor apple generic and identifier \"{BUNDLE_ID}\" and certificate leaf[subject.OU] = \"{team}\"")
}

/// Why the app at `app` can't install updates itself, if it can't (`signed`: whether we know
/// whose signature an update must carry): the user is told to download the update instead.
pub fn blocker(app: Option<&Path>, signed: bool) -> Option<String> {
    let Some(app) = app else { return Some("This copy of Orbvane isn't an app bundle.".into()) };
    let path = app.to_string_lossy();
    if path.contains("/AppTranslocation/") || path.starts_with("/Volumes/") {
        return Some("Move Orbvane to the Applications folder to install updates.".into());
    }
    if !signed {
        return Some("This copy of Orbvane isn't signed, so updates can't be verified.".into());
    }
    None
}

/// Whether we can create files in `dir`.
fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".orbvane-write-test-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// Where the update waits for `target` (the running app) to be replaced: next to it when its
/// folder is writable (so the swap is two renames), else in `fallback`.
pub fn staging_path(target: &Path, fallback: &Path) -> PathBuf {
    match target.parent() {
        Some(dir) if writable(dir) => dir.join(STAGED),
        _ => fallback.join(APP),
    }
}

/// Whether swapping `target` can do without an administrator's password.
pub fn swap_needs_admin(target: &Path) -> bool {
    !target.parent().is_some_and(writable)
}

/// Removes what an earlier update left next to `target` (an update never swapped in, the app
/// one replaced).
pub fn clean_up(target: &Path) {
    if let Some(dir) = target.parent() {
        let _ = std::fs::remove_dir_all(dir.join(STAGED));
        let _ = std::fs::remove_dir_all(dir.join(OLD));
    }
}

/// Downloads the release's disk image to `dest`, checking its SHA-256.
pub fn download(r: &Release, dest: &Path) -> Result<(), String> {
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let out = Command::new("/usr/bin/curl")
        .args(["-sSfL", "--max-time", "900", "-A", concat!("Orbvane/", env!("CARGO_PKG_VERSION")), "-o"])
        .arg(dest)
        .arg(&r.url)
        .output()
        .map_err(|e| format!("Couldn't run curl: {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(dest);
        return Err(String::from_utf8_lossy(&out.stderr).trim().trim_start_matches("curl: ").to_string());
    }
    if let Some(expected) = &r.sha256 {
        if extensions::gallery::sha256_of(dest)? != *expected {
            let _ = std::fs::remove_file(dest);
            return Err("The download is damaged (its checksum doesn't match).".into());
        }
    }
    Ok(())
}

/// Checks that `app` meets `requirement` (and that its signature covers everything in it).
pub fn verify(app: &Path, requirement: &str) -> Result<(), String> {
    let out = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(format!("-R={requirement}"))
        .arg(app)
        .output()
        .map_err(|e| format!("Couldn't run codesign: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("The update isn't signed by Orbvane's developer ({}).", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// Mounts the disk image `dmg`, checks the app in it against `requirement` and copies it to
/// `staged` (replacing what's there).
pub fn prepare(dmg: &Path, requirement: &str, staged: &Path) -> Result<(), String> {
    let mount = std::env::temp_dir().join(format!("orbvane-update-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&mount);
    let attach = Command::new("/usr/bin/hdiutil")
        .args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-quiet", "-mountpoint"])
        .arg(&mount)
        .arg(dmg)
        .output()
        .map_err(|e| format!("Couldn't run hdiutil: {e}"))?;
    if !attach.status.success() {
        let _ = std::fs::remove_dir(&mount);
        return Err(format!("Couldn't open the update: {}", String::from_utf8_lossy(&attach.stderr).trim()));
    }
    let result = (|| {
        let app = mount.join(APP);
        if !app.is_dir() {
            return Err(format!("The update has no {APP}."));
        }
        verify(&app, requirement)?;
        let _ = std::fs::remove_dir_all(staged);
        if let Some(dir) = staged.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let copy = Command::new("/usr/bin/ditto").arg(&app).arg(staged).output().map_err(|e| format!("Couldn't run ditto: {e}"))?;
        if !copy.status.success() {
            return Err(format!("Couldn't copy the update: {}", String::from_utf8_lossy(&copy.stderr).trim()));
        }
        // The copy is what gets installed: check it too.
        verify(staged, requirement)
    })();
    let detached = Command::new("/usr/bin/hdiutil").args(["detach", "-quiet"]).arg(&mount).status().is_ok_and(|s| s.success());
    if !detached {
        let _ = Command::new("/usr/bin/hdiutil").args(["detach", "-quiet", "-force"]).arg(&mount).status();
    }
    let _ = std::fs::remove_dir(&mount);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(staged);
    }
    result
}

/// `s` quoted for the shell.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Puts the app at `staged` in place of `target`; the app it replaces is removed. When
/// `target`'s folder isn't writable, macOS asks for an administrator's password.
pub fn swap(staged: &Path, target: &Path) -> Result<(), String> {
    let dir = target.parent().ok_or("The app has no folder")?;
    let old = dir.join(OLD);
    if !swap_needs_admin(target) {
        let _ = std::fs::remove_dir_all(&old);
        std::fs::rename(target, &old).map_err(|e| format!("Couldn't move the current app aside: {e}"))?;
        if let Err(e) = std::fs::rename(staged, target) {
            let _ = std::fs::rename(&old, target);
            return Err(format!("Couldn't put the update in place: {e}"));
        }
        let _ = std::fs::remove_dir_all(&old);
        return Ok(());
    }
    // The update is copied in (from wherever it was staged) as root, then swapped.
    let new = dir.join(".Orbvane-new.app");
    let q = |p: &Path| sh_quote(&p.to_string_lossy());
    let script = format!(
        "/bin/rm -rf {old} {new} && /usr/bin/ditto {staged} {new} && /bin/mv {target} {old} && /bin/mv {new} {target} && /bin/rm -rf {old}",
        old = q(&old),
        new = q(&new),
        staged = q(staged),
        target = q(target)
    );
    let apple = format!("do shell script \"{}\" with administrator privileges", script.replace('\\', "\\\\").replace('"', "\\\""));
    let out = Command::new("/usr/bin/osascript").arg("-e").arg(apple).output().map_err(|e| format!("Couldn't run osascript: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(if err.contains("-128") { "The update was cancelled.".into() } else { format!("Couldn't install the update: {}", err.trim()) });
    }
    let _ = std::fs::remove_dir_all(staged);
    Ok(())
}

/// Opens `app` once this process has ended (after a swap, the new version).
pub fn relaunch(app: &Path) {
    let script = format!("while /bin/kill -0 {} 2>/dev/null; do /bin/sleep 0.2; done; /usr/bin/open {}", std::process::id(), sh_quote(&app.to_string_lossy()));
    let _ = Command::new("/bin/sh").args(["-c", &script]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

/// For tests: fake versions of the app, signed ad hoc (their requirement is `TEST_REQUIREMENT`),
/// and disk images holding them.
#[cfg(test)]
pub(crate) mod testing {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    pub const TEST_REQUIREMENT: &str = "identifier \"dev.orbvane.ide\"";

    /// `<dir>/Orbvane.app`, whose `Contents/Resources/version` file says `version`.
    pub fn fake_app(dir: &Path, version: &str) -> PathBuf {
        let app = dir.join("Orbvane.app");
        let _ = std::fs::remove_dir_all(&app);
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::create_dir_all(app.join("Contents/Resources")).unwrap();
        std::fs::copy("/usr/bin/true", app.join("Contents/MacOS/orbvane")).unwrap();
        std::fs::write(app.join("Contents/Resources/version"), version).unwrap();
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>orbvane</string>
<key>CFBundleIdentifier</key><string>dev.orbvane.ide</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
</dict></plist>
"#
        );
        std::fs::write(app.join("Contents/Info.plist"), plist).unwrap();
        let signed = Command::new("/usr/bin/codesign").args(["--force", "--sign", "-"]).arg(&app).output().unwrap();
        assert!(signed.status.success(), "{}", String::from_utf8_lossy(&signed.stderr));
        app
    }

    /// The version a fake app says it is.
    pub fn version_of(app: &Path) -> String {
        std::fs::read_to_string(app.join("Contents/Resources/version")).unwrap_or_default()
    }

    /// `<dir>/Orbvane-<version>.dmg` holding a fake app.
    pub fn fake_dmg(dir: &Path, version: &str) -> PathBuf {
        let src = dir.join(format!("dmg-{version}"));
        std::fs::create_dir_all(&src).unwrap();
        fake_app(&src, version);
        let dmg = dir.join(format!("Orbvane-{version}.dmg"));
        let made = Command::new("/usr/bin/hdiutil")
            .args(["create", "-quiet", "-ov", "-volname", "Orbvane", "-fs", "HFS+", "-format", "UDZO", "-srcfolder"])
            .arg(&src)
            .arg(&dmg)
            .output()
            .unwrap();
        assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
        dmg
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("orbvane-updater-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn stages_a_verified_update_and_swaps_it_in() {
        let dir = scratch("swap");
        let dmg = fake_dmg(&dir, "2.0.0");
        let target = fake_app(&dir.join("Applications"), "1.0.0");

        // Signed by someone else: refused, nothing staged.
        let staged = staging_path(&target, &dir.join("fallback"));
        assert_eq!(staged, dir.join("Applications").join(STAGED));
        let err = prepare(&dmg, &requirement("XZANL37WQK"), &staged).unwrap_err();
        assert!(err.contains("isn't signed by Orbvane's developer"), "{err}");
        assert!(!staged.exists());

        // Ours: staged next to the app, then swapped in; nothing is left behind.
        prepare(&dmg, TEST_REQUIREMENT, &staged).unwrap();
        assert_eq!(version_of(&staged), "2.0.0");
        assert!(!swap_needs_admin(&target));
        swap(&staged, &target).unwrap();
        assert_eq!(version_of(&target), "2.0.0");
        verify(&target, TEST_REQUIREMENT).unwrap();
        assert!(!staged.exists());
        assert!(!dir.join("Applications").join(OLD).exists());
        // The image was detached.
        assert!(!std::env::temp_dir().join(format!("orbvane-update-{}", std::process::id())).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn says_why_a_copy_cant_update_itself() {
        assert!(blocker(None, true).unwrap().contains("isn't an app bundle"));
        let translocated = Path::new("/private/var/folders/x/AppTranslocation/1234/d/Orbvane.app");
        assert!(blocker(Some(translocated), true).unwrap().contains("Applications folder"));
        assert!(blocker(Some(Path::new("/Volumes/Orbvane/Orbvane.app")), true).unwrap().contains("Applications folder"));
        assert!(blocker(Some(Path::new("/Applications/Orbvane.app")), false).unwrap().contains("isn't signed"));
        assert_eq!(blocker(Some(Path::new("/Applications/Orbvane.app")), true), None);
    }

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(!is_newer("0.1", "0.1.0"));
        assert!(!is_newer("0.2.0-beta", "0.2.0"));
    }

    #[test]
    fn parses_a_release() {
        let v = serde_json::json!({
            "tag_name": "v0.1.1",
            "html_url": "https://github.com/sbaruwal/orbvane/releases/tag/v0.1.1",
            "assets": [
                { "name": "notes.txt", "browser_download_url": "https://example.com/notes.txt" },
                { "name": "Orbvane-0.1.1.dmg", "browser_download_url": "https://example.com/Orbvane-0.1.1.dmg", "digest": "sha256:ABCD" }
            ]
        });
        let r = Release::parse(&v).unwrap();
        assert_eq!(r.version, "0.1.1");
        assert_eq!(r.url, "https://example.com/Orbvane-0.1.1.dmg");
        assert_eq!(r.sha256.as_deref(), Some("abcd"));
        assert!(r.page.ends_with("/v0.1.1"));
        let none = serde_json::json!({ "tag_name": "v0.2.0", "assets": [] });
        assert!(Release::parse(&none).unwrap_err().contains("Orbvane-0.2.0.dmg"));
    }

    #[test]
    fn quotes_for_the_shell() {
        assert_eq!(sh_quote("/Applications/Orbvane.app"), "'/Applications/Orbvane.app'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }
}
