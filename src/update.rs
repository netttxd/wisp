//! Updates from GitHub. A newer release there shows a pop-up; on yes, Wisp downloads it, swaps its files and
//! restarts on the new version.

use super::*;

/// Where Wisp's releases are published (see the GitHub releases memory/README).
const REPO: &str = "netttxd/wisp";

/// "v0.3.1" or "0.3.1" as numbers, so 0.3.10 counts as newer than 0.3.9.
fn version(s: &str) -> Vec<u32> {
    s.trim().trim_start_matches('v').split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// The newest release, if it is newer than this Wisp: (its tag, the Wisp.zip link, the zip's size in bytes).
fn newer() -> Option<(String, String, u64)> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let json: serde_json::Value = serde_json::from_str(&curl(&["-A", "Wisp-updater", "-H", "Accept: application/vnd.github+json", &url])).ok()?;
    let tag = json["tag_name"].as_str()?;
    if version(tag) <= version(env!("CARGO_PKG_VERSION")) {
        return None;
    }
    let zip = json["assets"].as_array()?.iter().find(|a| a["name"] == "Wisp.zip")?;
    Some((tag.to_string(), zip["browser_download_url"].as_str()?.to_string(), zip["size"].as_u64()?))
}

/// Downloads the zip and unpacks it. Returns the folder holding the new files.
fn download(url: &str, size: u64) -> Result<PathBuf, String> {
    let dir = env::temp_dir().join("wisp-update");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let zip = dir.join("Wisp.zip");
    let ok = Command::new("curl").args(["-s", "-L", "--max-time", "600", "-A", "Wisp-updater", "-o"]).arg(&zip).arg(url).creation_flags(CREATE_NO_WINDOW).status().is_ok_and(|s| s.success());
    // A cut-off download has the wrong size: never install it.
    if !ok || fs::metadata(&zip).map_or(0, |m| m.len()) != size {
        return Err("the download didn't finish".into());
    }
    // Windows' own tar unpacks zip files.
    let ok = Command::new("tar").arg("-xf").arg(&zip).arg("-C").arg(&dir).creation_flags(CREATE_NO_WINDOW).status().is_ok_and(|s| s.success());
    let files = dir.join("Wisp");
    if !ok || !files.join("wisp.exe").exists() {
        return Err("the download is damaged".into());
    }
    Ok(files)
}

/// Puts the new files in place of the old ones. Running files can't be overwritten but can be renamed, so each
/// old file becomes "<name>.old" (deleted at the next start). On any failure the old files come back.
fn install(new: &Path, target: &Path) -> Result<(), String> {
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    let result = (|| -> std::io::Result<()> {
        for e in fs::read_dir(new)? {
            let e = e?;
            let to = target.join(e.file_name());
            if to.exists() {
                let old = target.join(format!("{}.old", e.file_name().to_string_lossy()));
                let _ = fs::remove_file(&old);
                fs::rename(&to, &old)?;
                moved.push((to.clone(), old));
            }
            fs::copy(e.path(), &to)?;
        }
        Ok(())
    })();
    result.map_err(|e| {
        for (to, old) in &moved {
            let _ = fs::remove_file(to);
            let _ = fs::rename(old, to);
        }
        e.to_string()
    })
}

/// Deletes the files the last update left behind.
pub fn clean_up() {
    let dir = env::current_exe().unwrap_or_default().parent().map(Path::to_path_buf).unwrap_or_default();
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.path().extension().is_some_and(|x| x == "old") {
            let _ = fs::remove_file(e.path());
        }
    }
    let _ = fs::remove_dir_all(env::temp_dir().join("wisp-update"));
}

/// Checks GitHub shortly after start and then twice a day, on its own thread. When the user says yes to a new
/// version, its files land in `ready` for the main loop, which stops the wallpapers and calls `apply`.
pub fn watch(ready: Arc<std::sync::Mutex<Option<PathBuf>>>) {
    std::thread::spawn(move || {
        let mut declined = String::new();
        std::thread::sleep(Duration::from_secs(20));
        loop {
            if let Some((tag, url, size)) = newer().filter(|(tag, _, _)| *tag != declined) {
                let text = format!("Wisp {} is available (you have {}).\n\nUpdate now? Wisp restarts on the new version.", tag.trim_start_matches('v'), env!("CARGO_PKG_VERSION"));
                if message(&text, MB_YESNO | MB_ICONINFORMATION) == IDYES {
                    match download(&url, size) {
                        Ok(files) => *ready.lock().unwrap() = Some(files),
                        Err(e) => {
                            message(&format!("Couldn't update: {e}. You can download it from github.com/{REPO}."), MB_ICONWARNING);
                        }
                    }
                } else {
                    // Asked again at the next start, not every 12 hours of this one.
                    declined = tag;
                }
            }
            std::thread::sleep(Duration::from_secs(12 * 3600));
        }
    });
}

/// Installs downloaded files next to wisp.exe. The caller restarts Wisp after.
pub fn apply(files: &Path) -> Result<(), String> {
    let target = env::current_exe().map_err(|e| e.to_string())?.parent().ok_or("no folder")?.to_path_buf();
    install(files, &target).map_err(|e| format!("Couldn't update ({e}). If Wisp is in a protected folder like Program Files, move it to a folder of your own."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(version("v0.3.10") > version("0.3.9"));
        assert!(version("v0.4") > version("0.3.1"));
        assert!(version("v0.3.1") <= version("0.3.1"));
    }

    #[test]
    fn swaps_files_and_rolls_back() {
        let base = env::temp_dir().join(format!("wisp-install-test-{}", std::process::id()));
        let (new, target) = (base.join("new"), base.join("app"));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&new).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(new.join("wisp.exe"), "new").unwrap();
        fs::write(new.join("extra.dll"), "new dll").unwrap();
        fs::write(target.join("wisp.exe"), "old").unwrap();
        install(&new, &target).unwrap();
        assert_eq!(fs::read_to_string(target.join("wisp.exe")).unwrap(), "new");
        assert_eq!(fs::read_to_string(target.join("wisp.exe.old")).unwrap(), "old");
        assert!(target.join("extra.dll").exists());
        // A file that can't be copied (here: a folder in the way) puts every old file back.
        fs::write(new.join("wisp.exe"), "newer").unwrap();
        fs::create_dir_all(new.join("blocker")).unwrap();
        assert!(install(&new, &target).is_err());
        assert_eq!(fs::read_to_string(target.join("wisp.exe")).unwrap(), "new");
        let _ = fs::remove_dir_all(&base);
    }

    /// Downloads the real latest release and installs it into a temporary folder. Needs the internet: --ignored.
    #[test]
    #[ignore]
    fn downloads_real_release() {
        let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
        let json: serde_json::Value = serde_json::from_str(&curl(&["-A", "Wisp-updater", &url])).unwrap();
        let zip = json["assets"].as_array().unwrap().iter().find(|a| a["name"] == "Wisp.zip").unwrap();
        let files = download(zip["browser_download_url"].as_str().unwrap(), zip["size"].as_u64().unwrap()).unwrap();
        let target = env::temp_dir().join(format!("wisp-real-install-{}", std::process::id()));
        fs::create_dir_all(&target).unwrap();
        install(&files, &target).unwrap();
        assert!(target.join("wisp.exe").exists() && target.join("libmpv-2.dll").exists());
        let _ = fs::remove_dir_all(&target);
    }
}
