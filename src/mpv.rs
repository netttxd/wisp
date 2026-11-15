//! libmpv, loaded from libmpv-2.dll next to wisp.exe. Videos play inside Wisp (one process in Task Manager)
//! instead of in mpv.exe processes; making copies, thumbnails and reading a video's size use it too.

use super::*;
use std::ffi::{c_char, c_void, CStr, CString};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

/// The few libmpv functions Wisp needs (see mpv/client.h).
struct Api {
    create: unsafe extern "C" fn() -> *mut c_void,
    set_option_string: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> i32,
    initialize: unsafe extern "C" fn(*mut c_void) -> i32,
    command: unsafe extern "C" fn(*mut c_void, *const *const c_char) -> i32,
    terminate_destroy: unsafe extern "C" fn(*mut c_void),
    wait_event: unsafe extern "C" fn(*mut c_void, f64) -> *const Event,
    get_property_string: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_char,
    set_property_string: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char) -> i32,
    free: unsafe extern "C" fn(*mut c_void),
}

/// mpv_event from client.h.
#[repr(C)]
struct Event {
    event_id: i32,
    error: i32,
    reply_userdata: u64,
    data: *mut c_void,
}

const EVENT_NONE: i32 = 0;
const EVENT_SHUTDOWN: i32 = 1;
const EVENT_END_FILE: i32 = 7;
const EVENT_FILE_LOADED: i32 = 8;
/// mpv_end_file_reason: the file played to its end.
const END_EOF: i32 = 0;
/// mpv_end_file_reason: another file replaced it (Wisp switching videos in place).
const END_STOP: i32 = 2;

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let beside = env::current_exe().unwrap_or_default().with_file_name("libmpv-2.dll");
        let mut lib = LoadLibraryW(w(&beside.to_string_lossy()).as_ptr());
        if lib.is_null() {
            lib = LoadLibraryW(w("libmpv-2.dll").as_ptr());
        }
        if lib.is_null() {
            return None;
        }
        macro_rules! f {
            ($name:literal) => {
                std::mem::transmute(GetProcAddress(lib, concat!($name, "\0").as_ptr())?)
            };
        }
        Some(Api {
            create: f!("mpv_create"),
            set_option_string: f!("mpv_set_option_string"),
            initialize: f!("mpv_initialize"),
            command: f!("mpv_command"),
            terminate_destroy: f!("mpv_terminate_destroy"),
            wait_event: f!("mpv_wait_event"),
            get_property_string: f!("mpv_get_property_string"),
            set_property_string: f!("mpv_set_property_string"),
            free: f!("mpv_free"),
        })
    })
    .as_ref()
}

/// Whether libmpv-2.dll could be loaded, for a clear message instead of black screens.
pub fn available() -> bool {
    api().is_some()
}

/// One mpv player. Dropping it stops playback and frees it.
pub struct Mpv(*mut c_void);

// libmpv handles may be used from any thread (client.h: "all functions are thread-safe").
unsafe impl Send for Mpv {}

/// What a finished file came to.
pub enum Ended {
    /// Played to the end.
    Done,
    /// Unplayable, or mpv stopped.
    Failed,
}

impl Mpv {
    /// A player with these options (mpv's command-line options without "--"), not yet playing anything.
    pub fn new(options: &[(&str, &str)]) -> Option<Mpv> {
        let api = api()?;
        unsafe {
            let h = (api.create)();
            if h.is_null() {
                return None;
            }
            let mpv = Mpv(h);
            for (k, v) in options {
                let (k, v) = (CString::new(*k).ok()?, CString::new(*v).ok()?);
                // A bad option fails like it would on mpv's command line: the player isn't made.
                if (api.set_option_string)(h, k.as_ptr(), v.as_ptr()) < 0 {
                    return None;
                }
            }
            ((api.initialize)(h) >= 0).then_some(mpv)
        }
    }

    pub fn command(&self, args: &[&str]) -> bool {
        let Some(api) = api() else { return false };
        let owned: Vec<CString> = args.iter().filter_map(|a| CString::new(*a).ok()).collect();
        let mut ptrs: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(null());
        unsafe { (api.command)(self.0, ptrs.as_ptr()) >= 0 }
    }

    pub fn get(&self, name: &str) -> Option<String> {
        let api = api()?;
        let name = CString::new(name).ok()?;
        unsafe {
            let p = (api.get_property_string)(self.0, name.as_ptr());
            if p.is_null() {
                return None;
            }
            let s = CStr::from_ptr(p).to_string_lossy().into_owned();
            (api.free)(p as *mut c_void);
            Some(s)
        }
    }

    /// Changes a property while playing (like "volume" or "aid").
    pub fn set(&self, name: &str, value: &str) -> bool {
        let Some(api) = api() else { return false };
        let (Ok(n), Ok(v)) = (CString::new(name), CString::new(value)) else { return false };
        unsafe { (api.set_property_string)(self.0, n.as_ptr(), v.as_ptr()) >= 0 }
    }

    /// The next event, waiting up to `timeout` seconds: (event id, end-file reason or 0).
    fn event(&self, timeout: f64) -> (i32, i32) {
        let Some(api) = api() else { return (EVENT_SHUTDOWN, 0) };
        unsafe {
            let e = &*(api.wait_event)(self.0, timeout);
            let reason = if e.event_id == EVENT_END_FILE && !e.data.is_null() { *(e.data as *const i32) } else { 0 };
            (e.event_id, reason)
        }
    }

    /// Takes the waiting events without blocking. Some(..) once the file ended or mpv stopped.
    pub fn ended(&self) -> Option<Ended> {
        loop {
            match self.event(0.0) {
                (EVENT_NONE, _) => return None,
                (EVENT_END_FILE, END_EOF) => return Some(Ended::Done),
                // The old file closing after a switch: the new one plays on.
                (EVENT_END_FILE, END_STOP) => {}
                (EVENT_END_FILE | EVENT_SHUTDOWN, _) => return Some(Ended::Failed),
                _ => {}
            }
        }
    }

    /// Waits until the file is open (true) or failed to open (false), up to `timeout` seconds.
    fn wait_loaded(&self, timeout: Duration) -> bool {
        let until = Instant::now() + timeout;
        while Instant::now() < until {
            match self.event(0.25) {
                (EVENT_FILE_LOADED, _) => return true,
                (EVENT_END_FILE | EVENT_SHUTDOWN, _) => return false,
                _ => {}
            }
        }
        false
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        if let Some(api) = api() {
            unsafe { (api.terminate_destroy)(self.0) };
        }
    }
}

/// Wallpaper sound, shared by all players: Wisp's background loop changes it, players follow it while playing.
pub struct Audio {
    pub on: std::sync::atomic::AtomicBool,
    /// 0 to 100.
    pub volume: std::sync::atomic::AtomicU32,
}

impl Audio {
    /// (on, volume) now.
    pub fn get(&self) -> (bool, u32) {
        use std::sync::atomic::Ordering::Relaxed;
        (self.on.load(Relaxed), self.volume.load(Relaxed))
    }
}

/// Plays `file` once with these options and waits until it ends: for jobs like making a copy or a thumbnail
/// (mpv writes a file when the "o" option is set). True when it played to the end.
pub fn run(options: &[(&str, &str)], file: &str) -> bool {
    let Some(mpv) = Mpv::new(options) else { return false };
    if !mpv.command(&["loadfile", file]) {
        return false;
    }
    loop {
        match mpv.event(1.0) {
            (EVENT_END_FILE, reason) => return reason == END_EOF,
            (EVENT_SHUTDOWN, _) => return false,
            _ => {}
        }
    }
}

/// A video's width, height and frame rate (0 when unknown), read without decoding it.
pub fn video_info(file: &str) -> Option<(i32, i32, f64)> {
    let mpv = Mpv::new(&[("vo", "null"), ("ao", "null"), ("pause", "yes")])?;
    if !mpv.command(&["loadfile", file]) || !mpv.wait_loaded(Duration::from_secs(10)) {
        return None;
    }
    let w = mpv.get("current-tracks/video/demux-w")?.parse().ok()?;
    let h = mpv.get("current-tracks/video/demux-h")?.parse().ok()?;
    let fps = mpv.get("container-fps").and_then(|f| f.parse().ok()).unwrap_or(0.0);
    Some((w, h, fps))
}
