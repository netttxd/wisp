#![windows_subsystem = "windows"]

mod mpv;
mod scene;
mod update;
mod window;

use std::{
    collections::{hash_map::DefaultHasher, BTreeMap},
    env, fs,
    hash::{Hash, Hasher},
    mem::{size_of, zeroed},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, OnceLock,
    },
    time::{Duration, Instant, SystemTime},
};
use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::*, Gdi::*},
    System::{
        Threading::{CreateMutexW, GetCurrentProcess, SetPriorityClass, SetProcessWorkingSetSize, BELOW_NORMAL_PRIORITY_CLASS},
    },
    UI::{Controls::Dialogs::*, WindowsAndMessaging::*},
};

const APP: &str = "Wisp";
/// How mpv draws the video: cheapest, sharper scaling with debanding, or Anime4K upscaling on top.
const QUALITIES: [&str; 3] = ["fast", "sharp", "anime"];
const QUALITY_LABELS: [&str; 3] = ["Fast", "Sharp", "Anime"];

/// Anime4K's "Mode A (Fast)" preset for mpv, in its documented order. MIT licensed, see assets/anime4k/LICENSE.
const ANIME4K: [(&str, &str); 6] = [
    ("Anime4K_Clamp_Highlights.glsl", include_str!("../assets/anime4k/Anime4K_Clamp_Highlights.glsl")),
    ("Anime4K_Restore_CNN_M.glsl", include_str!("../assets/anime4k/Anime4K_Restore_CNN_M.glsl")),
    ("Anime4K_Upscale_CNN_x2_M.glsl", include_str!("../assets/anime4k/Anime4K_Upscale_CNN_x2_M.glsl")),
    ("Anime4K_AutoDownscalePre_x2.glsl", include_str!("../assets/anime4k/Anime4K_AutoDownscalePre_x2.glsl")),
    ("Anime4K_AutoDownscalePre_x4.glsl", include_str!("../assets/anime4k/Anime4K_AutoDownscalePre_x4.glsl")),
    ("Anime4K_Upscale_CNN_x2_S.glsl", include_str!("../assets/anime4k/Anime4K_Upscale_CNN_x2_S.glsl")),
];

/// Writes the Anime4K shaders next to the cache (mpv reads them from files) and returns mpv's list of them.
fn anime4k_shaders() -> String {
    let dir = cache_dir().with_file_name("shaders");
    let _ = fs::create_dir_all(&dir);
    let paths: Vec<String> = ANIME4K
        .iter()
        .map(|(name, text)| {
            let p = dir.join(name);
            if fs::metadata(&p).map(|m| m.len()).ok() != Some(text.len() as u64) {
                let _ = fs::write(&p, text);
            }
            p.display().to_string()
        })
        .collect();
    paths.join(";") // mpv's list separator on Windows
}
const FITS: [&str; 3] = ["fill", "fit", "stretch"];
const FIT_LABELS: [&str; 3] = ["Fill", "Fit", "Stretch"];
const VIDEO_EXT: [&str; 7] = ["mp4", "webm", "gif", "mkv", "mov", "avi", "webp"];
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CACHE_LIMIT: u64 = 500 * 1024 * 1024;
const WALLPAPER_ENGINE: &str = "431960";

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

struct Config {
    /// One wallpaper per monitor, primary first. Empty means none.
    files: Vec<String>,
    /// One fit mode per monitor.
    fits: Vec<String>,
    /// Per monitor, for Fill: which part of the picture shows, -1 to 1 each way (see `place`).
    pans: Vec<(f32, f32)>,
    /// Per monitor, for Fill: how far it's zoomed in, 1 to MAX_ZOOM.
    zooms: Vec<f32>,
    /// Fit for monitors with no saved fit yet.
    fit: String,
    /// Frame rate cap, 0 for none.
    fps: u32,
    /// "fast", "sharp" or "anime": how wallpapers are drawn.
    quality: String,
    paused: bool,
    /// One wallpaper (Screen 1's) across all screens instead of one per screen.
    span: bool,
    /// Wallpaper sound (videos and scenes' music), off unless turned on.
    audio: bool,
    /// 0 to 100.
    volume: u32,
    /// Last used wallpapers, newest first.
    recent: Vec<String>,
    /// Per monitor: a Workshop item id waiting for Steam to download it.
    waits: Vec<String>,
    /// Scene settings the user changed, per wallpaper: file, then setting name to its value as JSON text.
    settings: BTreeMap<String, BTreeMap<String, String>>,
    /// Stop wallpapers while a game or other full-screen app is open (on unless turned off).
    game_pause: bool,
    /// Stop wallpapers while a laptop runs on battery (on unless turned off).
    battery_pause: bool,
    /// The window already explained, once, that closing it keeps Wisp running in the tray.
    told_tray: bool,
    /// The window already said, once, whether Wallpaper Engine is installed.
    told_we: bool,
}

/// The library: every wallpaper used, newest first. Home shows the first few as "recent".
const RECENT_MAX: usize = 60;

/// "" is monitor 0 (the old single-monitor keys), "3" is monitor 3.
fn monitor_index(suffix: &str) -> Option<usize> {
    let i = if suffix.is_empty() { 0 } else { suffix.parse().ok()? };
    (i < 16).then_some(i)
}

fn set_at(v: &mut Vec<String>, i: usize, value: &str) {
    if v.len() <= i {
        v.resize(i + 1, String::new());
    }
    v[i] = value.into();
}

impl Config {
    fn path() -> PathBuf {
        PathBuf::from(env::var("APPDATA").unwrap_or_default()).join(APP).join("config.txt")
    }

    fn load() -> Config {
        // Older names of this app, newest first.
        let appdata = PathBuf::from(env::var("APPDATA").unwrap_or_default());
        let text = Config::read_disk()
            .or_else(|| fs::read_to_string(appdata.join(r"Lumen\config.txt")).ok())
            .or_else(|| fs::read_to_string(appdata.join(r"livewall\config.txt")).ok())
            .unwrap_or_default();
        let mut c = Config::parse(&text);
        // Configs from before the recent list: start it with the wallpapers in use.
        if c.recent.is_empty() {
            c.recent = c.files.iter().filter(|f| !f.is_empty()).cloned().collect();
        }
        c
    }

    fn parse(text: &str) -> Config {
        let mut c = Config {
            files: Vec::new(),
            fits: Vec::new(),
            pans: Vec::new(),
            zooms: Vec::new(),
            fit: "fill".into(),
            fps: 0,
            quality: "sharp".into(),
            paused: false,
            span: false,
            audio: false,
            volume: 50,
            recent: Vec::new(),
            waits: Vec::new(),
            settings: BTreeMap::new(),
            game_pause: true,
            battery_pause: true,
            told_tray: false,
            told_we: false,
        };
        for line in text.lines() {
            match line.split_once('=') {
                // Old configs had one "fit" for every screen.
                Some(("fit", v)) if FITS.contains(&v) => c.fit = v.into(),
                Some((k, v)) if k.starts_with("recent") && !v.is_empty() && c.recent.len() < RECENT_MAX => c.recent.push(v.into()),
                Some((k, v)) if k.starts_with("wait") => {
                    if let Some(i) = monitor_index(&k[4..]) {
                        set_at(&mut c.waits, i, v);
                    }
                }
                Some((k, v)) if k.starts_with("file") => {
                    if let Some(i) = monitor_index(&k[4..]) {
                        set_at(&mut c.files, i, v);
                    }
                }
                Some((k, v)) if k.starts_with("fit") && FITS.contains(&v) => {
                    if let Some(i) = monitor_index(&k[3..]) {
                        set_at(&mut c.fits, i, v);
                    }
                }
                // "place0=0.5,-1,1.5": which part shows (x, y) and the zoom. (The older "pan" keys meant something
                // else and are dropped.)
                Some((k, v)) if k.starts_with("place") => {
                    let n: Vec<f32> = v.split(',').filter_map(|n| n.trim().parse().ok()).collect();
                    if let (Some(i), &[x, y, z]) = (monitor_index(&k[5..]), &n[..]) {
                        if c.pans.len() <= i {
                            c.pans.resize(i + 1, (0.0, 0.0));
                            c.zooms.resize(i + 1, 1.0);
                        }
                        c.pans[i] = (x.clamp(-1.0, 1.0), y.clamp(-1.0, 1.0));
                        c.zooms[i] = z.clamp(1.0, MAX_ZOOM);
                    }
                }
                Some(("fps", v)) => c.fps = v.parse::<u32>().unwrap_or(0).min(240),
                Some(("quality", v)) if QUALITIES.contains(&v) => c.quality = v.into(),
                Some(("paused", v)) => c.paused = v == "true",
                Some(("audio", v)) => c.audio = v == "true",
                Some(("span", v)) => c.span = v == "true",
                Some(("game_pause", v)) => c.game_pause = v != "false",
                Some(("battery_pause", v)) => c.battery_pause = v != "false",
                Some(("told_tray", v)) => c.told_tray = v == "true",
                Some(("told_we", v)) => c.told_we = v == "true",
                Some(("volume", v)) => c.volume = v.parse::<u32>().unwrap_or(50).min(100),
                // "setting=<file>|<name>|<json>": paths can't contain "|", and the JSON comes last.
                Some(("setting", v)) => {
                    let mut parts = v.splitn(3, '|');
                    if let (Some(file), Some(name), Some(value)) = (parts.next(), parts.next(), parts.next()) {
                        c.settings.entry(file.into()).or_default().insert(name.into(), value.into());
                    }
                }
                _ => {}
            }
        }
        for f in c.fits.iter_mut().filter(|f| f.is_empty()) {
            *f = c.fit.clone();
        }
        c
    }

    /// Makes sure there is a file, fit and wait slot for each of n monitors.
    fn fit_to(&mut self, n: usize) {
        let n = n.max(self.files.len());
        self.files.resize(n, String::new());
        self.fits.resize(n, self.fit.clone());
        self.pans.resize(n, (0.0, 0.0));
        self.zooms.resize(n, 1.0);
        self.waits.resize(n, String::new());
    }

    /// Sets a monitor's wallpaper and puts it at the top of the recent list.
    fn set_file(&mut self, i: usize, file: String) {
        self.recent.retain(|r| *r != file);
        self.recent.insert(0, file.clone());
        self.recent.truncate(RECENT_MAX);
        self.files[i] = file;
        self.waits[i].clear();
    }

    /// Writes the config and returns the text written, so the writer can tell its own change from others'.
    fn save(&self) -> String {
        let p = Config::path();
        let _ = fs::create_dir_all(p.parent().unwrap());
        let out = self.save_text();
        // Write a temp file, then swap it in: the other process never reads a half-written config.
        let tmp = p.with_extension("tmp");
        if fs::write(&tmp, &out).is_ok() {
            let _ = fs::rename(&tmp, &p);
        }
        out
    }

    fn save_text(&self) -> String {
        let mut out = String::new();
        for (i, file) in self.files.iter().enumerate() {
            let fit = self.fits.get(i).unwrap_or(&self.fit);
            let wait = self.waits.get(i).map_or("", |s| s.as_str());
            out += &format!("file{i}={file}\nfit{i}={fit}\nwait{i}={wait}\n");
            let ((x, y), z) = (self.pans.get(i).copied().unwrap_or_default(), self.zooms.get(i).copied().unwrap_or(1.0));
            if (x, y, z) != (0.0, 0.0, 1.0) {
                out += &format!("place{i}={x},{y},{z}\n");
            }
        }
        for (k, r) in self.recent.iter().enumerate() {
            out += &format!("recent{k}={r}\n");
        }
        out += &format!("fps={}\nquality={}\npaused={}\naudio={}\nvolume={}\nspan={}\n", self.fps, self.quality, self.paused, self.audio, self.volume, self.span);
        out += &format!("game_pause={}\nbattery_pause={}\ntold_tray={}\ntold_we={}\n", self.game_pause, self.battery_pause, self.told_tray, self.told_we);
        // Settings of wallpapers no longer in the library are dropped.
        for (file, values) in self.settings.iter().filter(|(f, _)| self.recent.contains(f) || self.files.contains(f)) {
            for (name, value) in values {
                out += &format!("setting={file}|{name}|{value}\n");
            }
        }
        out
    }

    /// A wallpaper's changed scene settings as a JSON object, for the scene player's command line.
    fn settings_json(&self, file: &str) -> String {
        let values: serde_json::Map<String, serde_json::Value> = self.settings.get(file).into_iter().flatten().filter_map(|(k, v)| Some((k.clone(), serde_json::from_str(v).ok()?))).collect();
        serde_json::Value::Object(values).to_string()
    }

    /// The config text on disk, or None when it can't be read or is empty (so it is never mistaken for "no settings").
    fn read_disk() -> Option<String> {
        fs::read_to_string(Config::path()).ok().filter(|t| !t.trim().is_empty())
    }
}

/// A monitor layout in a form that can be compared, to notice resolution and monitor changes.
fn layout(mons: &[(RECT, RECT)]) -> Vec<[i32; 8]> {
    mons.iter().map(|(m, w)| [m.left, m.top, m.right, m.bottom, w.left, w.top, w.right, w.bottom]).collect()
}

/// Every monitor as (whole screen, screen minus taskbar), primary first.
fn monitors() -> Vec<(RECT, RECT)> {
    unsafe extern "system" fn each(mon: HMONITOR, _: HDC, _: *mut RECT, out: LPARAM) -> BOOL {
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(mon, &mut mi) != 0 {
            (*(out as *mut Vec<(RECT, RECT)>)).push((mi.rcMonitor, mi.rcWork));
        }
        1
    }
    let mut all: Vec<(RECT, RECT)> = Vec::new();
    unsafe { EnumDisplayMonitors(null_mut(), null(), Some(each), &mut all as *mut Vec<(RECT, RECT)> as LPARAM) };
    // The primary monitor is the one at 0,0.
    all.sort_by_key(|(r, _)| ((r.left, r.top) != (0, 0), r.left, r.top));
    all
}

/// True for windows that can hide the desktop: not see-through overlays (GPU, Discord, game bars, assistants),
/// not hidden, minimized or suspended app windows, and not the desktop or taskbar themselves.
unsafe fn solid_window(hwnd: HWND) -> bool {
    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    let mut cloaked = 0u32;
    DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED as u32, &mut cloaked as *mut u32 as _, 4);
    if IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 || cloaked != 0 || ex & (WS_EX_TRANSPARENT | WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) != 0 {
        return false;
    }
    let mut class = [0u16; 32];
    let n = GetClassNameW(hwnd, class.as_mut_ptr(), 32) as usize;
    !["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd"].contains(&String::from_utf16_lossy(&class[..n]).as_str())
}

/// True when some window hides the whole desktop of this monitor (maximized or fullscreen).
fn covered(work: &RECT) -> bool {
    unsafe extern "system" fn each(hwnd: HWND, data: LPARAM) -> BOOL {
        let (work, hit) = &mut *(data as *mut (RECT, bool));
        if !solid_window(hwnd) {
            return 1;
        }
        let mut r: RECT = zeroed();
        if GetWindowRect(hwnd, &mut r) != 0 && covers(&r, work) {
            *hit = true;
            return 0;
        }
        1
    }
    let mut data = (*work, false);
    unsafe { EnumWindows(Some(each), &mut data as *mut (RECT, bool) as LPARAM) };
    data.1
}

/// The window that sits behind the desktop icons.
unsafe fn wallpaper_host() -> HWND {
    let progman = FindWindowW(w("Progman").as_ptr(), null());
    // Undocumented message: makes Explorer create the WorkerW behind the icons.
    SendMessageTimeoutW(progman, 0x052C, 0xD, 0x1, SMTO_NORMAL, 1000, null_mut());
    // Windows 11 24H2+: the WorkerW is a child of Progman.
    let child = FindWindowExW(progman, null_mut(), w("WorkerW").as_ptr(), null());
    if !child.is_null() {
        return child;
    }
    // Older Windows: it is the top-level WorkerW after the window that owns the icons.
    unsafe extern "system" fn each(top: HWND, out: LPARAM) -> BOOL {
        if FindWindowExW(top, null_mut(), w("SHELLDLL_DefView").as_ptr(), null()).is_null() {
            return 1;
        }
        *(out as *mut HWND) = FindWindowExW(null_mut(), top, w("WorkerW").as_ptr(), null());
        0
    }
    let mut found: HWND = null_mut();
    EnumWindows(Some(each), &mut found as *mut HWND as LPARAM);
    found
}

fn covers(win: &RECT, mon: &RECT) -> bool {
    win.left <= mon.left && win.top <= mon.top && win.right >= mon.right && win.bottom >= mon.bottom
}

/// True when the focused window fills its whole monitor (a game, a fullscreen video).
// shortcut: only the focused window is checked, so clicking an app on another screen mid-game restarts the wallpapers. Track the game window if that matters.
fn fullscreen_app() -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        // A see-through overlay in focus (like a screen recorder or assistant) is not a game.
        if fg.is_null() || !solid_window(fg) {
            return false;
        }
        let mut win: RECT = zeroed();
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetWindowRect(fg, &mut win) != 0
            && GetMonitorInfoW(MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST), &mut mi) != 0
            && covers(&win, &mi.rcMonitor)
    }
}

fn cache_dir() -> PathBuf {
    PathBuf::from(env::var("LOCALAPPDATA").unwrap_or_default()).join(APP).join("cache")
}

/// What a screen's lighter copy is made at: the screen's size, and a frame rate cap (0 keeps the video's).
#[derive(Clone, Copy, PartialEq, Hash, Debug)]
struct Target {
    w: i32,
    h: i32,
    fps: u32,
}

fn target(cfg: &Config, rect: &RECT) -> Target {
    let (w, h) = size(rect);
    Target { w, h, fps: cfg.fps }
}

/// Where the lighter copy of `file` for a target lives. It may not exist yet.
fn cache_path(file: &str, t: Target) -> PathBuf {
    let meta = fs::metadata(file).ok();
    let mut hasher = DefaultHasher::new();
    // Size and date are in the key, so an edited file gets a fresh copy. The version changes when copies are made
    // differently (2: quality 100 and Lanczos), so old copies are remade.
    (file, t, meta.as_ref().map(|m| m.len()), meta.and_then(|m| m.modified().ok()), 2u8).hash(&mut hasher);
    cache_dir().join(format!("{:016x}.mp4", hasher.finish()))
}

use mpv::video_info;

fn video_size(file: &str) -> Option<(i32, i32)> {
    video_info(file).map(|(w, h, _)| (w, h))
}

/// Whether a vw x vh video at vfps gets smaller (size, frame rate) when made for this target.
fn worth_shrinking(vw: i32, vh: i32, vfps: f64, t: Target) -> (bool, bool) {
    // Never enlarge: a bigger copy would cost more RAM, not less.
    let size = (t.w as f64 / vw as f64).max(t.h as f64 / vh as f64) < 0.9;
    let fps = t.fps > 0 && vfps > t.fps as f64 + 1.0;
    (size, fps)
}

/// Makes the lighter copy of a video: smaller and/or at a lower frame rate. Runs on its own thread.
fn shrink(file: String, t: Target, out: PathBuf) -> bool {
    let Some((vw, vh, vfps)) = video_info(&file) else { return false };
    let (smaller, slower) = worth_shrinking(vw, vh, vfps, t);
    if !smaller && !slower {
        return false;
    }
    let _ = fs::create_dir_all(cache_dir());
    let part = out.with_extension("part");
    let mut filters = Vec::new();
    if smaller {
        // Lanczos: sharper than ffmpeg's default bicubic when shrinking 4K to the screen.
        filters.push(format!("scale=w={}:h={}:force_original_aspect_ratio=increase:flags=lanczos+accurate_rnd", t.w, t.h));
    }
    if slower {
        // Fewer frames in the file means fewer to decode, unlike capping at playback.
        filters.push(format!("fps={}", t.fps));
    }
    filters.push("scale=trunc(iw/2)*2:trunc(ih/2)*2,format=nv12".into());
    let vf = format!("lavfi=[{}]", filters.join(","));
    let part_name = part.display().to_string();
    // Windows' own H.264 encoder: the GPU one first, the software one when there is no GPU encoder.
    // Quality 100: at 85 dark areas lost their grain and textures smeared next to the original. It makes
    // bigger files (about 80 MB a minute at 1080p, against 44), which costs disk space, not RAM or decoding.
    for opts in ["rate_control=quality,quality=100,hw_encoding=1", "rate_control=quality,quality=100"] {
        let ok = mpv::run(&[("audio", "no"), ("vf", &vf), ("of", "mp4"), ("ovc", "h264_mf"), ("ovcopts", opts), ("o", &part_name)], &file);
        if ok && fs::metadata(&part).is_ok_and(|m| m.len() > 0) && fs::rename(&part, &out).is_ok() {
            return true;
        }
    }
    let _ = fs::remove_file(&part);
    false
}

/// Deletes the least recently used copies until the cache fits its limit.
fn trim_cache(in_use: &[PathBuf]) {
    let mut files: Vec<(SystemTime, u64, PathBuf)> = fs::read_dir(cache_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((m.modified().ok()?, m.len(), e.path()))
        })
        .collect();
    files.sort();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    for (_, len, path) in files {
        if total <= CACHE_LIMIT {
            break;
        }
        if !in_use.contains(&path) && fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

fn size(r: &RECT) -> (i32, i32) {
    (r.right - r.left, r.bottom - r.top)
}

/// A child of the wallpaper host that covers one monitor. mpv draws into it.
unsafe fn surface(monitor: &RECT) -> HWND {
    let host = wallpaper_host();
    if host.is_null() {
        return null_mut();
    }
    // The host spans every monitor, so screen coordinates must be converted to its own.
    let mut origin = POINT { x: monitor.left, y: monitor.top };
    ScreenToClient(host, &mut origin);
    let (width, height) = (monitor.right - monitor.left, monitor.bottom - monitor.top);
    CreateWindowExW(0, w("Static").as_ptr(), null(), WS_CHILD | WS_VISIBLE, origin.x, origin.y, width, height, host, null_mut(), null_mut(), null())
}

/// What plays a screen's wallpaper, inside this process.
enum Engine {
    Video(mpv::Mpv),
    /// Wisp's scene engine on its own thread; setting the flag stops it. The Live is how it sits on the screen.
    Scene(Arc<AtomicBool>, std::thread::JoinHandle<()>, Arc<scene::Live>),
}

struct Player {
    engine: Engine,
    surface: HWND,
    /// When to next hand unused memory back to Windows (see trim).
    trim_at: Instant,
    /// This screen's sound: on only for the screen that plays sound (see the main loop). A scene's thread reads it.
    audio: Arc<mpv::Audio>,
    /// The sound a video player last applied.
    applied: (bool, u32),
    /// For a lighter copy (it has no sound): the original, whose sound is added the first time sound turns on.
    sound_from: Option<String>,
}

impl Player {
    fn new(engine: Engine, surface: HWND, audio: Arc<mpv::Audio>, sound_from: Option<String>) -> Player {
        Player { engine, surface, trim_at: Instant::now() + Duration::from_secs(5), audio, applied: (false, 0), sound_from }
    }

    /// Sets this screen's sound while playing, no restart: a video switches its audio track on or off,
    /// a scene's thread starts or stops its music.
    fn sound(&mut self, on: bool, volume: u32) {
        self.audio.on.store(on, Ordering::Relaxed);
        self.audio.volume.store(volume, Ordering::Relaxed);
        let Engine::Video(m) = &self.engine else { return };
        if (on, volume) == self.applied {
            return;
        }
        m.set("volume", &volume.to_string());
        if !on {
            m.set("aid", "no");
        } else if let Some(original) = self.sound_from.take() {
            // One path, added as a track: no list splitting, so names with ";" work too.
            if !m.command(&["audio-add", &original, "select"]) {
                // The video isn't open yet: try again on the next round.
                self.sound_from = Some(original);
                return;
            }
        } else {
            m.set("aid", "auto");
        }
        self.applied = (on, volume);
    }

    /// False once the player stopped by itself: an unplayable file, or a scene that failed or crashed.
    fn alive(&mut self) -> bool {
        // Explorer restarted and took the desktop window with it.
        if unsafe { IsWindow(self.surface) } == 0 {
            return false;
        }
        match &self.engine {
            // Videos loop forever, so any end means mpv gave up on the file.
            Engine::Video(m) => m.ended().is_none(),
            Engine::Scene(_, thread, _) => !thread.is_finished(),
        }
    }

    /// mpv keeps what it used for starting up (about 100 MB) and only touches about 65 MB per loop of a video.
    /// Trimming the working set moves the rest out of RAM; Windows brings pages back if they are used again.
    /// Again every minute, since each loop of the video leaves a little more behind.
    /// Changes the fit mode, the part shown and the zoom while it plays: a video at once, a scene on its next frame.
    fn place(&self, fit: &str, pan: (f32, f32), zoom: f32) -> bool {
        match &self.engine {
            Engine::Video(m) => {
                let (panscan, keepaspect) = match fit {
                    "fill" => ("1.0", "yes"),
                    "stretch" => ("0.0", "no"),
                    _ => ("0.0", "yes"),
                };
                m.set("panscan", panscan) && m.set("keepaspect", keepaspect) && mpv_place(fit, pan, zoom).iter().all(|(k, v)| m.set(k, v))
            }
            Engine::Scene(_, _, live) => live.place.lock().map(|mut p| *p = (fit.to_string(), pan, zoom)).is_ok(),
        }
    }

    /// A video player opens another video in place, so there's no gap between the two. False for scenes.
    fn swap(&mut self, play: &str, original: &str, fit: &str, pan: (f32, f32), zoom: f32) -> bool {
        let Engine::Video(m) = &self.engine else { return false };
        if !m.command(&["loadfile", play]) {
            return false;
        }
        // A lighter copy has no sound: Player::sound adds the original's on its next round.
        self.sound_from = (play != original).then(|| original.to_string());
        self.applied = (false, u32::MAX);
        self.place(fit, pan, zoom)
    }

    /// Its first frame is on screen, so the wallpaper it replaces can go.
    fn ready(&self) -> bool {
        match &self.engine {
            Engine::Video(m) => m.get("estimated-frame-number").and_then(|n| n.parse::<i64>().ok()).is_some_and(|n| n >= 1),
            Engine::Scene(_, _, live) => live.ready.load(Ordering::Relaxed),
        }
    }

    fn trim(&mut self) {
        if Instant::now() >= self.trim_at {
            self.trim_at = Instant::now() + Duration::from_secs(60);
            unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX) };
        }
    }
}

/// How long one wallpaper fades into the next, in seconds.
const FADE: f32 = 0.3;

/// False when the person turned off "Animation effects" in Windows: changes then happen without fades.
fn animations_on() -> bool {
    let mut on: BOOL = 1;
    unsafe { SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, &mut on as *mut BOOL as *mut std::ffi::c_void, 0) };
    on != 0
}

/// The most a Fill wallpaper can be zoomed in.
const MAX_ZOOM: f32 = 3.0;

/// Where a picture shaped `pic` (width / height) sits on a screen shaped `scr`: its left, top, width and height,
/// with the screen 0 to 1 each way. Fill covers the screen and can be zoomed and moved. Moving only uses the part
/// that's cut off anyway (1 lines up the picture's left or top edge with the screen's, -1 its right or bottom), so
/// no edge ever shows black and moving never zooms. Fit and Stretch always show the whole picture.
fn place(pic: f32, scr: f32, fit: &str, (x, y): (f32, f32), zoom: f32) -> [f32; 4] {
    let (w, h) = match fit {
        "stretch" => (1.0, 1.0),
        "fit" if pic > scr => (1.0, scr / pic),
        "fit" => (pic / scr, 1.0),
        _ => {
            let z = zoom.clamp(1.0, MAX_ZOOM);
            if pic > scr { (pic / scr * z, z) } else { (z, scr / pic * z) }
        }
    };
    let (x, y) = if fit == "fill" { (x.clamp(-1.0, 1.0), y.clamp(-1.0, 1.0)) } else { (0.0, 0.0) };
    [(1.0 - w) / 2.0 + x * (w - 1.0).max(0.0) / 2.0, (1.0 - h) / 2.0 + y * (h - 1.0).max(0.0) / 2.0, w, h]
}

/// mpv's options for `place`. video-align lines the cropped video up with an edge the same way (-1 left/top,
/// 1 right/bottom, so it's the opposite sign), and needs no knowledge of the video's shape.
fn mpv_place(fit: &str, (x, y): (f32, f32), zoom: f32) -> [(&'static str, String); 3] {
    let (x, y, z) = if fit == "fill" { (-x.clamp(-1.0, 1.0), -y.clamp(-1.0, 1.0), zoom.clamp(1.0, MAX_ZOOM)) } else { (0.0, 0.0, 1.0) };
    [("video-align-x", x.to_string()), ("video-align-y", y.to_string()), ("video-zoom", z.log2().to_string())]
}

/// The wallpaper areas: each monitor as (screen, [its usable area]), or with `span` and several monitors one area
/// around them all, which Screen 1's wallpaper fills.
fn areas(mons: &[(RECT, RECT)], span: bool) -> Vec<(RECT, Vec<RECT>)> {
    if !span || mons.len() < 2 {
        return mons.iter().map(|&(rect, work)| (rect, vec![work])).collect();
    }
    let all = mons.iter().fold(mons[0].0, |a, (r, _)| RECT { left: a.left.min(r.left), top: a.top.min(r.top), right: a.right.max(r.right), bottom: a.bottom.max(r.bottom) });
    vec![(all, mons.iter().map(|&(_, work)| work).collect())]
}

/// One wallpaper area (a monitor, or all of them when spanning) and its running player, if any.
struct Slot {
    rect: RECT,
    /// The usable area (screen minus taskbar) of each monitor it covers.
    works: Vec<RECT>,
    /// The file a shrink was last started for, so each file is tried once.
    tried: String,
    shrinking: Option<std::thread::JoinHandle<bool>>,
    player: Option<Player>,
    /// The wallpaper being replaced: it stays on screen until the new one shows its first frame, so a change never
    /// flashes black. And since when.
    old: Option<Player>,
    old_since: Instant,
    /// When the old wallpaper started fading out, revealing the new one behind it.
    fade_from: Option<Instant>,
    started: Instant,
    fails: u32,
}

impl Slot {
    /// Starts replacing what's on screen: it stays until the new player is up (see the main loop).
    fn replace(&mut self) {
        // Mid-fade: the old one is nearly gone, so finish that and replace what's on screen now.
        if self.fade_from.take().is_some() {
            stop(&mut self.old);
        }
        if self.old.is_some() {
            // Already waiting on a new one: drop that, keep what's showing.
            stop(&mut self.player);
        } else {
            self.old = self.player.take();
            self.old_since = Instant::now();
        }
    }

    fn stop_all(&mut self) {
        stop(&mut self.player);
        stop(&mut self.old);
        self.fade_from = None;
    }

    /// Fades the replaced wallpaper out over FADE seconds once the new one is up, then removes it.
    fn fade_old(&mut self) {
        let Some(old) = &self.old else { return };
        let ready = self.player.as_ref().is_none_or(Player::ready) || self.old_since.elapsed().as_secs() >= 10;
        if self.fade_from.is_none() {
            if !ready {
                return;
            }
            // Nothing new to reveal, or the person turned animations off in Windows: just remove it.
            if self.player.is_none() || !animations_on() {
                stop(&mut self.old);
                return;
            }
            unsafe {
                SetWindowLongPtrW(old.surface, GWL_EXSTYLE, GetWindowLongPtrW(old.surface, GWL_EXSTYLE) | WS_EX_LAYERED as isize);
                SetLayeredWindowAttributes(old.surface, 0, 255, LWA_ALPHA);
            }
            self.fade_from = Some(Instant::now());
            return;
        }
        let k = self.fade_from.unwrap().elapsed().as_secs_f32() / FADE;
        if k >= 1.0 {
            stop(&mut self.old);
            self.fade_from = None;
        } else {
            // Eased: starts and ends gently.
            let shown = 1.0 - k * k * (3.0 - 2.0 * k);
            unsafe { SetLayeredWindowAttributes(old.surface, 0, (shown * 255.0) as u8, LWA_ALPHA) };
        }
    }
}

/// Wallpaper Engine scenes are stored as the path of their scene.pkg.
fn is_scene(path: &str) -> bool {
    path.to_lowercase().ends_with("scene.pkg")
}

/// `settings`: a scene's changed settings as JSON (see Config::settings_json); unused for videos.
/// `original`: the video `file` is a lighter copy of (copies have no sound, so it comes from the original).
/// `pan`, `zoom`: which part of a Fill wallpaper shows (see `place`). Players start silent; Player::sound turns sound on.
#[allow(clippy::too_many_arguments)]
fn spawn(quality: &str, fps: u32, file: &str, original: &str, fit: &str, monitor: &RECT, settings: &str, pan: (f32, f32), zoom: f32) -> Option<Player> {
    let surface = unsafe { surface(monitor) };
    if surface.is_null() {
        return None;
    }
    if is_scene(file) {
        // Wisp's own scene engine, on a thread of its own so it starts and stops like a video.
        let folder = Path::new(file).parent().unwrap_or(Path::new(file)).display().to_string();
        let fps = if fps == 0 { 60 } else { fps };
        let (fit, settings, parent) = (fit.to_string(), settings.to_string(), surface as isize);
        let stop = Arc::new(AtomicBool::new(false));
        let audio = Arc::new(mpv::Audio { on: AtomicBool::new(false), volume: AtomicU32::new(0) });
        let (flag, sound) = (stop.clone(), audio.clone());
        let live = Arc::new(scene::Live::new(&fit, pan, zoom));
        let shared = live.clone();
        let thread = std::thread::spawn(move || scene::run_wallpaper(&folder, parent, fps, &settings, &flag, Some(&sound), &shared));
        return Some(Player::new(Engine::Scene(stop, thread, live), surface, audio, None));
    }
    let wid = (surface as isize).to_string();
    let shaders = anime4k_shaders();
    let placed = mpv_place(fit, pan, zoom);
    let mut options = vec![
        ("wid", wid.as_str()),
        ("loop-file", "inf"),
        // Which part of a Fill video shows, and its zoom.
        (placed[0].0, placed[0].1.as_str()),
        (placed[1].0, placed[1].1.as_str()),
        (placed[2].0, placed[2].1.as_str()),
        // Silent until Player::sound; no sound device means silent playback, not a failure.
        ("aid", "no"),
        ("audio-fallback-to-null", "yes"),
        ("hwdec", "auto-safe"),
        ("osc", "no"),
        ("osd-bar", "no"),
        ("input-default-bindings", "no"),
        ("input-vo-keyboard", "no"),
        ("input-cursor", "no"),
        ("cursor-autohide", "no"),
        ("stop-screensaver", "no"),
        ("terminal", "no"),
        // Everything below only cuts RAM and CPU work: no scripts, small buffers.
        ("ytdl", "no"),
        ("media-controls", "no"),
        ("load-scripts", "no"),
        ("load-auto-profiles", "no"),
        ("load-commands", "no"),
        ("load-console", "no"),
        ("load-positioning", "no"),
        ("load-select", "no"),
        ("load-stats-overlay", "no"),
        ("cache", "no"),
        ("demuxer-max-bytes", "16MiB"),
        ("demuxer-max-back-bytes", "0"),
        ("vd-lavc-threads", "2"),
    ];
    match fit {
        "fill" => options.push(("panscan", "1.0")),
        "stretch" => options.push(("keepaspect", "no")),
        _ => {}
    }
    if quality == "fast" {
        // Cheapest scaling, no debanding: least GPU, softest picture.
        options.push(("profile", "fast"));
    } else {
        // Sharp scaling and debanding (removes color stripes in gradients).
        options.extend([("scale", "spline36"), ("cscale", "spline36"), ("dscale", "mitchell"), ("deband", "yes")]);
        if quality == "anime" {
            options.push(("glsl-shaders", shaders.as_str()));
        }
    }
    let audio = Arc::new(mpv::Audio { on: AtomicBool::new(false), volume: AtomicU32::new(0) });
    // A lighter copy has no sound track: the original's sound is added when sound turns on.
    let sound_from = (file != original).then(|| original.to_string());
    match mpv::Mpv::new(&options).filter(|m| m.command(&["loadfile", file])) {
        Some(m) => Some(Player::new(Engine::Video(m), surface, audio, sound_from)),
        None => {
            unsafe { DestroyWindow(surface) };
            None
        }
    }
}

/// Answers messages other threads send to this thread's windows, for up to `d`. Players' windows are children of
/// the screen windows made on this thread, so making or closing them waits for this thread to answer.
fn pump_for(d: Duration) {
    let until = Instant::now() + d;
    loop {
        unsafe {
            let mut msg: MSG = zeroed();
            // Only sent messages; posted ones stay for the main loop.
            PeekMessageW(&mut msg, null_mut(), 0, 0, PM_NOREMOVE | PM_QS_SENDMESSAGE);
        }
        if Instant::now() >= until {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Waits for a player's thread to end while answering its window messages (see pump_for); a plain join would
/// deadlock when the player closes its window.
fn wait_pumping(t: std::thread::JoinHandle<()>) {
    while !t.is_finished() {
        pump_for(Duration::from_millis(5));
    }
    let _ = t.join();
}

fn stop(player: &mut Option<Player>) {
    if let Some(p) = player.take() {
        match p.engine {
            // Dropping the player stops mpv and frees it; it closes its window on its own thread.
            Engine::Video(m) => wait_pumping(std::thread::spawn(move || drop(m))),
            Engine::Scene(stop, thread, _) => {
                stop.store(true, Ordering::Relaxed);
                wait_pumping(thread);
            }
        }
        // Removing the surface makes the normal wallpaper show again.
        unsafe { DestroyWindow(p.surface) };
    }
}

fn pick_file() -> Option<String> {
    let mut buf = [0u16; 1024];
    let filter = w("Video, GIF or scene\0*.mp4;*.gif;*.webm;*.mkv;*.mov;*.avi;*.webp;scene.pkg\0All files\0*.*\0");
    let mut o: OPENFILENAMEW = unsafe { zeroed() };
    o.lStructSize = size_of::<OPENFILENAMEW>() as u32;
    o.lpstrFilter = filter.as_ptr();
    o.lpstrFile = buf.as_mut_ptr();
    o.nMaxFile = buf.len() as u32;
    o.Flags = OFN_FILEMUSTEXIST | OFN_EXPLORER | OFN_NOCHANGEDIR;
    if unsafe { GetOpenFileNameW(&mut o) } == 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
    Some(String::from_utf16_lossy(&buf[..len]))
}

fn message(text: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    unsafe { MessageBoxW(null_mut(), w(text).as_ptr(), w(APP).as_ptr(), style | MB_SETFOREGROUND) }
}

/// The item id from a Workshop link (…/filedetails/?id=123) or a bare id.
fn workshop_id(text: &str) -> Option<String> {
    let t = text.trim();
    let id = match t.split(['?', '&']).find_map(|part| part.strip_prefix("id=")) {
        Some(rest) => rest.chars().take_while(|c| c.is_ascii_digit()).collect(),
        None => t.to_string(),
    };
    (!id.is_empty() && id.len() <= 20 && id.chars().all(|c| c.is_ascii_digit())).then_some(id)
}

/// Every Steam library folder on this PC.
fn steam_libraries() -> Vec<PathBuf> {
    let out = Command::new("reg")
        .args(["query", r"HKCU\Software\Valve\Steam", "/v", "SteamPath"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    // The registry spells it "c:/program files (x86)/steam"; libraryfolders.vdf lists the same folder with backslashes.
    let Some(steam) = out.lines().find_map(|l| l.split_once("REG_SZ")).map(|(_, v)| PathBuf::from(v.trim().replace('/', r"\"))) else {
        return Vec::new();
    };
    let vdf = fs::read_to_string(steam.join(r"steamapps\libraryfolders.vdf")).unwrap_or_default();
    let mut libs: Vec<PathBuf> = vdf
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("\"path\""))
        .filter_map(|l| l.trim_end_matches('"').rsplit('"').next())
        .map(|p| PathBuf::from(p.replace(r"\\", r"\")))
        .collect();
    libs.push(steam);
    // Same folder, different case: keep the first spelling only, so nothing is found twice.
    let mut seen = Vec::new();
    libs.retain(|l| {
        let key = l.to_string_lossy().to_lowercase();
        let new = !seen.contains(&key);
        seen.push(key);
        new
    });
    libs
}

/// The string value of the first `"key": "value"` in a JSON text.
// shortcut: not a real JSON parser, fine for Wallpaper Engine's flat project.json.
fn json_str(json: &str, key: &str) -> Option<String> {
    let start = json.find(&format!("\"{key}\""))? + key.len() + 2;
    let rest = json[start..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    Some(rest[..rest.find('"')?].replace(r"\/", "/"))
}

fn is_video(p: &Path) -> bool {
    p.extension().is_some_and(|e| VIDEO_EXT.contains(&e.to_string_lossy().to_lowercase().as_str()))
}

/// The video inside a downloaded Workshop item, if it has one.
fn workshop_video(item: &Path, app: &str) -> Option<PathBuf> {
    if app == WALLPAPER_ENGINE {
        // Wallpaper Engine names the main file in project.json: a video, scene.json (a Scene) or index.html (Web).
        let path = item.join(json_str(&fs::read_to_string(item.join("project.json")).ok()?, "file")?);
        let pkg = item.join("scene.pkg");
        if path.file_name().is_some_and(|n| n == "scene.json") && pkg.exists() {
            return Some(pkg);
        }
        return (is_video(&path) && path.exists()).then_some(path);
    }
    // Other wallpaper apps: the biggest video in the item that isn't a preview.
    // shortcut: only the item's top folder is searched, look in subfolders if some app nests its files.
    fs::read_dir(item)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_video(p) && !p.file_stem().is_some_and(|s| s.to_string_lossy().to_lowercase().starts_with("preview")))
        .max_by_key(|p| fs::metadata(p).map(|m| m.len()).unwrap_or(0))
}

/// Every Wallpaper Engine wallpaper already on this PC that Wisp can play (Workshop downloads and your own
/// projects), newest first. Web wallpapers are left out: Wisp can't play them.
fn installed_wallpapers() -> Vec<String> {
    let mut items: Vec<(SystemTime, PathBuf)> = Vec::new();
    for lib in steam_libraries() {
        for root in [lib.join(r"steamapps\workshop\content").join(WALLPAPER_ENGINE), lib.join(r"steamapps\common\wallpaper_engine\projects\myprojects")] {
            for e in fs::read_dir(&root).into_iter().flatten().flatten() {
                items.push((e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH), e.path()));
            }
        }
    }
    items.sort_by(|a, b| b.0.cmp(&a.0));
    items.into_iter().filter_map(|(_, item)| workshop_video(&item, WALLPAPER_ENGINE)).map(|p| p.display().to_string()).collect()
}

/// Finds a downloaded Workshop item in any app's Workshop folder: (folder, app id).
fn find_item(id: &str) -> Option<(PathBuf, String)> {
    static LIBS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    LIBS.get_or_init(steam_libraries).iter().find_map(|lib| {
        fs::read_dir(lib.join(r"steamapps\workshop\content")).ok()?.flatten().find_map(|app| {
            let item = app.path().join(id);
            item.exists().then(|| (item, app.file_name().to_string_lossy().into_owned()))
        })
    })
}

fn curl(args: &[&str]) -> String {
    Command::new("curl")
        .args(["-s", "--max-time", "10"])
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Which Steam app a Workshop item belongs to, asked from Steam's public API.
fn workshop_app(id: &str) -> Option<String> {
    let form = format!("publishedfileids[0]={id}");
    let json = curl(&["-d", "itemcount=1", "-d", &form, "https://api.steampowered.com/ISteamRemoteStorage/GetPublishedFileDetails/v1/"]);
    let rest = &json[json.find("\"consumer_app_id\":")? + 18..];
    let app: String = rest.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
    (!app.is_empty()).then_some(app)
}

fn app_name(app: &str) -> String {
    if app == WALLPAPER_ENGINE {
        return "Wallpaper Engine".into();
    }
    let json = curl(&[&format!("https://store.steampowered.com/api/appdetails?appids={app}&filters=basic")]);
    json_str(&json, "name").unwrap_or_else(|| format!("Steam app {app}"))
}

/// Opens a web or steam:// link with its default app. Unlike `cmd /c start`, a `&` in the link is safe.
fn open_url(url: &str) {
    let _ = Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).creation_flags(CREATE_NO_WINDOW).spawn();
}

/// The playable video of a downloaded item, or None after telling the user why not.
fn item_video(item: &Path, app: &str) -> Option<String> {
    let video = workshop_video(item, app).map(|v| v.display().to_string());
    if video.is_none() {
        message("Wisp can't play this Workshop item. Video and Scene wallpapers work; Web and Application wallpapers only run inside Wallpaper Engine.", MB_ICONWARNING);
    }
    video
}

enum Workshop {
    Ready(String),
    /// Not downloaded yet: play it as soon as Steam has it.
    Waiting(String),
    Nothing,
}

/// Turns a pasted Workshop link into a video, or starts waiting for Steam to download it.
/// Wallpaper Engine's copy of Steam's library (steam_api64.dll), if it's installed.
fn steam_api() -> Option<PathBuf> {
    steam_libraries().into_iter().map(|lib| lib.join(r"steamapps\common\wallpaper_engine\bin\steam_api64.dll")).find(|p| p.exists())
}

/// What `run_subscribe` asks Steam to do with a Workshop item.
#[derive(PartialEq, Clone, Copy)]
enum SteamJob {
    Subscribe,
    Download,
    Unsubscribe,
}

/// A Workshop item's (app, item number, folder) when `path` is inside one: `...\workshop\content\<app>\<item>\...`.
fn workshop_item(path: &str) -> Option<(String, String, PathBuf)> {
    let parts: Vec<_> = Path::new(path).components().collect();
    let at = parts.windows(2).position(|w| w[0].as_os_str().eq_ignore_ascii_case("workshop") && w[1].as_os_str().eq_ignore_ascii_case("content"))?;
    let (app, item) = (parts.get(at + 2)?.as_os_str().to_str()?, parts.get(at + 3)?.as_os_str().to_str()?);
    if !app.chars().all(|c| c.is_ascii_digit()) || !item.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((app.into(), item.into(), parts[..at + 4].iter().collect()))
}

/// `wisp.exe --remove <path>`: removes a Workshop wallpaper from the PC. It unsubscribes first (or Steam would
/// download it again), then deletes the item's folder. Only ever a Workshop item's own folder: see workshop_item.
fn remove_workshop_item(path: &str) {
    let Some((app, item, folder)) = workshop_item(path) else { return };
    run_subscribe(&app, &item, SteamJob::Unsubscribe);
    // Steam sometimes deletes it itself right after unsubscribing; either way it's gone.
    let _ = fs::remove_dir_all(&folder);
}

/// `wisp.exe --subscribe <app> <item>`: subscribes to a Workshop item through Steam's own library, like the app
/// itself would, so no Steam page opens. Runs as its own short process: while Steam's library is active, Steam counts
/// the app as running. Exit code 0 when Steam accepted the subscription; it then starts `--steam-download`.
///
/// `wisp.exe --steam-download <app> <item>` (`SteamJob::Download`): keeps Steam's library active until the item is
/// downloaded (up to 20 minutes), since Steam may hold back a Workshop download while its app isn't running.
///
/// `SteamJob::Unsubscribe` (used by `--remove`): unsubscribes, so Steam doesn't download the item again.
fn run_subscribe(app: &str, item: &str, job: SteamJob) -> i32 {
    use std::ffi::c_void;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    let (Some(dll), Ok(id)) = (steam_api(), item.parse::<u64>()) else { return 2 };
    // Steam's library reads which app it is from here.
    env::set_var("SteamAppId", app);
    env::set_var("SteamGameId", app);
    unsafe {
        let lib = LoadLibraryW(w(&dll.to_string_lossy()).as_ptr());
        if lib.is_null() {
            return 2;
        }
        macro_rules! f {
            ($name:literal, $ty:ty) => {
                match GetProcAddress(lib, concat!($name, "\0").as_ptr()) {
                    Some(p) => std::mem::transmute::<_, $ty>(p),
                    None => return 2,
                }
            };
        }
        // Newer Steam libraries start through InitFlat: 0 means OK, otherwise it writes why into the 1 KB buffer.
        let init = f!("SteamAPI_InitFlat", unsafe extern "C" fn(*mut [u8; 1024]) -> i32);
        let shutdown = f!("SteamAPI_Shutdown", unsafe extern "C" fn());
        let run_callbacks = f!("SteamAPI_RunCallbacks", unsafe extern "C" fn());
        let ugc = f!("SteamAPI_SteamUGC_v020", unsafe extern "C" fn() -> *mut c_void);
        let utils = f!("SteamAPI_SteamUtils_v010", unsafe extern "C" fn() -> *mut c_void);
        let subscribe = f!("SteamAPI_ISteamUGC_SubscribeItem", unsafe extern "C" fn(*mut c_void, u64) -> u64);
        let unsubscribe = f!("SteamAPI_ISteamUGC_UnsubscribeItem", unsafe extern "C" fn(*mut c_void, u64) -> u64);
        let download = f!("SteamAPI_ISteamUGC_DownloadItem", unsafe extern "C" fn(*mut c_void, u64, bool) -> bool);
        let done = f!("SteamAPI_ISteamUtils_IsAPICallCompleted", unsafe extern "C" fn(*mut c_void, u64, *mut bool) -> bool);
        let state = f!("SteamAPI_ISteamUGC_GetItemState", unsafe extern "C" fn(*mut c_void, u64) -> u32);
        // Fails when Steam isn't running or this account doesn't own the app.
        let mut why = [0u8; 1024];
        if init(&mut why) != 0 {
            return 3;
        }
        let (ugc, utils) = (ugc(), utils());
        if job == SteamJob::Unsubscribe {
            let call = unsubscribe(ugc, id);
            let mut failed = false;
            for _ in 0..300 {
                run_callbacks();
                if done(utils, call, &mut failed) {
                    shutdown();
                    return if failed { 4 } else { 0 };
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            shutdown();
            return 4;
        }
        if job == SteamJob::Download {
            download(ugc, id, true);
            // Item states: 4 installed; 8 needs an update, 16 downloading, 32 waiting to download.
            for _ in 0..20 * 60 * 4 {
                run_callbacks();
                let s = state(ugc, id);
                if s & 4 != 0 && s & (8 | 16 | 32) == 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            shutdown();
            return 0;
        }
        let call = subscribe(ugc, id);
        download(ugc, id, true);
        let mut failed = false;
        let mut accepted = false;
        for _ in 0..300 {
            run_callbacks();
            if done(utils, call, &mut failed) {
                accepted = !failed;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        shutdown();
        if !accepted {
            return 4;
        }
        let _ = Command::new(env::current_exe().unwrap_or_default()).args(["--steam-download", app, item]).creation_flags(CREATE_NO_WINDOW).spawn();
        0
    }
}

/// Subscribes through the helper above, waiting for it. False when it couldn't (then Steam's page is the way).
fn subscribe_quietly(app: &str, id: &str) -> bool {
    let Ok(mut child) = Command::new(env::current_exe().unwrap_or_default()).args(["--subscribe", app, id]).creation_flags(CREATE_NO_WINDOW).spawn() else { return false };
    for _ in 0..450 {
        if let Ok(Some(status)) = child.try_wait() {
            return status.success();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    false
}

fn from_workshop(link: &str) -> Workshop {
    let Some(id) = workshop_id(link) else {
        message("That isn't a Steam Workshop link.\n\nExample: steamcommunity.com/sharedfiles/filedetails/?id=123456789", MB_ICONINFORMATION);
        return Workshop::Nothing;
    };
    if let Some((item, app)) = find_item(&id) {
        return item_video(&item, &app).map_or(Workshop::Nothing, Workshop::Ready);
    }
    let app = workshop_app(&id).unwrap_or_else(|| WALLPAPER_ENGINE.into());
    let installed = steam_libraries().iter().any(|lib| lib.join(format!(r"steamapps\appmanifest_{app}.acf")).exists());
    let name = app_name(&app);
    if !installed {
        // Steam keeps a subscription but downloads nothing for an app that isn't installed.
        let ask = format!(
            "This wallpaper is from the Workshop of \"{name}\", which isn't installed. Steam only downloads Workshop items for installed apps.\n\nInstall \"{name}\" now? You never have to open it: Wisp plays the wallpaper.\n\nAfter installing, click Subscribe on the wallpaper's page. Wisp starts it by itself once Steam has downloaded it."
        );
        if message(&ask, MB_YESNO | MB_ICONQUESTION) != IDYES {
            return Workshop::Nothing;
        }
        open_url(&format!("steam://install/{app}"));
    } else if subscribe_quietly(&app, &id) {
        // Subscribed without opening Steam: it downloads now, and Wisp starts the wallpaper once it's there.
        return Workshop::Waiting(id);
    }
    open_url(&format!("steam://url/CommunityFilePage/{id}"));
    if installed {
        message(
            &format!("Click Subscribe on the page that just opened in Steam.\n\nWisp starts the wallpaper by itself once Steam has downloaded it. If nothing happens, start \"{name}\" once so Steam downloads it."),
            MB_ICONINFORMATION,
        );
    }
    Workshop::Waiting(id)
}

fn reg(args: &[&str]) -> bool {
    Command::new("reg").args(args).creation_flags(CREATE_NO_WINDOW).status().is_ok_and(|s| s.success())
}

fn set_autostart(on: bool) {
    let exe = env::current_exe().unwrap_or_default();
    if on {
        reg(&["add", RUN_KEY, "/v", APP, "/d", &format!("\"{}\"", exe.display()), "/f"]);
    } else {
        reg(&["delete", RUN_KEY, "/v", APP, "/f"]);
    }
}

/// The app icon, made from assets/logo.png by tools/icon.ps1.
const TRAY_ICON: &[u8] = include_bytes!("../assets/tray32.rgba");


/// Starts another wisp.exe: the app window, or a restarted Wisp.
fn spawn_self(args: &[&str]) {
    let _ = Command::new(env::current_exe().unwrap_or_default()).args(args).spawn();
}

/// Opens the app window, a separate process that exits when closed so it costs nothing while hidden.
fn open_window() {
    spawn_self(&["--window"]);
}

/// Opens the window on a page ("settings", or "change" for the Change wallpaper sheet). Passed through a file,
/// so a window that is already open goes there too.
fn open_window_at(page: &str) {
    let _ = fs::write(goto_path(), page);
    open_window();
}

/// The background process's hidden message window (see main).
const MAILBOX: &str = "wisp-background";

/// Tells the background process the config changed, so it applies it now.
fn wake_background() {
    unsafe {
        let mailbox = FindWindowExW(HWND_MESSAGE, null_mut(), w("Static").as_ptr(), w(MAILBOX).as_ptr());
        if !mailbox.is_null() {
            PostMessageW(mailbox, WM_USER, 0, 0);
        }
    }
}

fn goto_path() -> PathBuf {
    Config::path().with_file_name("goto.txt")
}

/// What the background process is doing, for the window: one line per screen, then memory.
/// screenN = playing | starting | covered | game | battery | paused | broken | waiting | empty.
fn status_path() -> PathBuf {
    Config::path().with_file_name("status.txt")
}

fn on_battery() -> bool {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut s: SYSTEM_POWER_STATUS = unsafe { zeroed() };
    // ACLineStatus: 0 unplugged, 1 plugged in, 255 unknown (desktops).
    unsafe { GetSystemPowerStatus(&mut s) != 0 && s.ACLineStatus == 0 }
}

/// This process's CPU and GPU use, the way Task Manager shows them.
#[derive(Default)]
struct Usage {
    /// The GPU counters (a PDH query and its counter), opened on first use.
    query: isize,
    gpu: isize,
    /// When CPU time was last read, and the process's total CPU time then (100 ns units).
    cpu_at: Option<(Instant, u64)>,
}

impl Usage {
    /// (CPU %, GPU %) since the last call. CPU is of the whole machine (all cores = 100); GPU is the busiest engine
    /// type (3D, video decode...) summed over this process's engines, like Task Manager's GPU column.
    fn sample(&mut self) -> (f32, f32) {
        use windows_sys::Win32::System::{Performance::*, Threading::{GetCurrentProcessId, GetProcessTimes}};
        let mut t: [FILETIME; 4] = unsafe { zeroed() };
        let busy = if unsafe { GetProcessTimes(GetCurrentProcess(), &mut t[0], &mut t[1], &mut t[2], &mut t[3]) } != 0 {
            let ft = |f: &FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
            ft(&t[2]) + ft(&t[3])
        } else {
            0
        };
        let now = Instant::now();
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f32;
        let cpu = self.cpu_at.map_or(0.0, |(at, was)| busy.saturating_sub(was) as f32 / 1e7 / now.duration_since(at).as_secs_f32().max(0.001) / cores * 100.0);
        self.cpu_at = Some((now, busy));
        unsafe {
            if self.query == 0 && PdhOpenQueryW(null(), 0, &mut self.query) == 0 {
                PdhAddEnglishCounterW(self.query, w(r"\GPU Engine(*)\Utilization Percentage").as_ptr(), 0, &mut self.gpu);
            }
            if self.gpu == 0 || PdhCollectQueryData(self.query) != 0 {
                return (cpu, 0.0);
            }
            let (mut size, mut n) = (0u32, 0u32);
            if PdhGetFormattedCounterArrayW(self.gpu, PDH_FMT_DOUBLE, &mut size, &mut n, null_mut()) != PDH_MORE_DATA {
                return (cpu, 0.0);
            }
            // u64s, so the items inside are aligned.
            let mut buf = vec![0u64; size as usize / 8 + 1];
            let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(self.gpu, PDH_FMT_DOUBLE, &mut size, &mut n, items) != 0 {
                return (cpu, 0.0);
            }
            // Instances look like "pid_1234_luid_..._eng_0_engtype_3D".
            let mine = format!("pid_{}_", GetCurrentProcessId());
            let mut by_type: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
            for item in std::slice::from_raw_parts(items, n as usize) {
                if item.FmtValue.CStatus != 0 || item.szName.is_null() {
                    continue;
                }
                let len = (0..).take_while(|&k| *item.szName.add(k) != 0).count();
                let name = String::from_utf16_lossy(std::slice::from_raw_parts(item.szName, len));
                if let Some(kind) = name.strip_prefix(&mine).and_then(|r| r.split("engtype_").nth(1)) {
                    *by_type.entry(kind.to_string()).or_default() += item.FmtValue.Anonymous.doubleValue;
                }
            }
            (cpu, by_type.values().fold(0.0_f64, |a, b| a.max(*b)).min(100.0) as f32)
        }
    }
}

/// This process's memory in MB, as Task Manager's "Memory" column counts it (the working set).
fn memory_mb() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    let mut c: PROCESS_MEMORY_COUNTERS = unsafe { zeroed() };
    c.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) } == 0 {
        return 0;
    }
    c.WorkingSetSize as u64 / 1_048_576
}

/// The cached lighter copy of each monitor's wallpaper.
fn copies(cfg: &Config, slots: &[Slot]) -> Vec<PathBuf> {
    slots.iter().enumerate().map(|(i, s)| cache_path(&cfg.files[i], target(cfg, &s.rect))).collect()
}

/// Writes a crash (a Rust panic, which otherwise ends the process without a trace) to crash.txt next to the config,
/// with which part of Wisp it was and where, so it can be found and fixed.
fn log_crashes() {
    let part = if env::args().any(|a| a == "--window") { "window" } else { "background" };
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let at = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        let text = format!("[{at}] Wisp {} {part}, thread {thread}: {info}\n", env!("CARGO_PKG_VERSION"));
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(Config::path().with_file_name("crash.txt")) {
            use std::io::Write;
            let _ = f.write_all(text.as_bytes());
        }
        default(info);
    }));
}

fn main() {
    log_crashes();
    if env::args().any(|a| a == "--window") {
        window::run();
        return;
    }
    // Prototype: `wisp.exe --scene <Workshop item folder>` previews a Wallpaper Engine scene.
    if let Some(i) = env::args().position(|a| a == "--scene") {
        scene::preview(&env::args().nth(i + 1).unwrap_or_default());
        return;
    }
    // `wisp.exe --subscribe <app> <item>`: subscribes to a Workshop item through Steam (see run_subscribe).
    // `wisp.exe --remove <path>`: unsubscribes from a Workshop wallpaper and deletes it (see remove_workshop_item).
    if let Some(i) = env::args().position(|a| a == "--remove") {
        remove_workshop_item(&env::args().nth(i + 1).unwrap_or_default());
        return;
    }
    if let Some(i) = env::args().position(|a| a == "--subscribe" || a == "--steam-download") {
        let arg = |k: usize| env::args().nth(i + k).unwrap_or_default();
        let job = if arg(0) == "--steam-download" { SteamJob::Download } else { SteamJob::Subscribe };
        std::process::exit(run_subscribe(&arg(1), &arg(2), job));
    }
    // `wisp.exe --scene-shot <folder> <out.png> [settings]`: test render of a scene, with a report of what failed.
    if let Some(i) = env::args().position(|a| a == "--scene-shot") {
        let arg = |k: usize| env::args().nth(i + k).unwrap_or_default();
        scene::snapshot(&arg(1), &arg(2), &arg(3));
        return;
    }
    // `wisp.exe --scene-wall <folder> <window> <fit> <fps> [settings]`: the per-screen scene player Wisp starts.
    if let Some(i) = env::args().position(|a| a == "--scene-wall") {
        let a: Vec<String> = env::args().skip(i + 1).collect();
        let arg = |k: usize| a.get(k).cloned().unwrap_or_default();
        scene::run_wallpaper(&arg(0), arg(1).parse().unwrap_or(0), arg(3).parse().unwrap_or(60), &arg(4), &AtomicBool::new(false), None, &scene::Live::new(&arg(2), (0.0, 0.0), 1.0));
        return;
    }

    // One background instance only: two would fight over the desktop.
    let mutex = unsafe {
        let m = CreateMutexW(null(), 0, w("wisp-single-instance").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // Started again while running (say, double-clicked): open the window instead.
            open_window();
            return;
        }
        m
    };
    unsafe {
        // Without this, Windows hands out scaled coordinates on monitors above 100% scaling.
        SetProcessDPIAware();
        // Videos and scenes play in this process now: keep them behind games and apps, as mpv.exe was.
        SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS);
    }
    if !mpv::available() {
        message("libmpv-2.dll is missing next to wisp.exe, so videos can't play. Scenes still work.", MB_ICONWARNING);
    }
    // Copies cut short by a quit or crash never finish; start clean.
    for e in fs::read_dir(cache_dir()).into_iter().flatten().flatten() {
        if e.path().extension().is_some_and(|x| x == "part") {
            let _ = fs::remove_file(e.path());
        }
    }
    update::clean_up();
    let update_ready = Arc::new(std::sync::Mutex::new(None));
    update::watch(update_ready.clone());

    // The app was called livewall, then Lumen: move the autostart entry and screen-sized copies over.
    let mut was_autostart = false;
    for old in ["livewall", "Lumen"] {
        was_autostart |= reg(&["delete", RUN_KEY, "/v", old, "/f"]);
    }
    // Also refresh our own entry, so moving wisp.exe to another folder doesn't break "Start with Windows".
    if was_autostart || reg(&["query", RUN_KEY, "/v", APP]) {
        set_autostart(true);
    }
    let old_cache = PathBuf::from(env::var("LOCALAPPDATA").unwrap_or_default()).join(r"Lumen\cache");
    if !cache_dir().exists() && old_cache.exists() {
        let _ = fs::create_dir_all(cache_dir().parent().unwrap());
        let _ = fs::rename(old_cache, cache_dir());
    }

    let rects = monitors();
    let mut cfg = Config::load();
    cfg.fit_to(rects.len());
    // The window and this process share the config file: `seen` is the text last read or written here.
    let mut seen = cfg.save();
    let first_run = cfg.files.iter().all(|f| f.is_empty()) && cfg.recent.is_empty();

    let mut slots: Vec<Slot> = areas(&rects, cfg.span)
        .into_iter()
        .map(|(rect, works)| Slot { rect, works, tried: String::new(), shrinking: None, player: None, old: None, old_since: Instant::now(), fade_from: None, started: Instant::now(), fails: 0 })
        .collect();

    let open = MenuItem::new("Open Wisp", true, None);
    let pause = CheckMenuItem::new("Pause all", true, cfg.paused, None);
    let sound = CheckMenuItem::new("Sound", true, cfg.audio, None);
    let change = MenuItem::new("Change wallpaper…", true, None);
    let settings = MenuItem::new("Settings", true, None);
    let quit = MenuItem::new("Quit Wisp", true, None);
    let menu = Menu::new();
    let sep = PredefinedMenuItem::separator;
    menu.append_items(&[&open, &pause, &sound, &sep(), &change, &settings, &sep(), &quit]).unwrap();
    let mut last_status = String::new();
    let mut usage = Usage::default();
    let mut shown_usage: Option<(f32, f32)> = None;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip(APP)
        .with_icon(Icon::from_rgba(TRAY_ICON.to_vec(), 32, 32).unwrap())
        .build()
        .unwrap();
    if first_run {
        open_window();
    }

    unsafe {
        let slow = SetTimer(null_mut(), 0, 1000, None);
        // A quick tick while a new wallpaper loads behind the old one, so the swap happens the moment it's ready.
        let mut fast = 0;
        // A hidden mailbox the window pokes after saving (see wake_background), so a change applies at once
        // instead of on the next once-a-second tick.
        CreateWindowExW(0, w("Static").as_ptr(), w(MAILBOX).as_ptr(), 0, 0, 0, 0, 0, HWND_MESSAGE, null_mut(), null_mut(), null());
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);

            // The quick tick only drives a wallpaper change (is the new one up, the fade); everything else waits
            // for the next real event or the once-a-second tick.
            if msg.message == WM_TIMER && fast != 0 && msg.wParam == fast {
                slots.iter_mut().for_each(Slot::fade_old);
                if !slots.iter().any(|s| s.old.is_some()) {
                    KillTimer(null_mut(), fast);
                    fast = 0;
                }
                continue;
            }

            while let Ok(e) = TrayIconEvent::receiver().try_recv() {
                if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                    open_window();
                }
            }
            while let Ok(e) = MenuEvent::receiver().try_recv() {
                if e.id == open.id() {
                    open_window();
                } else if e.id == pause.id() {
                    cfg.paused = pause.is_checked();
                    slots.iter_mut().for_each(|s| s.fails = 0);
                    seen = cfg.save();
                } else if e.id == sound.id() {
                    cfg.audio = sound.is_checked();
                    seen = cfg.save();
                } else if e.id == change.id() {
                    open_window_at("change");
                } else if e.id == settings.id() {
                    open_window_at("settings");
                } else if e.id == quit.id() {
                    slots.iter_mut().for_each(Slot::stop_all);
                    return;
                }
            }

            // The window changed the config: apply it, restarting only the screens that changed.
            let disk = Config::read_disk().unwrap_or_else(|| seen.clone());
            if disk != seen {
                let mut new = Config::parse(&disk);
                // Spanning turned on or off: the wallpaper areas change, so start over cleanly (about a second).
                if new.span != cfg.span {
                    slots.iter_mut().for_each(Slot::stop_all);
                    CloseHandle(mutex);
                    spawn_self(&[]);
                    return;
                }
                new.fit_to(slots.len());
                let old_copies = copies(&cfg, &slots);
                for (i, s) in slots.iter_mut().enumerate() {
                    let file_changed = new.files[i] != cfg.files[i];
                    let settings_changed = new.settings.get(&new.files[i]) != cfg.settings.get(&cfg.files[i]);
                    let everything = new.fps != cfg.fps || new.quality != cfg.quality || new.paused != cfg.paused;
                    let placed = new.fits[i] != cfg.fits[i] || new.pans[i] != cfg.pans[i] || new.zooms[i] != cfg.zooms[i];
                    if file_changed || settings_changed || everything {
                        s.fails = 0;
                        // Video to video: the playing player opens the new file, so it's there at once.
                        let file = &new.files[i];
                        let copy = cache_path(file, target(&new, &s.rect));
                        let play = if copy.exists() { copy.display().to_string() } else { file.clone() };
                        let swapped = file_changed
                            && !settings_changed
                            && !everything
                            && !file.is_empty()
                            && !is_scene(file)
                            && s.old.is_none()
                            && s.player.as_mut().is_some_and(|p| p.swap(&play, file, &new.fits[i], new.pans[i], new.zooms[i]));
                        if swapped {
                            s.started = Instant::now();
                        } else {
                            s.replace();
                        }
                    } else if placed && !s.player.as_ref().is_some_and(|p| p.place(&new.fits[i], new.pans[i], new.zooms[i])) {
                        s.replace();
                    }
                    // A removed wallpaper: delete its screen-sized copy, unless another screen still plays it.
                    if new.files[i].is_empty() && !cfg.files[i].is_empty() && old_copies.iter().filter(|c| **c == old_copies[i]).count() == 1 {
                        let _ = fs::remove_file(&old_copies[i]);
                    }
                }
                cfg = new;
                pause.set_checked(cfg.paused);
                sound.set_checked(cfg.audio);
                seen = disk;
            }

            // Resolution or monitors changed: the surfaces are per monitor, so start over cleanly.
            let now = monitors();
            if !now.is_empty() && layout(&now) != layout(&rects) {
                slots.iter_mut().for_each(Slot::stop_all);
                CloseHandle(mutex);
                spawn_self(&[]);
                return;
            }

            // The user said yes to an update and it's downloaded: stop the wallpapers, swap the files, restart.
            let files = update_ready.lock().unwrap().take();
            if let Some(files) = files {
                slots.iter_mut().for_each(Slot::stop_all);
                match update::apply(&files) {
                    Ok(()) => {
                        CloseHandle(mutex);
                        spawn_self(&[]);
                        return;
                    }
                    Err(e) => {
                        message(&e, MB_ICONWARNING);
                    }
                }
            }

            // mpv is killed, not paused, while a game is fullscreen or windows hide the desktop:
            // that frees all its RAM, VRAM and GPU time. It restarts in about a second.
            // Why every screen is stopped, if it is (the window shows it, so a stopped wallpaper never looks broken).
            let idle_why = if cfg.paused {
                Some("paused")
            } else if cfg.game_pause && fullscreen_app() {
                Some("game")
            } else if cfg.battery_pause && on_battery() {
                Some("battery")
            } else {
                None
            };
            let idle = idle_why.is_some();
            let mut status = String::new();
            let mut shrunk = false;
            for (i, s) in slots.iter_mut().enumerate() {
                // A pasted Workshop item finished downloading: switch to it.
                // Steam downloads into workshop\downloads and moves the item to workshop\content when done.
                let found = (!cfg.waits[i].is_empty()).then(|| find_item(&cfg.waits[i])).flatten();
                if let Some((item, app)) = found {
                    cfg.waits[i].clear();
                    if let Some(f) = item_video(&item, &app) {
                        cfg.set_file(i, f);
                        s.fails = 0;
                        s.replace();
                    }
                    seen = cfg.save();
                }
                let file = &cfg.files[i];
                let t = target(&cfg, &s.rect);
                let copy = cache_path(file, t);
                // Each file and target is tried once; a new quality or frame rate cap makes a new copy.
                let key = format!("{file}|{t:?}");
                if !file.is_empty() && !is_scene(file) && s.tried != key && s.shrinking.is_none() && !copy.exists() {
                    s.tried = key;
                    let (f, out) = (file.clone(), copy.clone());
                    s.shrinking = Some(std::thread::spawn(move || shrink(f, t, out)));
                }
                if s.shrinking.as_ref().is_some_and(|t| t.is_finished()) && s.shrinking.take().unwrap().join().unwrap_or(false) {
                    // The copy is ready: the player switches to it in place (or starts over on it).
                    // The copy must be this file's: after a quick switch, the finished one can be for the last file.
                    let c = copy.display().to_string();
                    if !copy.exists() || !s.player.as_mut().is_some_and(|p| p.swap(&c, file, &cfg.fits[i], cfg.pans[i], cfg.zooms[i])) {
                        s.replace();
                    }
                    shrunk = true;
                }

                // 3 quick deaths in a row means the file is unplayable: stop retrying it.
                // A spanning wallpaper pauses only when every screen is covered.
                let hidden = s.works.iter().all(covered);
                let want = !idle && !file.is_empty() && s.fails < 3 && !hidden;
                let state = if !cfg.waits[i].is_empty() {
                    "waiting"
                } else if file.is_empty() {
                    "empty"
                } else if s.fails >= 3 {
                    "broken"
                } else if let Some(why) = idle_why {
                    why
                } else if hidden {
                    "covered"
                } else if s.player.is_some() && s.started.elapsed().as_secs() >= 2 {
                    "playing"
                } else {
                    "starting"
                };
                status += &format!("screen{i}={state}\n");
                if !want {
                    s.stop_all();
                } else if s.player.as_mut().is_some_and(|p| !p.alive()) {
                    // The player stopped: Explorer restarted, or the file is unplayable.
                    stop(&mut s.player);
                    s.fails = if s.started.elapsed().as_secs() < 3 { s.fails + 1 } else { 0 };
                } else if s.player.is_none() {
                    // The frame rate cap lives in the copy. A playback cap costs RAM and saves no decoding,
                    // so the original plays uncapped for the few seconds until its copy is ready.
                    let play = if copy.exists() {
                        // Mark the copy as recently used, so the cache trim keeps it.
                        let _ = fs::File::options().write(true).open(&copy).and_then(|f| f.set_modified(SystemTime::now()));
                        copy.display().to_string()
                    } else {
                        file.clone()
                    };
                    s.player = spawn(&cfg.quality, cfg.fps, &play, file, &cfg.fits[i], &s.rect, &cfg.settings_json(file), cfg.pans[i], cfg.zooms[i]);
                    s.started = Instant::now();
                    // Behind the wallpaper it replaces, which goes once this one is up.
                    if let (Some(p), Some(_)) = (&s.player, &s.old) {
                        SetWindowPos(p.surface, HWND_BOTTOM, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                    }
                } else if let Some(p) = s.player.as_mut() {
                    p.trim();
                }
                // The replaced wallpaper fades out once the new one shows its first frame (or failed, or took too long).
                s.fade_old();
            }
            // Sound comes from one screen only, so two never echo or clash: the first one playing. When it gets
            // covered and stops, the next playing screen takes over, live (sound on, volume and changes need no restart).
            let sound_from = slots.iter().position(|s| s.player.is_some());
            for (i, s) in slots.iter_mut().enumerate() {
                if let Some(p) = s.player.as_mut() {
                    p.sound(cfg.audio && sound_from == Some(i), cfg.volume);
                }
            }
            if shrunk {
                trim_cache(&copies(&cfg, &slots));
            }
            // Memory moves a little every second: round it so the file isn't rewritten each time.
            status += &format!("ram={}\n", memory_mb().div_ceil(2) * 2);
            let tick = msg.message == WM_TIMER && msg.wParam == slow;
            let loading = slots.iter().any(|s| s.old.is_some());
            if loading && fast == 0 {
                fast = SetTimer(null_mut(), 0, 16, None);
            } else if !loading && fast != 0 {
                KillTimer(null_mut(), fast);
                fast = 0;
            }
            // CPU and GPU only while the window is open to show them: the GPU counters cost a little to read.
            if tick {
                shown_usage = (!FindWindowW(null(), w(APP).as_ptr()).is_null()).then(|| usage.sample());
                // Window closed: the next reading starts fresh instead of averaging over the closed time.
                if shown_usage.is_none() {
                    usage.cpu_at = None;
                }
            }
            if let Some((cpu, gpu)) = shown_usage {
                status += &format!("cpu={cpu:.1}\ngpu={gpu:.0}\n");
            }
            if status != last_status {
                let playing = status.matches("=playing").count();
                let tip = if cfg.paused { "Wisp · paused".to_string() } else if playing > 0 { format!("Wisp · playing on {playing} screen{}", if playing == 1 { "" } else { "s" }) } else { "Wisp".to_string() };
                let _ = tray.set_tooltip(Some(tip));
                let _ = fs::write(status_path(), &status);
                last_status = status;
            } else if !last_status.is_empty() && tick {
                // Touched every few seconds, so the window can tell a running Wisp from one that quit.
                let _ = fs::File::options().write(true).open(status_path()).and_then(|f| f.set_modified(SystemTime::now()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_parses() {
        let c = Config::parse("file=C:\\a=b.mp4\nfile2=x.gif\nfit2=fit\nfps=24\nfit=stretch\npaused=true\nfit1=bogus\n");
        assert_eq!(c.files, ["C:\\a=b.mp4", "", "x.gif"]);
        // The old global "fit" fills every screen without its own fit.
        assert_eq!(c.fits, ["stretch", "stretch", "fit"]);
        assert_eq!((c.fps, c.paused), (24, true));
        // Sound is off until turned on; volume is kept within 0..100.
        assert_eq!((c.audio, c.volume), (false, 50));
        let c = Config::parse(&format!("{}audio=true\nvolume=250\n", c.save_text()));
        assert_eq!((c.audio, c.volume), (true, 100));
    }

    #[test]
    fn pause_options_saved() {
        // On unless turned off, so older configs keep pausing for games and get battery saving.
        let c = Config::parse("");
        assert!(c.game_pause && c.battery_pause && !c.told_tray);
        let c = Config::parse(&Config { game_pause: false, told_tray: true, ..c }.save_text());
        assert!(!c.game_pause && c.battery_pause && c.told_tray);
    }

    #[test]
    fn spans_screens() {
        let r = |left, top, right, bottom| RECT { left, top, right, bottom };
        // A landscape 1080p screen with a portrait one to its right, a bit higher up (like this PC).
        let mons = [(r(0, 0, 1920, 1080), r(0, 0, 1920, 1032)), (r(1920, -400, 3000, 1520), r(1920, -400, 3000, 1520))];
        assert_eq!(areas(&mons, false).len(), 2);
        let span = areas(&mons, true);
        assert_eq!(span.len(), 1);
        let (all, works) = &span[0];
        assert_eq!((all.left, all.top, all.right, all.bottom), (0, -400, 3000, 1520));
        assert_eq!(works.len(), 2);
        // One screen: nothing to span.
        assert_eq!(areas(&mons[..1], true).len(), 1);
        // The setting is saved.
        assert!(Config::parse(&format!("{}span=true\n", Config::parse("").save_text())).span);
    }

    #[test]
    fn saves_positions() {
        let mut c = Config::parse("file0=a.mp4\nfile1=b.mp4\nplace1=0.5,-1,1.5\nplace0=9,x,1\npan0=0.2,0.1\n");
        c.fit_to(2);
        // A bad value and the old "pan" keys are ignored; a good one kept; untouched screens write nothing.
        assert_eq!(c.pans, [(0.0, 0.0), (0.5, -1.0)]);
        assert_eq!(c.zooms, [1.0, 1.5]);
        let text = c.save_text();
        assert!(text.contains("place1=0.5,-1,1.5") && !text.contains("place0="));
        // Out of range is held in range.
        let c = Config::parse("place0=2,-3,9\n");
        assert_eq!((c.pans[0], c.zooms[0]), ((1.0, -1.0), MAX_ZOOM));
    }

    #[test]
    fn places_pictures() {
        let a = 16.0 / 9.0;
        let px = |r: [f32; 4], w: f32, h: f32| [(r[0] * w).round(), (r[1] * h).round(), (r[2] * w).round(), (r[3] * h).round()];
        // A 16:9 picture on a portrait 1080x1920 screen, and on 4:3 and matching screens, in pixels.
        let tall = 1080.0 / 1920.0;
        assert_eq!(px(place(a, tall, "fill", (0.0, 0.0), 1.0), 1080.0, 1920.0), [-1167.0, 0.0, 3413.0, 1920.0]);
        assert_eq!(px(place(a, tall, "fit", (0.0, 0.0), 1.0), 1080.0, 1920.0), [0.0, 656.0, 1080.0, 608.0]);
        assert_eq!(px(place(a, 4.0 / 3.0, "fill", (0.0, 0.0), 1.0), 1440.0, 1080.0), [-240.0, 0.0, 1920.0, 1080.0]);
        assert_eq!(px(place(a, a, "fill", (0.0, 0.0), 1.0), 1920.0, 1080.0), [0.0, 0.0, 1920.0, 1080.0]);
        assert_eq!(place(a, tall, "stretch", (1.0, 1.0), 2.0), [0.0, 0.0, 1.0, 1.0]);
        // Moved all the way, an edge lines up with the screen's: never a gap, never zoomed by moving.
        for (x, y, z) in [(1.0, 1.0, 1.0), (-1.0, -1.0, 1.0), (0.3, -0.7, 2.0), (-1.0, 1.0, 3.0)] {
            let [l, t, w, h] = place(a, tall, "fill", (x, y), z);
            assert!(l <= 1e-6 && t <= 1e-6 && l + w >= 1.0 - 1e-6 && t + h >= 1.0 - 1e-6, "gap at {x},{y},{z}");
        }
        assert_eq!(place(a, tall, "fill", (1.0, 0.0), 1.0)[0], 0.0);
        // A matching shape has nothing cut off, so it only moves once zoomed.
        assert_eq!(place(a, a, "fill", (1.0, 1.0), 1.0), [0.0, 0.0, 1.0, 1.0]);
        // mpv lines up the same edge: left edge on the left is align -1.
        assert_eq!(mpv_place("fill", (1.0, -0.5), 2.0)[0..2], [("video-align-x", "-1".to_string()), ("video-align-y", "0.5".to_string())]);
        assert_eq!(mpv_place("fit", (1.0, 1.0), 2.0)[2], ("video-zoom", "0".to_string()));
    }

    #[test]
    fn scene_settings_saved() {
        let mut c = Config::parse("file=C:\\w\\1\\scene.pkg\nsetting=C:\\w\\1\\scene.pkg|clock|false\nsetting=C:\\w\\1\\scene.pkg|tint|\"0.1 0.2 0.3\"\nsetting=C:\\gone\\scene.pkg|x|1\n");
        assert_eq!(c.settings_json("C:\\w\\1\\scene.pkg"), r#"{"clock":false,"tint":"0.1 0.2 0.3"}"#);
        assert_eq!(c.settings_json("other"), "{}");
        // Saving keeps settings of wallpapers in use and drops the rest.
        c.recent.clear();
        let text = c.save_text();
        assert!(text.contains("setting=C:\\w\\1\\scene.pkg|clock|false") && !text.contains("gone"));
        assert_eq!(Config::parse(&text).settings, c.settings.into_iter().filter(|(f, _)| !f.contains("gone")).collect());
    }

    #[test]
    fn workshop_links() {
        let id = |s: &str| workshop_id(s);
        assert_eq!(id("https://steamcommunity.com/sharedfiles/filedetails/?id=1234567890").as_deref(), Some("1234567890"));
        assert_eq!(id("https://steamcommunity.com/sharedfiles/filedetails/?searchtext=x&id=42&l=en").as_deref(), Some("42"));
        assert_eq!(id("  987654321\r\n").as_deref(), Some("987654321"));
        assert_eq!(id("https://steamcommunity.com/sharedfiles/filedetails/?searchtext=paid=1"), None);
        assert_eq!(id("hello"), None);
        assert_eq!(id(""), None);
    }

    #[test]
    fn workshop_items() {
        // Real project.json files nest a "type": "color" before the top-level "type", so only "file" is trusted.
        let j = r#"{"contentrating":"Everyone","file":"my clip.mp4","general":{"properties":{"schemecolor":{"type":"color"}}},"preview":"preview.gif","type":"video"}"#;
        assert_eq!(json_str(j, "file").as_deref(), Some("my clip.mp4"));

        let root = env::temp_dir().join(format!("wisp-test-{}", std::process::id()));
        let (video, scene) = (root.join("1"), root.join("2"));
        fs::create_dir_all(&video).unwrap();
        fs::create_dir_all(&scene).unwrap();
        fs::write(video.join("project.json"), j).unwrap();
        fs::write(video.join("my clip.mp4"), b"x").unwrap();
        fs::write(scene.join("project.json"), r#"{"file" : "scene.json", "type":"scene"}"#).unwrap();
        fs::write(scene.join("scene.json"), b"{}").unwrap();
        assert_eq!(workshop_video(&video, WALLPAPER_ENGINE), Some(video.join("my clip.mp4")));
        assert_eq!(workshop_video(&scene, WALLPAPER_ENGINE), None);
        assert_eq!(workshop_video(&root.join("missing"), WALLPAPER_ENGINE), None);

        // Other apps: the biggest video that isn't a preview.
        let other = root.join("3");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("preview.mp4"), vec![0u8; 900]).unwrap();
        fs::write(other.join("small.webm"), vec![0u8; 10]).unwrap();
        fs::write(other.join("Celestial Veil.MP4"), vec![0u8; 500]).unwrap();
        fs::write(other.join("notes.txt"), vec![0u8; 5000]).unwrap();
        assert_eq!(workshop_video(&other, "4056970"), Some(other.join("Celestial Veil.MP4")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fullscreen_rect_check() {
        let mon = RECT { left: 0, top: 0, right: 1920, bottom: 1080 };
        assert!(covers(&RECT { left: 0, top: 0, right: 1920, bottom: 1080 }, &mon));
        // A maximized window stops above the taskbar, so it does not count.
        assert!(!covers(&RECT { left: -8, top: -8, right: 1928, bottom: 1040 }, &mon));
    }

    #[test]
    fn shrink_rules() {
        let t = |w, h, fps| Target { w, h, fps };
        // 4K onto 1440x1080 covers at 0.5 scale: worth it.
        assert_eq!(worth_shrinking(3840, 2160, 60.0, t(1440, 1080, 0)), (true, false));
        // A 1080p video onto a portrait 1080x1920 screen would need enlarging: not worth it.
        assert_eq!(worth_shrinking(1920, 1080, 30.0, t(1080, 1920, 0)), (false, false));
        // Almost the screen size already: not worth a copy.
        assert_eq!(worth_shrinking(1600, 1200, 30.0, t(1440, 1080, 0)), (false, false));
        // A 30 fps cap on a 60 fps video is worth a copy even at the same size; on a 30 fps one it isn't.
        assert_eq!(worth_shrinking(1920, 1080, 60.0, t(1920, 1080, 30)), (false, true));
        assert_eq!(worth_shrinking(1920, 1080, 29.97, t(1920, 1080, 30)), (false, false));
        // Same file and target: same copy. Any other size or cap: a different copy.
        assert_eq!(cache_path("a.mp4", t(1440, 1080, 0)), cache_path("a.mp4", t(1440, 1080, 0)));
        assert_ne!(cache_path("a.mp4", t(1440, 1080, 0)), cache_path("a.mp4", t(1080, 1920, 0)));
        assert_ne!(cache_path("a.mp4", t(1440, 1080, 0)), cache_path("a.mp4", t(1440, 1080, 30)));
        // Old quality values fall back to Sharp.
        assert_eq!(Config::parse("quality=low\n").quality, "sharp");
        assert_eq!(Config::parse("quality=anime\n").quality, "anime");
        assert_eq!(anime4k_shaders().split(';').count(), 6);
    }

    #[test]
    fn tray_icon() {
        assert_eq!(TRAY_ICON.len(), 32 * 32 * 4);
        assert_eq!(TRAY_ICON[3], 0, "corner is transparent");
        assert!(TRAY_ICON[(16 * 32 + 16) * 4 + 3] > 200, "middle is solid");
    }

    /// Asks Steam's public API, so it needs the internet. Run with: cargo test -- --ignored
    #[test]
    #[ignore]
    fn steam_lookup() {
        assert_eq!(workshop_app("3603384003").as_deref(), Some("4056970"));
        assert_eq!(app_name("4056970"), "Live Wallpaper");
        assert_eq!(workshop_app("1"), None);
    }

    /// Needs libmpv-2.dll beside the test exe and a video in Wisp's cache: run with --ignored.
    #[test]
    #[ignore]
    fn libmpv_jobs() {
        let video = fs::read_dir(cache_dir()).unwrap().flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x == "mp4")).unwrap();
        let video = video.display().to_string();
        let (w, h, fps) = video_info(&video).unwrap();
        assert!(w > 0 && h > 0 && fps > 0.0, "{w}x{h} {fps}");
        // A thumbnail, the way the window makes them: mpv writes a file through the "o" option.
        let out = env::temp_dir().join("wisp-libmpv-test.png");
        let _ = fs::remove_file(&out);
        let o = out.display().to_string();
        assert!(mpv::run(&[("audio", "no"), ("start", "25%"), ("frames", "1"), ("vf", "scale=480:-2"), ("ovc", "png"), ("o", &o)], &video));
        assert!(image::open(&out).is_ok_and(|i| i.width() == 480));
        // A lighter copy, encoded by Windows' H.264 encoder inside this process.
        let copy = env::temp_dir().join("wisp-libmpv-test.mp4");
        let _ = fs::remove_file(&copy);
        assert!(shrink(video.clone(), Target { w: 640, h: 360, fps: 0 }, copy.clone()));
        assert_eq!(video_size(&copy.display().to_string()).map(|(_, h)| h), Some(360));
        // A player that can't open its file reports it as ended.
        let m = mpv::Mpv::new(&[("vo", "null"), ("ao", "null")]).unwrap();
        assert!(m.command(&["loadfile", r"C:\nope\missing.mp4"]));
        let until = Instant::now() + Duration::from_secs(5);
        let mut ended = None;
        while ended.is_none() && Instant::now() < until {
            ended = m.ended();
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(matches!(ended, Some(mpv::Ended::Failed)));
    }

    /// Changes a playing wallpaper without restarting it: video to video, fit, move and zoom, and when each shows its
    /// first frame. Run with --ignored.
    #[test]
    #[ignore]
    fn switches_in_place() {
        // Plays on the first screen for a few seconds. Needs two screen-sized copies in the cache and the scene below.
        let mon = monitors()[0].0;
        let videos: Vec<String> = fs::read_dir(cache_dir()).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "mp4")).take(2).map(|p| p.display().to_string()).collect();
        let until_ready = |p: &Player| {
            let t = Instant::now();
            while !p.ready() && t.elapsed().as_secs() < 15 {
                pump_for(Duration::from_millis(20));
            }
            assert!(p.ready(), "never showed a frame");
            t.elapsed()
        };
        let started = Instant::now();
        let mut p = spawn("sharp", 60, &videos[0], &videos[0], "fill", &mon, "{}", (0.0, 0.0), 1.0).unwrap();
        println!("video started in {:?}", started.elapsed() + until_ready(&p));
        assert!(p.place("stretch", (0.0, 0.0), 1.0) && p.place("fit", (0.0, 0.0), 1.0) && p.place("fill", (0.5, -0.5), 2.0), "live fit change");
        pump_for(Duration::from_millis(500));
        let t = Instant::now();
        let next = videos.get(1).unwrap_or(&videos[0]);
        assert!(p.swap(next, next, "fill", (0.0, 0.0), 1.0));
        // A fresh file counts frames from 0 again.
        pump_for(Duration::from_millis(30));
        println!("swapped to another video in {:?}", t.elapsed() + until_ready(&p));
        assert!(p.alive());
        stop(&mut Some(p));
        let scene = r"C:\Program Files (x86)\Steam\steamapps\workshop\content\431960\3411756828\scene.pkg";
        let t = Instant::now();
        let mut p = spawn("sharp", 60, scene, scene, "fill", &mon, "{}", (0.0, 0.0), 1.0).unwrap();
        println!("scene showed its first frame in {:?}", t.elapsed() + until_ready(&p));
        assert!(p.place("fit", (0.0, 0.0), 1.0), "scenes move live");
        pump_for(Duration::from_millis(500));
        assert!(p.alive());
        stop(&mut Some(p));
    }

    /// Plays a video and a scene in this process the way the background loop does, then starts and stops them
    /// again to catch leaks. Needs libmpv-2.dll, a cached video and Wallpaper Engine scenes: run with --ignored.
    #[test]
    #[ignore]
    fn players_in_process() {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetGuiResources};
        let ram = || {
            std::thread::sleep(Duration::from_millis(300));
            let out = Command::new("powershell").args(["-NoProfile", "-Command", &format!("[int]((Get-CimInstance Win32_PerfFormattedData_PerfProc_Process -Filter 'IDProcess={}').WorkingSetPrivate/1MB)", std::process::id())]).output().unwrap();
            String::from_utf8_lossy(&out.stdout).trim().parse::<i32>().unwrap_or(-1)
        };
        let objects = || unsafe { (GetGuiResources(GetCurrentProcess(), 0), GetGuiResources(GetCurrentProcess(), 1)) };
        let mon = monitors()[0].0;
        let video = fs::read_dir(cache_dir()).unwrap().flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x == "mp4")).unwrap().display().to_string();
        let scene = r"C:\Program Files (x86)\Steam\steamapps\workshop\content\431960\3411756828\scene.pkg";
        for (name, file) in [("video", video.as_str()), ("scene", scene)] {
            let mut p = spawn("anime", 60, file, file, "fill", &mon, "{}", (0.0, 0.0), 1.0).unwrap();
            for _ in 0..12 {
                pump_for(Duration::from_secs(1));
                assert!(p.alive(), "{name} stopped by itself");
                p.trim();
            }
            println!("{name}: playing RAM {}MB", ram());
            stop(&mut Some(p));
        }
        let before = (ram(), objects());
        for _ in 0..5 {
            for file in [video.as_str(), scene] {
                let mut p = Some(spawn("anime", 60, file, file, "fill", &mon, "{}", (0.0, 0.0), 1.0).unwrap());
                pump_for(Duration::from_secs(3));
                stop(&mut p);
            }
        }
        unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX) };
        let after = (ram(), objects());
        println!("after 5 restarts of each: RAM {}MB -> {}MB, GDI/USER objects {:?} -> {:?}", before.0, after.0, before.1, after.1);
        assert!(after.1 .0 <= before.1 .0 + 10 && after.1 .1 <= before.1 .1 + 10, "window or GDI objects leak");
    }

    /// A lighter copy plays its original's sound, and sound turns off and on live. Scene music has its own test
    /// (scene::plays_scene_music). Plays very quietly on this PC's speakers. Needs the fishing frogs video and its
    /// copy: run with --ignored.
    #[test]
    #[ignore]
    fn sound_follows_setting() {
        let mon = monitors()[0].0;
        let original = fs::read_dir(r"C:\Program Files (x86)\Steam\steamapps\workshop\content\431960\3737237256").unwrap().flatten().map(|e| e.path()).find(|p| is_video(p)).unwrap().display().to_string();
        let copy = cache_path(&original, Target { w: 1920, h: 1080, fps: 60 }).display().to_string();
        assert!(Path::new(&copy).exists(), "no copy yet: {copy}");
        let mut p = spawn("sharp", 60, &copy, &original, "fill", &mon, "{}", (0.0, 0.0), 1.0).unwrap();
        let track = |p: &Player| match &p.engine {
            Engine::Video(m) => m.get("current-tracks/audio/codec"),
            _ => None,
        };
        // Starts silent.
        pump_for(Duration::from_secs(2));
        assert!(track(&p).is_none());
        // On: the original's sound joins the copy, like the main loop does it (once a round).
        let until = Instant::now() + Duration::from_secs(5);
        while track(&p).is_none() {
            assert!(Instant::now() < until, "the copy plays without the original's sound");
            p.sound(true, 1);
            pump_for(Duration::from_millis(200));
        }
        // Off, then on again: no restart.
        p.sound(false, 1);
        pump_for(Duration::from_millis(500));
        assert!(track(&p).is_none());
        p.sound(true, 1);
        pump_for(Duration::from_millis(800));
        assert!(track(&p).is_some());
        stop(&mut Some(p));
    }

    #[test]
    fn finds_workshop_items() {
        let (app, item, folder) = workshop_item(r"C:\Program Files (x86)\Steam\steamapps\workshop\content\431960\3715870843\scene.pkg").unwrap();
        assert_eq!((app.as_str(), item.as_str()), ("431960", "3715870843"));
        assert_eq!(folder, PathBuf::from(r"C:\Program Files (x86)\Steam\steamapps\workshop\content\431960\3715870843"));
        assert!(workshop_item(r"E:\Videos\workshop\content\clip.mp4").is_none(), "not a Workshop item folder");
        assert!(workshop_item(r"C:\Users\me\Videos\wallpaper.mp4").is_none());
        // Removing a non-Workshop path deletes nothing.
        let mine = env::temp_dir().join("wisp-keep-me.txt");
        fs::write(&mine, "keep").unwrap();
        remove_workshop_item(&mine.display().to_string());
        assert!(mine.exists());
        let _ = fs::remove_file(&mine);
    }

    /// Subscribes to an item this account already has (so nothing changes) through Steam's library.
    /// Needs Steam running and Wallpaper Engine: run with --ignored.
    #[test]
    #[ignore]
    fn subscribes_through_steam() {
        println!("library: {:?}", steam_api());
        assert!(steam_api().is_some());
        assert_eq!(run_subscribe(WALLPAPER_ENGINE, "3411756828", SteamJob::Subscribe), 0);
        // Already downloaded: the download helper sees it installed and ends right away.
        let start = Instant::now();
        assert_eq!(run_subscribe(WALLPAPER_ENGINE, "3411756828", SteamJob::Download), 0);
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    /// Needs Wallpaper Engine items on this PC: run with --ignored.
    #[test]
    #[ignore]
    fn finds_installed_wallpapers() {
        let all = installed_wallpapers();
        assert!(!all.is_empty());
        assert!(all.iter().all(|p| Path::new(p).exists() && (is_scene(p) || is_video(Path::new(p)))));
        let mut unique: Vec<String> = all.iter().map(|p| p.to_lowercase().replace('/', r"\")).collect();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), all.len());
    }
}
