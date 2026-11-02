//! Updates are checked only when the person asks from Settings. A check may
//! download a newer app build for this machine, checks its sha256 and unpacks
//! it next to itself (`<install>.new`). The person chooses when to restart
//! into it or update herdr-core; neither is applied automatically.
//!
//! A build from source never updates: only `scripts/package` (the release
//! builds) sets `HERDR_APP_RELEASE`. `scripts/install-macos` makes a
//! Herdr.app too, so the install's shape alone can't tell.
//!
//! An explicit check also looks at `core/latest.json`: a newer core of the
//! same generation is offered unless a package manager owns it. Within a
//! generation a newer core only adds.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Where this app is installed: the folder (Linux) or bundle (macOS) an
/// update replaces. Read once: after a swap this process's own file is gone.
fn root() -> Option<PathBuf> {
    static ROOT: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    ROOT.get_or_init(find_root).clone()
}

fn find_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let dir = exe.parent()?;
    if cfg!(target_os = "macos") {
        // Herdr.app/Contents/MacOS/Herdr
        let bundle = dir.parent()?.parent()?;
        (bundle.extension()? == "app").then(|| bundle.to_path_buf())
    } else {
        // The release folder holds the binary and its icon; cargo's target/ doesn't.
        dir.join("herdr-app.svg").is_file().then(|| dir.to_path_buf())
    }
}

/// The folder or bundle name inside a release tarball.
const PACKED: &str = if cfg!(target_os = "macos") { "Herdr.app" } else { "herdr-app" };

fn beside(root: &Path, suffix: &str) -> PathBuf {
    let mut name = root.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    // macOS: Spotlight skips a folder ending in `.noindex`, so a staged
    // `Herdr.app.new` never shows up in search under that name.
    if cfg!(target_os = "macos") {
        name.push(".noindex");
    }
    root.with_file_name(name)
}

fn target() -> String {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    };
    format!("{os}-{}", std::env::consts::ARCH)
}

fn releases() -> String {
    std::env::var("HERDR_RELEASES_URL").unwrap_or_else(|_| "https://releases.herdr.dev".into())
}

/// The version downloaded and waiting for a restart.
static READY: Mutex<Option<String>> = Mutex::new(None);

pub fn ready() -> Option<String> {
    READY.lock().unwrap().clone()
}

/// The core the app runs, from its `server.status`, and a newer release waiting.
#[derive(Clone, Default, serde::Serialize)]
pub struct CoreInfo {
    pub version: Option<String>,
    /// A package manager that owns it (homebrew, nix, mise): never updated by the app.
    pub managed: Option<String>,
    pub ready: Option<String>,
    pub updating: bool,
    pub error: Option<String>,
}

static CORE: Mutex<CoreInfo> = Mutex::new(CoreInfo { version: None, managed: None, ready: None, updating: false, error: None });
/// When the last check succeeded (unix seconds); a failed check clears it.
static CHECKED: Mutex<Option<u64>> = Mutex::new(None);
/// A manually requested check is currently in progress.
static CHECKING: AtomicBool = AtomicBool::new(false);

pub fn core() -> CoreInfo {
    CORE.lock().unwrap().clone()
}

/// What the connected core says about itself (on connect and every reconnect).
pub fn set_core(version: Option<String>, managed: Option<String>) {
    let mut c = CORE.lock().unwrap();
    if let (Some(have), Some(ready)) = (&version, &c.ready)
        && !newer(ready, have)
    {
        c.ready = None;
    }
    c.version = version;
    c.managed = managed;
}

fn checks_app() -> Option<PathBuf> {
    root().filter(|_| option_env!("HERDR_APP_RELEASE").is_some() || std::env::var_os("HERDR_RELEASES_URL").is_some())
}

/// A release build checks the core; a source build only when pointed at a test release server.
fn checks_core() -> bool {
    option_env!("HERDR_APP_RELEASE").is_some() || std::env::var_os("HERDR_RELEASES_URL").is_some()
}

/// For the settings screen: both programs, what's waiting, when we last looked.
pub fn status() -> serde_json::Value {
    serde_json::json!({
        "app": { "version": env!("CARGO_PKG_VERSION"), "ready": ready(), "updates": checks_app().is_some() },
        "notes": crate::update_notes::notes(ready().as_deref()),
        "core": core(),
        "checked": *CHECKED.lock().unwrap(),
        "checks": checks_app().is_some() || checks_core(),
        "checking": CHECKING.load(Ordering::Relaxed),
    })
}

pub fn check_now() {
    let app = checks_app();
    let core = checks_core();
    if app.is_none() && !core {
        return;
    }
    if CHECKING.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(move || {
        let mut succeeded = true;
        if let Some(root) = &app {
            match check(root) {
                Ok(found) => {
                    if found.is_some() {
                        *READY.lock().unwrap() = found;
                    }
                }
                Err(e) => {
                    tracing::warn!("update check: {e}");
                    crate::telemetry::count("app_update_failed");
                    succeeded = false;
                }
            }
        }
        if core && let Err(e) = check_core() {
            tracing::warn!("core update check: {e}");
            succeeded = false;
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        *CHECKED.lock().unwrap() = checked_at(succeeded, now);
        CHECKING.store(false, Ordering::Release);
    });
}

fn checked_at(succeeded: bool, now: u64) -> Option<u64> {
    succeeded.then_some(now)
}

/// Looks for a newer core of this app's generation.
fn check_core() -> Result<(), String> {
    let (have, managed) = {
        let c = CORE.lock().unwrap();
        (c.version.clone(), c.managed.clone())
    };
    let Some(have) = have else { return Ok(()) };
    if managed.is_some() {
        return Ok(());
    }
    let latest = latest_core()?;
    let same = latest.generation == Some(crate::hc::GENERATION);
    CORE.lock().unwrap().ready = (same && newer(&latest.version, &have)).then_some(latest.version);
    Ok(())
}

#[derive(serde::Deserialize)]
struct CoreRelease {
    version: String,
    generation: Option<u64>,
}

fn latest_core() -> Result<CoreRelease, String> {
    let text = curl_text(&format!("{}/core/latest.json", releases()))?;
    serde_json::from_str(&text).map_err(|e| format!("core/latest.json: {e}"))
}

/// `hc update` (terminals keep running; the app reconnects when the core restarts).
pub fn update_core() {
    {
        let mut c = CORE.lock().unwrap();
        if c.updating {
            return;
        }
        c.updating = true;
        c.error = None;
    }
    std::thread::spawn(|| {
        let result = crate::core_bin::update();
        let mut c = CORE.lock().unwrap();
        c.updating = false;
        match result {
            Ok(now) => {
                c.version = Some(now.trim_start_matches("hc ").to_owned());
                c.ready = None;
                crate::telemetry::count("core_update_ok");
            }
            Err(e) => {
                c.error = Some(e);
                crate::telemetry::count("core_update_failed");
            }
        }
    });
}

/// Downloads the latest release if it's newer. Ok(Some): a version is ready.
fn check(root: &Path) -> Result<Option<String>, String> {
    check_from(root, &releases())
}

fn check_from(root: &Path, releases: &str) -> Result<Option<String>, String> {
    let path = "app/latest.json";
    let text = curl_text(&format!("{releases}/{path}"))?;
    let manifest: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
    let version = manifest["version"].as_str().ok_or(format!("{path}: no version"))?;
    if !newer(version, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    // Notes are optional: a missing feed must never hold up the update.
    if !crate::update_notes::has(version)
        && let Ok(text) = curl_text(&format!("{releases}/app/{version}/changelog.md"))
    {
        crate::update_notes::cache(&text);
    }
    let staged = beside(root, ".new");
    if staged_version(root).as_deref() == Some(version) {
        return Ok(Some(version.to_owned()));
    }
    let entry = &manifest["targets"][target()];
    let (Some(url), Some(sha)) = (entry["url"].as_str(), entry["sha256"].as_str()) else {
        return Ok(None);
    };
    tracing::info!("downloading Herdr {version}");
    let work = beside(root, ".download");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| format!("{}: {e}", work.display()))?;
    let result = unpack(url, sha, &work).and_then(|unpacked| {
        if version_of(&unpacked).as_deref() != Some(version) {
            return Err(format!("the download doesn't run as {version}"));
        }
        let _ = std::fs::remove_file(beside(root, ".new.json"));
        let _ = std::fs::remove_dir_all(&staged);
        std::fs::rename(&unpacked, &staged).map_err(|e| format!("{}: {e}", staged.display()))?;
        // Outside the bundle: writing a receipt must not change its signature.
        std::fs::write(beside(root, ".new.json"), serde_json::json!({ "version": version }).to_string()).map_err(|e| e.to_string())
    });
    let _ = std::fs::remove_dir_all(&work);
    result.map(|()| Some(version.to_owned()))
}

/// Downloads, checks and unpacks a release into `work`; returns the app inside.
fn unpack(url: &str, sha: &str, work: &Path) -> Result<PathBuf, String> {
    let tarball = work.join("app.tar.gz");
    run(crate::spawn::command(system("curl")).args(["-fsSL", "--max-time", "600", "-o"]).arg(&tarball).arg(url))?;
    let bytes = std::fs::read(&tarball).map_err(|e| e.to_string())?;
    if sha256(&bytes) != sha.to_ascii_lowercase() {
        return Err("the download is damaged (checksum mismatch)".into());
    }
    run(crate::spawn::command(system("tar")).arg("-xzf").arg(&tarball).arg("-C").arg(work))?;
    let app = work.join(PACKED);
    if cfg!(target_os = "macos") {
        // Authority is absent at -dv, even for a Developer ID release.
        let signature = Command::new("/usr/bin/codesign").arg("-dvv").arg(&app).output().map_err(|e| format!("codesign: {e}"))?;
        prepare_macos_bundle(&app, &signature)?;
    }
    Ok(app)
}

fn prepare_macos_bundle(app: &Path, signature: &std::process::Output) -> Result<(), String> {
    if release_signature(signature.status.success(), &String::from_utf8_lossy(&signature.stderr))? {
        run(Command::new("/usr/bin/codesign").args(["--verify", "--deep", "--strict"]).arg(app))?;
    } else {
        // Only known dev builds may change the plist and lose their signature.
        let home = std::env::var("HOME").unwrap_or_default();
        let path = format!("{home}/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin");
        let plist = app.join("Contents/Info.plist");
        run(Command::new("/usr/libexec/PlistBuddy").arg("-c").arg(format!("Add :LSEnvironment:PATH string {path}")).arg(&plist))?;
        run(Command::new("/usr/bin/codesign").args(["--force", "--sign", "-"]).arg(app))?;
    }
    Ok(())
}

fn release_signature(success: bool, signature: &str) -> Result<bool, String> {
    if success && signature.lines().any(|line| line.starts_with("Authority=Developer ID Application:")) {
        Ok(true)
    } else if (success && signature.lines().any(|line| line == "Signature=adhoc"))
        || (!success && signature.lines().any(|line| line.ends_with(": code object is not signed at all")))
    {
        Ok(false)
    } else {
        Err(format!("cannot determine app signature: {}", signature.trim()))
    }
}

/// curl and tar: on Windows its own (System32), since Git's tar, often first
/// on PATH, takes `C:\...` for a remote host.
pub fn system(name: &str) -> PathBuf {
    match std::env::var_os("SystemRoot").filter(|_| cfg!(windows)) {
        Some(root) => PathBuf::from(root).join("System32").join(format!("{name}.exe")),
        None => PathBuf::from(name),
    }
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn curl_text(url: &str) -> Result<String, String> {
    let out = crate::spawn::command(system("curl")).args(["-fsSL", "--max-time", "30", url]).stdin(Stdio::null()).output().map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(format!("{url}: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}

fn run(cmd: &mut Command) -> Result<(), String> {
    let out = cmd.stdin(Stdio::null()).output().map_err(|e| format!("{:?}: {e}", cmd.get_program()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("{:?}: {}", cmd.get_program(), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

fn exe_in(app: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        app.join("Contents/MacOS/Herdr")
    } else {
        app.join(format!("herdr-app{}", std::env::consts::EXE_SUFFIX))
    }
}

/// What `herdr-app --version` says in this folder or bundle (`0.2.0`).
fn version_of(app: &Path) -> Option<String> {
    let out = crate::spawn::command(exe_in(app)).arg("--version").stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        return None;
    }
    text.trim().strip_prefix("herdr-app ").map(str::to_owned)
}

/// `a` is a later version than `b` (semver; `0.2.0-rc.1` comes before `0.2.0`).
pub(crate) fn newer(a: &str, b: &str) -> bool {
    fn parts(v: &str) -> (Vec<u64>, Option<&str>) {
        let (core, pre) = v.trim_start_matches('v').split_once('-').map_or((v.trim_start_matches('v'), None), |(c, p)| (c, Some(p)));
        (core.split('.').map(|n| n.parse().unwrap_or(0)).collect(), pre)
    }
    let ((ca, pa), (cb, pb)) = (parts(a), parts(b));
    match ca.cmp(&cb) {
        std::cmp::Ordering::Equal => match (pa, pb) {
            (None, Some(_)) => true,
            (Some(x), Some(y)) => x > y,
            _ => false,
        },
        o => o == std::cmp::Ordering::Greater,
    }
}

/// A receipt is published only after the archive and its executable were verified.
/// Legacy stages without one are left for the normal checker to download again.
fn staged_version(root: &Path) -> Option<String> {
    let staged = beside(root, ".new");
    let receipt = beside(root, ".new.json");
    let version = std::fs::read(&receipt).ok().and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value["version"].as_str().map(str::to_owned));
    let version = version?;
    if version_of(&staged).as_deref() != Some(&version) {
        tracing::warn!("update: staged executable does not match its verified manifest; discarding it");
        let _ = std::fs::remove_file(receipt);
        let _ = std::fs::remove_dir_all(staged);
        return None;
    }
    Some(version)
}

/// Puts the downloaded version in place of this one. The running app keeps
/// its files open, so it can go on until it quits. True if it swapped.
/// Windows lets a running program's folder be renamed, not deleted; the old
/// one is left beside it for the person to remove.
pub fn apply() -> bool {
    let Some(root) = root() else { return false };
    let staged = staged_version(&root);
    if ready().as_deref() != staged.as_deref() || !staged.as_deref().is_some_and(|version| newer(version, env!("CARGO_PKG_VERSION"))) {
        return false;
    }
    swap(&root)
}

fn swap(root: &Path) -> bool {
    let staged = beside(root, ".new");
    let old = beside(root, ".old");
    let _ = std::fs::remove_dir_all(&old);
    if let Err(e) = std::fs::rename(root, &old) {
        tracing::warn!("update: cannot move {} aside: {e}", root.display());
        return false;
    }
    if let Err(e) = std::fs::rename(&staged, root) {
        tracing::warn!("update: cannot put the new version in place: {e}");
        if let Err(rollback) = std::fs::rename(&old, root) {
            tracing::error!("update: cannot restore the previous version: {rollback}");
        }
        return false;
    }
    let _ = std::fs::remove_file(beside(root, ".new.json"));
    *READY.lock().unwrap() = None;
    true
}

/// Opens the app again once this process has exited (after `apply`).
pub fn relaunch() {
    relaunch_after_quit();
}

#[cfg(windows)]
fn relaunch_after_quit() -> bool {
    let Some(root) = root() else { return false };
    let exe = exe_in(&root).to_string_lossy().replace('\'', "''");
    let script = format!("Wait-Process -Id {} -ErrorAction SilentlyContinue; Start-Process '{exe}'", std::process::id());
    let spawned = crate::spawn::command("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match spawned {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("update: cannot reopen the app: {e}");
            false
        }
    }
}

/// Opens the app again once this process has exited (after `apply`).
#[cfg(unix)]
fn relaunch_after_quit() -> bool {
    let Some(root) = root() else { return false };
    let open = if cfg!(target_os = "macos") {
        "open \"$1\""
    } else {
        "exec \"$1/herdr-app\""
    };
    let script = format!("while kill -0 \"$0\" 2>/dev/null; do sleep 0.1; done; {open}");
    let spawned = {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(script)
            .arg(std::process::id().to_string())
            .arg(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
    };
    match spawned {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("update: cannot reopen the app: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_check_clears_success_and_recovery_records_a_new_time() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::time::Duration;

        // An isolated release server drops the first connection, then recovers.
        // No environment changes, app window, core, or installed files are involved.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for recover in [false, true] {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                assert!(request.starts_with(b"GET /app/latest.json HTTP/1.1\r\n"));
                if recover {
                    let body = format!(r#"{{"version":"{}"}}"#, env!("CARGO_PKG_VERSION"));
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                }
            }
        });
        let home = std::env::temp_dir().join(format!("herdr-offline-check-{}", std::process::id()));
        let mut checked = checked_at(true, 100);
        assert_eq!(checked, Some(100));
        let failed = check_from(&home, &url);
        assert!(failed.is_err(), "a dropped connection must fail: {failed:?}");
        checked = checked_at(failed.is_ok(), 200);
        assert_eq!(checked, None);
        let recovered = check_from(&home, &url);
        assert_eq!(recovered, Ok(None));
        checked = checked_at(recovered.is_ok(), 300);
        assert_eq!(checked, Some(300));
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn stages_need_a_receipt_and_a_matching_executable() {
        use std::os::unix::fs::PermissionsExt;
        let work = std::env::temp_dir().join(format!("herdr-update-{}", std::process::id()));
        let root = work.join(PACKED);
        let staged = beside(&root, ".new");
        let receipt = beside(&root, ".new.json");
        let exe = exe_in(&staged);
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "#!/bin/sh\necho herdr-app 0.1.8\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(staged_version(&root), None);
        assert!(staged.is_dir()); // Legacy stages are left for the checker.
        std::fs::write(&receipt, r#"{"version":"0.1.8"}"#).unwrap();
        assert_eq!(staged_version(&root).as_deref(), Some("0.1.8"));
        std::fs::write(&receipt, r#"{"version":"0.1.9"}"#).unwrap();
        assert_eq!(staged_version(&root), None);
        assert!(!staged.exists());
        assert!(!receipt.exists());
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "corrupt").unwrap();
        std::fs::write(&receipt, r#"{"version":"0.1.8"}"#).unwrap();
        assert_eq!(staged_version(&root), None);
        assert!(!staged.exists());
        std::fs::remove_dir_all(work).unwrap();
    }

    #[test]
    fn signature_inspection_fails_closed() {
        let release = include_str!("../scripts/fixtures/codesign/developer-id.txt");
        let adhoc = include_str!("../scripts/fixtures/codesign/adhoc.txt");
        let unsigned = include_str!("../scripts/fixtures/codesign/unsigned.txt");
        assert_eq!(release_signature(true, release), Ok(true));
        assert_eq!(release_signature(true, adhoc), Ok(false));
        assert_eq!(release_signature(false, unsigned), Ok(false));
        for (success, output) in [(false, release), (false, adhoc), (true, unsigned), (true, ""), (false, "codesign: internal error"), (true, "Authority=Apple Development: Someone")] {
            assert!(release_signature(success, output).unwrap_err().starts_with("cannot determine app signature:"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn unknown_signature_does_not_modify_the_bundle() {
        use std::os::unix::process::ExitStatusExt;
        let work = std::env::temp_dir().join(format!("herdr-unknown-signature-{}", std::process::id()));
        let plist = work.join("Contents/Info.plist");
        std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
        std::fs::write(&plist, "unchanged plist").unwrap();
        let signature = std::process::Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: b"codesign: internal error".to_vec(),
        };
        assert_eq!(prepare_macos_bundle(&work, &signature), Err("cannot determine app signature: codesign: internal error".into()));
        assert_eq!(std::fs::read_to_string(plist).unwrap(), "unchanged plist");
        assert_eq!(std::fs::read_dir(work.join("Contents")).unwrap().count(), 1);
        std::fs::remove_dir_all(work).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "downloads the current release; runs real codesign on isolated bundles"]
    fn macos_unpack_preserves_release_signature_and_prepares_dev_builds() {
        let version = env!("CARGO_PKG_VERSION");
        let manifest: serde_json::Value = serde_json::from_str(&curl_text(&format!("{}/app/{version}/manifest.json", releases())).unwrap()).unwrap();
        let work = std::env::temp_dir().join(format!("herdr-signature-test-{}", std::process::id()));
        let signed = work.join("signed");
        let reference = work.join("reference");
        std::fs::create_dir_all(&signed).unwrap();
        std::fs::create_dir_all(&reference).unwrap();
        let entry = &manifest["targets"][target()];
        let app = unpack(entry["url"].as_str().unwrap(), entry["sha256"].as_str().unwrap(), &signed).unwrap();
        let signature = Command::new("/usr/bin/codesign").arg("-dvv").arg(&app).output().unwrap();
        assert_eq!(release_signature(signature.status.success(), &String::from_utf8_lossy(&signature.stderr)), Ok(true));
        run(Command::new("/usr/bin/codesign").args(["--verify", "--deep", "--strict"]).arg(&app)).unwrap();
        run(Command::new("tar").arg("-xzf").arg(signed.join("app.tar.gz")).arg("-C").arg(&reference)).unwrap();
        for file in ["Contents/Info.plist", "Contents/MacOS/Herdr", "Contents/_CodeSignature/CodeResources"] {
            assert_eq!(std::fs::read(app.join(file)).unwrap(), std::fs::read(reference.join(PACKED).join(file)).unwrap(), "{file} was modified");
        }
        for kind in ["adhoc", "unsigned"] {
            let source = reference.join(PACKED);
            let mut codesign = Command::new("/usr/bin/codesign");
            if kind == "adhoc" {
                codesign.args(["--force", "--sign", "-"]);
            } else {
                codesign.arg("--remove-signature");
            }
            run(codesign.arg(&source)).unwrap();
            let archive = work.join(format!("{kind}.tar.gz"));
            run(Command::new("tar").arg("-czf").arg(&archive).arg("-C").arg(&reference).arg(PACKED)).unwrap();
            let dev = work.join(kind);
            std::fs::create_dir_all(&dev).unwrap();
            let sha = sha256(&std::fs::read(&archive).unwrap());
            let app = unpack(&format!("file://{}", archive.display()), &sha, &dev).unwrap();
            let signature = Command::new("/usr/bin/codesign").arg("-dvv").arg(&app).output().unwrap();
            assert!(signature.status.success());
            let signature = String::from_utf8_lossy(&signature.stderr);
            assert_eq!(release_signature(true, &signature), Ok(false));
            assert!(signature.lines().any(|line| line == "Signature=adhoc"));
            run(Command::new("/usr/bin/codesign").args(["--verify", "--deep", "--strict"]).arg(&app)).unwrap();
            let path = Command::new("/usr/libexec/PlistBuddy").args(["-c", "Print :LSEnvironment:PATH"]).arg(app.join("Contents/Info.plist")).output().unwrap();
            assert!(path.status.success());
            assert!(String::from_utf8_lossy(&path.stdout).contains("/opt/homebrew/bin"));
        }
        std::fs::remove_dir_all(work).unwrap();
    }

    #[test]
    fn versions_compare_like_semver() {
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("0.10.0", "0.9.0"));
        assert!(newer("1.0.0", "1.0.0-rc.1"));
        assert!(!newer("1.0.0-rc.1", "1.0.0"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn sha256_is_hex() {
        assert_eq!(sha256(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
