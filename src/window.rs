//! The app window. Runs as its own `wisp.exe --window` process and only edits the config file;
//! the background process notices the change within a second and applies it.

use super::*;
use eframe::egui::{self, pos2, Color32, ColorImage, CornerRadius, Pos2, Rect, RichText, Sense, Stroke, TextureHandle, Vec2};
use std::{
    collections::HashMap,
    sync::mpsc::{channel, Receiver, Sender},
    sync::Arc,
    thread::JoinHandle,
    time::Duration,
};

// A deep, slightly cool dark theme.
const BG_TOP: Color32 = Color32::from_rgb(19, 25, 37);
const BG_BOTTOM: Color32 = Color32::from_rgb(11, 14, 20);
const SIDEBAR: Color32 = Color32::from_rgb(10, 13, 19);
const PANEL: Color32 = Color32::from_rgb(24, 29, 39);
const RAISED: Color32 = Color32::from_rgb(33, 39, 51);
const SELECTED: Color32 = Color32::from_rgb(56, 64, 80);
const TEXT: Color32 = Color32::from_rgb(240, 242, 246);
const MUTED: Color32 = Color32::from_rgb(134, 143, 158);
/// Secondary text: lighter than MUTED, for explanations people should read.
const GRAY: Color32 = Color32::from_rgb(154, 163, 178);
const SOFT: Color32 = Color32::from_rgb(200, 208, 220);
const FIELD: Color32 = Color32::from_rgb(17, 21, 29);
const LINE: Color32 = Color32::from_rgb(42, 49, 64);
// Status dots: playing, paused, downloading, broken.
const GREEN: Color32 = Color32::from_rgb(95, 211, 141);
const AMBER: Color32 = Color32::from_rgb(242, 184, 75);
const BLUE: Color32 = Color32::from_rgb(143, 180, 255);
const RED: Color32 = Color32::from_rgb(255, 107, 107);

const RECENT_SHOWN: usize = 10;

#[derive(PartialEq, Clone, Copy)]
enum Page {
    Home,
    Discover,
    Library,
    Settings,
}

#[derive(Clone, Copy)]
enum Icon {
    Search,
    Library,
    Gear,
    Screen,
    Link,
    Folder,
}

/// Small line icons, drawn so the app needs no icon font.
fn draw_icon(painter: &egui::Painter, icon: Icon, rect: Rect, color: Color32) {
    let s = Stroke::new(1.8_f32, color);
    let at = |x: f32, y: f32| pos2(rect.left() + x * rect.width(), rect.top() + y * rect.height());
    let r = rect.width();
    match icon {
        Icon::Search => {
            painter.circle_stroke(at(0.43, 0.43), r * 0.27, s);
            painter.line_segment([at(0.63, 0.63), at(0.86, 0.86)], s);
        }
        Icon::Library => {
            for (x, y) in [(0.14, 0.14), (0.54, 0.14), (0.14, 0.54), (0.54, 0.54)] {
                painter.rect_stroke(Rect::from_min_size(at(x, y), Vec2::splat(r * 0.32)), CornerRadius::same(2), s, egui::StrokeKind::Middle);
            }
        }
        Icon::Gear => {
            painter.circle_stroke(at(0.5, 0.5), r * 0.17, s);
            for k in 0..8 {
                let a = k as f32 * std::f32::consts::TAU / 8.0;
                let (dx, dy) = (a.cos(), a.sin());
                painter.line_segment([at(0.5 + dx * 0.27, 0.5 + dy * 0.27), at(0.5 + dx * 0.42, 0.5 + dy * 0.42)], Stroke::new(2.4_f32, color));
            }
        }
        Icon::Screen => {
            painter.rect_stroke(Rect::from_min_max(at(0.1, 0.18), at(0.9, 0.7)), CornerRadius::same(2), s, egui::StrokeKind::Middle);
            painter.line_segment([at(0.5, 0.7), at(0.5, 0.84)], s);
            painter.line_segment([at(0.32, 0.86), at(0.68, 0.86)], s);
        }
        Icon::Link => {
            painter.circle_stroke(at(0.5, 0.5), r * 0.38, s);
            painter.line_segment([at(0.5, 0.3), at(0.5, 0.7)], s);
            painter.line_segment([at(0.3, 0.5), at(0.7, 0.5)], s);
        }
        Icon::Folder => {
            let pts = vec![at(0.1, 0.24), at(0.4, 0.24), at(0.5, 0.36), at(0.9, 0.36), at(0.9, 0.8), at(0.1, 0.8), at(0.1, 0.24)];
            painter.add(egui::Shape::line(pts, s));
        }
    }
}

/// The 256 px image inside assets/wisp.ico, for the window icon and the sidebar logo.
fn logo() -> Option<image::RgbaImage> {
    let ico = include_bytes!("../assets/wisp.ico");
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    let entry = (0..count).map(|i| &ico[6 + i * 16..6 + i * 16 + 16]).find(|e| e[0] == 0)?; // width 0 means 256
    let len = u32::from_le_bytes(entry[8..12].try_into().ok()?) as usize;
    let at = u32::from_le_bytes(entry[12..16].try_into().ok()?) as usize;
    Some(image::load_from_memory(ico.get(at..at + len)?).ok()?.to_rgba8())
}

pub fn run() {
    // One window only: a second launch brings the open one to the front.
    unsafe {
        CreateMutexW(null(), 0, w("wisp-window").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let open = FindWindowW(null(), w(APP).as_ptr());
            if !open.is_null() {
                ShowWindow(open, SW_RESTORE);
                SetForegroundWindow(open);
            }
            return;
        }
    }
    let mut viewport = egui::ViewportBuilder::default().with_title(APP).with_inner_size([1240.0, 800.0]).with_min_inner_size([1000.0, 640.0]).with_drag_and_drop(true);
    if let Some(img) = logo() {
        viewport = viewport.with_icon(egui::IconData { width: img.width(), height: img.height(), rgba: img.into_raw() });
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let _ = eframe::run_native(
        APP,
        options,
        Box::new(|cc| {
            style(&cc.egui_ctx);
            Ok(Box::new(App::new(&cc.egui_ctx)))
        }),
    );
}

/// Segoe UI (Windows' own font) and a dark, flat look.
fn style(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (name, file) in [("segoe", "segoeui.ttf"), ("segoe-bold", "segoeuib.ttf")] {
        if let Ok(bytes) = fs::read(Path::new(r"C:\Windows\Fonts").join(file)) {
            fonts.font_data.insert(name.into(), Arc::new(egui::FontData::from_owned(bytes)));
        }
    }
    if fonts.font_data.contains_key("segoe") {
        fonts.families.get_mut(&egui::FontFamily::Proportional).unwrap().insert(0, "segoe".into());
    }
    // Fallbacks for Workshop titles in Chinese, Japanese and Korean, which Segoe UI can't draw (they showed as boxes).
    for (name, file) in [("yahei", "msyh.ttc"), ("malgun", "malgun.ttf")] {
        if let Ok(bytes) = fs::read(Path::new(r"C:\Windows\Fonts").join(file)) {
            fonts.font_data.insert(name.into(), Arc::new(egui::FontData::from_owned(bytes)));
            fonts.families.get_mut(&egui::FontFamily::Proportional).unwrap().push(name.into());
        }
    }
    let bold = if fonts.font_data.contains_key("segoe-bold") { "segoe-bold" } else { "segoe" };
    let mut family = vec![bold.to_string()];
    family.extend(fonts.families[&egui::FontFamily::Proportional].clone());
    fonts.families.insert(egui::FontFamily::Name("bold".into()), family);
    ctx.set_fonts(fonts);

    // "Animation effects" off in Windows: no animations here either.
    if !animations_on() {
        ctx.style_mut(|s| s.animation_time = 0.0);
    }
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG_BOTTOM;
    v.window_fill = PANEL;
    v.override_text_color = Some(TEXT);
    v.extreme_bg_color = Color32::from_rgb(17, 21, 29);
    v.selection.bg_fill = SELECTED;
    v.selection.stroke = Stroke::new(1.0_f32, TEXT);
    for (w, fill) in [(&mut v.widgets.inactive, RAISED), (&mut v.widgets.hovered, Color32::from_rgb(44, 51, 65)), (&mut v.widgets.active, SELECTED)] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.bg_stroke = Stroke::NONE;
        w.corner_radius = CornerRadius::same(9);
        w.fg_stroke = Stroke::new(1.0_f32, TEXT);
    }
    v.widgets.noninteractive.bg_stroke = Stroke::NONE;
    // Sliders fill their track up to the handle, so the value reads at a glance.
    v.slider_trailing_fill = true;
    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = Vec2::new(8.0, 8.0);
        s.spacing.button_padding = Vec2::new(14.0, 7.0);
        s.text_styles.insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
        s.text_styles.insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
    });
}

fn bold(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name("bold".into()))
}

fn file_name(path: &str) -> String {
    // Wallpaper Engine files have names like "scene.pkg" or "video_1.mp4": show the title from project.json instead.
    let project = Path::new(path).with_file_name("project.json");
    if let Some(title) = fs::read_to_string(project).ok().and_then(|j| json_str(&j, "title")) {
        return title;
    }
    let stem = Path::new(path).file_stem().map_or_else(|| path.to_string(), |s| s.to_string_lossy().into_owned());
    // Downloads are saved as "<8 hex digits>-<name>" so two links can't clash; show just the name.
    match stem.split_once('-') {
        Some((tag, rest)) if tag.len() == 8 && tag.chars().all(|c| c.is_ascii_hexdigit()) && !rest.is_empty() => rest.replace('-', " "),
        _ => stem,
    }
}

/// Where a wallpaper came from, for the gray line under its name.
fn source(path: &str) -> &'static str {
    let p = path.to_lowercase();
    if p.contains(r"workshop\content") {
        "Steam Workshop"
    } else if p.contains(r"wisp\downloads") {
        "Downloaded"
    } else {
        "Your files"
    }
}

fn dir_size(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            _ => e.metadata().map(|m| m.len()).unwrap_or(0),
        })
        .sum()
}

fn downloads_dir() -> PathBuf {
    PathBuf::from(env::var("LOCALAPPDATA").unwrap_or_default()).join(APP).join("downloads")
}

/// Makes a link found in a page absolute: "//host/x", "/x" and "x" are all relative to the page.
fn absolute(link: &str, page: &str) -> String {
    let link = link.replace("&amp;", "&");
    if link.starts_with("http://") || link.starts_with("https://") {
        return link;
    }
    let scheme_end = page.find("://").map_or(0, |i| i + 3);
    let origin_end = page[scheme_end..].find('/').map_or(page.len(), |i| scheme_end + i);
    if let Some(rest) = link.strip_prefix("//") {
        format!("{}{rest}", &page[..scheme_end])
    } else if link.starts_with('/') {
        format!("{}{link}", &page[..origin_end])
    } else {
        let base_end = page.rfind('/').filter(|&i| i >= origin_end).unwrap_or(origin_end);
        format!("{}/{link}", &page[..base_end])
    }
}

/// Every value of `attr=` in the page, quoted or not.
fn attr_values<'a>(html: &'a str, attr: &str) -> Vec<&'a str> {
    let pat = format!("{attr}=");
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find(&pat) {
        rest = &rest[i + pat.len()..];
        let value = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => rest[1..].split(q).next().unwrap_or(""),
            _ => rest.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or(""),
        };
        out.push(value);
    }
    out
}

fn has_video_ext(link: &str) -> bool {
    let path = link.split(['?', '#']).next().unwrap_or("").to_lowercase();
    [".mp4", ".webm", ".mov", ".mkv"].iter().any(|e| path.ends_with(e))
}

/// Video links in a wallpaper page, best first: (link, is it only a preview).
/// Full downloads first (direct video links, MotionBGs' 4K then HD buttons), then what the page's player shows.
/// 4K first because Wisp shrinks big videos to the screen anyway, and a tall screen crops a wide video a lot.
fn page_videos(html: &str, page: &str) -> Vec<(String, bool)> {
    let hrefs = attr_values(html, "href");
    let mut found: Vec<(String, bool)> = Vec::new();
    for kind in ["/dl/4k/", "/dl/hd/"] {
        found.extend(hrefs.iter().filter(|h| h.contains(kind)).map(|h| (absolute(h, page), false)));
    }
    found.extend(hrefs.iter().filter(|h| has_video_ext(h)).map(|h| (absolute(h, page), false)));
    // og:video names the page's main video: <meta property="og:video" content="...">.
    for tag in html.split('<').filter(|t| t.starts_with("meta") && t.contains("og:video")) {
        if let Some(c) = attr_values(tag, "content").first().filter(|c| !c.is_empty()) {
            found.push((absolute(c, page), true));
        }
    }
    for tag in html.split('<').filter(|t| t.starts_with("source") || t.starts_with("video")) {
        if let Some(src) = attr_values(tag, "src").first().filter(|s| !s.is_empty()) {
            found.push((absolute(src, page), true));
        }
    }
    let mut seen = std::collections::HashSet::new();
    found.retain(|(l, _)| seen.insert(l.clone()));
    found
}

/// One search result: its picture, its name, and the link that adds it (a Workshop item or a wallpaper page).
#[derive(Clone)]
struct Hit {
    title: String,
    thumb: String,
    link: String,
}

/// Where Wisp searches, all without an account or key.
const SEARCH: [&str; 4] = ["Wallpaper Engine", "Live Wallpaper", "MoeWalls", "MotionBGs"];

/// A filter: its name and choices as (label, value). The first choice is the default. For Steam, a value is a
/// Workshop tag the results must have, or with "-" in front one they must not have; "" filters nothing.
type Filter = (&'static str, &'static [(&'static str, &'static str)]);

/// Steam Workshop sort orders. The first is trending, or best match while searching.
const SORT: Filter = ("Sort", &[("Trending", ""), ("Most subscribed", "totaluniquesubscribers"), ("Top rated", "toprated"), ("Newest", "mostrecent")]);

/// Filters per source (index into SEARCH). Steam values are Workshop tags; site values are category pages, which
/// those sites only offer without a search.
fn filters(source: usize) -> &'static [Filter] {
    const WE: [Filter; 5] = [
        ("Type", &[("All types", ""), ("Scene", "Scene"), ("Video", "Video")]),
        (
            "Genre",
            &[
                ("All genres", ""),
                ("Anime", "Anime"),
                ("Game", "Game"),
                ("Nature", "Nature"),
                ("Landscape", "Landscape"),
                ("Abstract", "Abstract"),
                ("Fantasy", "Fantasy"),
                ("Sci-Fi", "Sci-Fi"),
                ("Cyberpunk", "Cyberpunk"),
                ("Girls", "Girls"),
                ("Relaxing", "Relaxing"),
                ("Pixel art", "Pixel art"),
                ("Retro", "Retro"),
                ("Vehicle", "Vehicle"),
                ("Animal", "Animal"),
                ("Music", "Music"),
            ],
        ),
        ("Size", &[("Any size", ""), ("1080p", "1920 x 1080"), ("1440p", "2560 x 1440"), ("4K", "3840 x 2160")]),
        // Steam's unfiltered list includes Mature items (about a third of what's trending), so they're hidden unless picked.
        ("Age", &[("No mature", "-Mature"), ("Everyone", "Everyone"), ("Questionable", "Questionable"), ("Mature", "Mature"), ("Any age", "")]),
        SORT,
    ];
    const LIVE: [Filter; 1] = [SORT];
    const MOE: [Filter; 1] = [(
        "Category",
        &[
            ("All", ""),
            ("Anime", "anime"),
            ("Games", "games"),
            ("Movies", "movies"),
            ("Fantasy", "fantasy"),
            ("Sci-Fi", "sci-fi"),
            ("Landscape", "landscape"),
            ("Abstract", "abstract"),
            ("Pixel art", "pixel-art"),
            ("Vehicle", "vehicle"),
            ("Animal", "animal"),
            ("Lifestyle", "lifestyle"),
        ],
    )];
    const MOTION: [Filter; 1] = [("Category", &[("All", ""), ("Anime", "anime"), ("Games", "games"), ("Anime nature", "anime-nature"), ("Cyberpunk", "cyberpunk"), ("Dark", "dark"), ("Car", "car"), ("Animal", "animal")])];
    match source {
        0 => &WE,
        1 => &LIVE,
        2 => &MOE,
        _ => &MOTION,
    }
}

/// The page to read for a search: `picks` holds the chosen choice of each of the source's filters; `no_anime` is the
/// Wallpaper Engine switch that hides anime; `page` counts from 1 ("Load more" asks for the next).
fn search_url(source: usize, query: &str, picks: &[usize], no_anime: bool, page: u32) -> String {
    let enc = |s: &str| -> String { s.trim().bytes().map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") }).collect() };
    let pick = |i: usize| filters(source).get(i).and_then(|f| f.1.get(picks.get(i).copied().unwrap_or(0))).map_or("", |c| c.1);
    let q = enc(query);
    // Each site writes "page 2" its own way; page 1 is the plain address.
    let (more, n) = (page > 1, page);
    match source {
        0 | 1 => {
            let app = if source == 0 { 431960 } else { 4056970 };
            let sort_at = filters(source).len() - 1;
            let sort = match pick(sort_at) {
                "" if q.is_empty() => "trend",
                "" => "textsearch",
                s => s,
            };
            let mut url = format!("https://steamcommunity.com/workshop/browse/?appid={app}&browsesort={sort}&searchtext={q}");
            if source == 0 {
                // Steam's Web wallpapers are left out: Wisp can't play them.
                url += "&excludedtags[]=Web";
                for i in 0..sort_at {
                    match pick(i) {
                        "" => {}
                        tag if tag.starts_with('-') => url += &format!("&excludedtags[]={}", enc(&tag[1..])),
                        tag => url += &format!("&requiredtags[]={}", enc(tag)),
                    }
                }
                if no_anime {
                    url += "&excludedtags[]=Anime";
                }
            }
            if more {
                url += &format!("&p={n}");
            }
            url
        }
        2 => {
            let page = if more { format!("page/{n}/") } else { String::new() };
            match (q.is_empty(), pick(0)) {
                (false, _) => format!("https://moewalls.com/{page}?s={q}"),
                (true, "") => format!("https://moewalls.com/{page}"),
                (true, cat) => format!("https://moewalls.com/category/{cat}/{page}"),
            }
        }
        _ => {
            let page = if more { format!("{n}/") } else { String::new() };
            match (q.is_empty(), pick(0)) {
                (false, _) if more => format!("https://motionbgs.com/search?q={q}&page={n}"),
                (false, _) => format!("https://motionbgs.com/search?q={q}"),
                (true, "") => format!("https://motionbgs.com/{page}"),
                (true, tag) => format!("https://motionbgs.com/tag:{tag}/{page}"),
            }
        }
    }
}

/// The few HTML escapes that show up in titles.
fn unescape(s: &str) -> String {
    s.replace("&amp;", "&").replace("&quot;", "\"").replace("&#39;", "'").replace("&#039;", "'").replace("&#8217;", "’").replace("&lt;", "<").replace("&gt;", ">")
}

/// The first `max` bytes of `s`, or a little fewer so a character (Chinese and Japanese take 3 bytes) isn't cut
/// in half, which would crash.
fn head(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The text between `start` and the next `end` in `s`.
fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let rest = &s[s.find(start)? + start.len()..];
    Some(&rest[..rest.find(end)?])
}

/// Steam Workshop results: each is a link to the item with its preview picture, whose alt text is the title.
fn steam_hits(html: &str) -> Vec<Hit> {
    let mut seen = std::collections::HashSet::new();
    html.split("sharedfiles/filedetails/?id=")
        .skip(1)
        .filter_map(|part| {
            let id: String = part.chars().take_while(char::is_ascii_digit).collect();
            let near = head(part, 800);
            let thumb = between(near, "<img src=\"", "\"")?;
            let title = between(near, "alt=\"", "\"").unwrap_or("");
            // Steam pads previews into black squares; unpadded they keep their own shape.
            let thumb = unescape(thumb).replace("letterbox=true", "letterbox=false");
            (!id.is_empty() && seen.insert(id.clone())).then(|| Hit { title: unescape(title), thumb, link: format!("https://steamcommunity.com/sharedfiles/filedetails/?id={id}") })
        })
        .collect()
}

/// MoeWalls results: one <article> each, with the title, the wallpaper page and its thumbnail.
fn moewalls_hits(html: &str) -> Vec<Hit> {
    html.split("<article")
        .skip(1)
        .filter_map(|a| Some(Hit { title: unescape(between(a, "title=\"", "\"")?), link: between(a, "href=\"", "\"")?.to_string(), thumb: between(a, "src=\"", "\"")?.to_string() }))
        .collect()
}

/// MotionBGs results: a link whose title ends in "live wallpaper", followed by its pictures (taking the JPEG).
fn motionbgs_hits(html: &str) -> Vec<Hit> {
    html.split("<a title=\"")
        .skip(1)
        .filter_map(|a| {
            let title = a[..a.find('"')?].strip_suffix(" live wallpaper")?;
            let slug = attr_values(a, "href").first().copied()?;
            let thumb = attr_values(head(a, 1500), "srcset").into_iter().find(|s| s.ends_with(".jpg"))?;
            Some(Hit { title: unescape(title), link: absolute(slug, "https://motionbgs.com/"), thumb: absolute(thumb, "https://motionbgs.com/") })
        })
        .collect()
}

/// Searches one source (an index into SEARCH) with its filters; an empty query lists its popular or newest.
fn search(source: usize, query: &str, picks: &[usize], no_anime: bool, page: u32) -> Result<Vec<Hit>, String> {
    let url = search_url(source, query, picks, no_anime, page);
    let html = Command::new("curl").args(["-L", "-s", "--max-time", "20", "-A", "Mozilla/5.0"]).arg(&url).creation_flags(CREATE_NO_WINDOW).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    if html.is_empty() {
        return Err("Couldn't reach the site. Check your internet connection.".into());
    }
    Ok(match source {
        0 | 1 => steam_hits(&html),
        2 => moewalls_hits(&html),
        _ => motionbgs_hits(&html),
    })
}

/// Downloads one link to `file` with curl. The page is sent as the referrer, which some video hosts want.
fn fetch(url: &str, referer: &str, file: &Path) -> bool {
    Command::new("curl")
        // 2 GB is far above any real wallpaper; it stops a wrong link from filling the disk.
        .args(["-L", "-f", "-s", "--max-time", "600", "--max-filesize", "2G", "-A", "Mozilla/5.0", "-e", referer, "-o"])
        .arg(file)
        .arg(url)
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .is_ok_and(|s| s.success())
}

fn download_path(url: &str) -> PathBuf {
    let last = url.split(['?', '#']).next().unwrap_or("").trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let name: String = last.chars().filter(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c)).take(80).collect();
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    downloads_dir().join(format!("{:08x}-{}", hasher.finish() as u32, if name.is_empty() { "wallpaper" } else { &name }))
}

/// Downloads a video or GIF link, or the video of a wallpaper page, into Wisp's downloads folder.
/// Returns the file and, when only a preview could be found, a note saying so. Runs on its own thread.
fn download(url: String) -> Result<(String, Option<String>), String> {
    let _ = fs::create_dir_all(downloads_dir());
    let first = download_path(&url);
    if !fetch(&url, &url, &first) {
        let _ = fs::remove_file(&first);
        return Err("The download failed. Check that the link works in your browser.".into());
    }
    if video_size(&first.display().to_string()).is_some() {
        return Ok((first.display().to_string(), None));
    }
    // Not a video: read it as a web page and try the videos it links to.
    let html = String::from_utf8_lossy(&fs::read(&first).unwrap_or_default()).into_owned();
    let _ = fs::remove_file(&first);
    for (link, preview) in page_videos(&html, &url).into_iter().take(4) {
        let file = download_path(&link);
        if fetch(&link, &url, &file) && video_size(&file.display().to_string()).is_some() {
            let note = preview.then(|| {
                "Added the preview video from that page, which can be lower quality than the site's own Download button. \
                 For full quality, download it there and add it with Choose file."
                    .to_string()
            });
            // Name it after the page, not the CDN's file name.
            let named = download_path(&url).with_extension(file.extension().unwrap_or_default());
            let path = if fs::rename(&file, &named).is_ok() { named } else { file };
            return Ok((path.display().to_string(), note));
        }
        let _ = fs::remove_file(&file);
    }
    Err("Couldn't find a video on that page.\n\nOn the site, right-click the video (or its Download button) and copy that link instead.".into())
}

/// For an animated GIF, a frame about a second in (the 20th, or the last if shorter): many fade in from black.
/// None for other pictures.
fn gif_frame(bytes: &[u8]) -> Option<image::RgbaImage> {
    use image::AnimationDecoder;
    if image::guess_format(bytes).ok()? != image::ImageFormat::Gif {
        return None;
    }
    let frames = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)).ok()?.into_frames();
    frames.take(20).filter_map(Result::ok).last().map(|f| f.into_buffer())
}

/// A small still of a video, cached on disk next to the screen-sized copies. Web links (search results'
/// pictures) are downloaded and shrunk.
fn make_thumb(path: &str) -> Option<ColorImage> {
    if path.starts_with("https://") {
        let out = Command::new("curl").args(["-L", "-s", "--max-time", "20", "-A", "Mozilla/5.0"]).arg(path).creation_flags(CREATE_NO_WINDOW).output().ok()?;
        let img = gif_frame(&out.stdout).map(image::DynamicImage::ImageRgba8).or_else(|| image::load_from_memory(&out.stdout).ok())?;
        let img = img.thumbnail(480, 480).to_rgba8();
        return Some(ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw()));
    }
    // Wallpaper Engine items come with a preview picture: quicker than asking mpv, and mpv can't read scene.pkg.
    // Shrunk, so a Library full of them stays light.
    if let Some(img) = ["preview.jpg", "preview.png", "preview.gif"].iter().find_map(|n| image::open(Path::new(path).with_file_name(n)).ok()) {
        let img = img.thumbnail(480, 480).to_rgba8();
        return Some(ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw()));
    }
    if is_scene(path) {
        return None;
    }
    let meta = fs::metadata(path).ok()?;
    let mut hasher = DefaultHasher::new();
    (path, meta.len(), meta.modified().ok()).hash(&mut hasher);
    let dir = cache_dir().join("thumbs");
    let out = dir.join(format!("{:016x}.png", hasher.finish()));
    if !out.exists() {
        let _ = fs::create_dir_all(&dir);
        let o = out.display().to_string();
        mpv::run(&[("audio", "no"), ("start", "25%"), ("frames", "1"), ("vf", "scale=480:-2"), ("ovc", "png"), ("o", &o)], path);
    }
    let img = image::open(&out).ok()?.to_rgba8();
    Some(ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw()))
}

/// What adding a link came to.
enum Added {
    /// A file to play, and maybe a note (like "this is only a preview").
    File(String, Option<String>),
    /// A Workshop item to play once Steam has downloaded it.
    Wait(String),
    /// Nothing, with the reason to show (None when the user was already told).
    Failed(Option<String>),
}

enum Thumb {
    Loading,
    Ready(TextureHandle),
    Missing,
}

fn toggle(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(42.0, 24.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool(resp.id, *on);
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(12), if *on { TEXT } else { SELECTED });
    let x = egui::lerp(rect.left() + 12.0..=rect.right() - 12.0, t);
    p.circle_filled(pos2(x, rect.center().y), 8.5, if *on { BG_BOTTOM } else { TEXT });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A row of options in a pill, like "Native | 15 | 24 | 30". Returns the newly picked one.
fn segmented(ui: &mut egui::Ui, labels: &[&str], selected: usize) -> Option<usize> {
    let mut picked = None;
    // In a right-aligned row widgets are placed right to left, so add the options in reverse to read left to right.
    let rtl = ui.layout().main_dir() == egui::Direction::RightToLeft;
    egui::Frame::new().fill(Color32::from_rgb(17, 21, 29)).corner_radius(CornerRadius::same(10)).inner_margin(3.0).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.spacing_mut().button_padding = Vec2::new(12.0, 5.0);
            let mut order: Vec<usize> = (0..labels.len()).collect();
            if rtl {
                order.reverse();
            }
            for k in order {
                let label = labels[k];
                let on = k == selected;
                let text = RichText::new(label).size(13.0).color(if on { TEXT } else { MUTED });
                let b = egui::Button::new(text).fill(if on { SELECTED } else { Color32::TRANSPARENT }).corner_radius(CornerRadius::same(8));
                if ui.add(b).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !on {
                    picked = Some(k);
                }
            }
        });
    });
    picked
}

/// A settings group: a rounded panel of rows, each with a title, a gray description and a control on the right.
fn group(ui: &mut egui::Ui, title: &str, rows: impl FnOnce(&mut egui::Ui)) {
    ui.label(RichText::new(title).font(bold(15.0)));
    egui::Frame::new().fill(PANEL).corner_radius(CornerRadius::same(12)).inner_margin(egui::Margin::symmetric(16, 12)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 14.0;
        rows(ui);
    });
    ui.add_space(14.0);
}

fn row(ui: &mut egui::Ui, title: &str, desc: &str, control: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(RichText::new(title).font(bold(14.0)));
            ui.label(RichText::new(desc).size(12.5).color(MUTED));
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control);
    });
}

/// A scene setting's value as on/off: Wallpaper Engine stores bools, but also 0/1 and "true".
fn is_on(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::String(s) => s == "true" || s == "1",
        v => v.as_f64().is_some_and(|x| x != 0.0),
    }
}

fn number(v: &serde_json::Value) -> f64 {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())).unwrap_or(0.0)
}

fn soft_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).size(13.5)).corner_radius(CornerRadius::same(9))).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The one main action in a place: white, with dark text.
fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let b = egui::Button::new(RichText::new(text).font(bold(13.5)).color(BG_BOTTOM)).fill(TEXT).corner_radius(CornerRadius::same(9)).min_size(Vec2::new(0.0, 34.0));
    ui.add(b).on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn ghost_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let b = egui::Button::new(RichText::new(text).size(13.5).color(SOFT)).fill(Color32::TRANSPARENT).corner_radius(CornerRadius::same(9));
    ui.add(b).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A pill that is on or off, like the Library's "All | Scenes | Videos".
fn chip(ui: &mut egui::Ui, text: &str, on: bool) -> egui::Response {
    let rt = if on { RichText::new(text).size(13.0).font(bold(13.0)).color(BG_BOTTOM) } else { RichText::new(text).size(13.0).color(SOFT) };
    let b = egui::Button::new(rt).fill(if on { TEXT } else { Color32::TRANSPARENT }).stroke(Stroke::new(1.5_f32, if on { TEXT } else { LINE })).corner_radius(CornerRadius::same(16));
    ui.add(b).on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn title(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).font(bold(24.0)));
}

fn sub(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(13.5).color(GRAY)).wrap());
    ui.add_space(14.0);
}

fn small(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).size(12.5).color(GRAY)
}

fn divider(ui: &mut egui::Ui) {
    ui.add_space(10.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::ZERO, Color32::from_rgb(37, 44, 58));
    ui.add_space(10.0);
}

/// How far below a text's top the middle of its capital letters is: where an icon or dot beside it is centered,
/// so they line up with the letters and not with the text box (which has room above and below the letters).
fn cap_middle(g: &egui::Galley, size: f32) -> f32 {
    let baseline = g.rows.first().and_then(|r| Some(r.pos.y + r.row.glyphs.first()?.pos.y));
    // Segoe UI's capitals are 0.7 of the font size tall.
    baseline.map_or(g.size().y / 2.0, |b| b - 0.35 * size)
}

/// An animation's length: as given, or none when the person turned animations off in Windows (see style).
fn anim(ctx: &egui::Context, seconds: f32) -> f32 {
    if ctx.style().animation_time > 0.0 { seconds } else { 0.0 }
}

/// A status dot: its color fades when the status changes, and while downloading it's a small spinner.
fn status_dot(ui: &mut egui::Ui, id: egui::Id, center: Pos2, color: Color32) {
    if color == BLUE && anim(ui.ctx(), 1.0) > 0.0 {
        egui::Spinner::new().size(10.0).color(BLUE).paint_at(ui, Rect::from_center_size(center, Vec2::splat(10.0)));
        return;
    }
    let ctx = ui.ctx().clone();
    let t = anim(&ctx, 0.3);
    let ch = |k: usize, v: u8| ctx.animate_value_with_time(id.with(k), v as f32, t).round() as u8;
    ui.painter().circle_filled(center, 4.0, Color32::from_rgb(ch(0, color.r()), ch(1, color.g()), ch(2, color.b())));
}

/// A colored status dot and its text (wrapping if long), the dot centered on the first line's capitals.
fn status_line(ui: &mut egui::Ui, text: &str, color: Color32, size: f32) {
    let width = ui.available_width();
    let galley = ui.painter().layout(text.to_string(), egui::FontId::proportional(size), SOFT, width - 14.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, galley.size().y), Sense::hover());
    let id = ui.id().with("status-dot");
    status_dot(ui, id, pos2(rect.left() + 4.0, rect.top() + cap_middle(&galley, size)), color);
    ui.painter().galley(pos2(rect.left() + 14.0, rect.top()), galley, SOFT);
}

/// A small label on a picture's top-left corner, like "Scene".
fn badge_on(ui: &egui::Ui, rect: Rect, text: &str) {
    let galley = ui.painter().layout_no_wrap(text.into(), bold(11.0), TEXT);
    let r = Rect::from_min_size(rect.min + Vec2::new(8.0, 8.0), galley.size() + Vec2::new(12.0, 6.0));
    ui.painter().rect_filled(r, CornerRadius::same(6), Color32::from_black_alpha(185));
    ui.painter().galley(r.min + Vec2::new(6.0, 3.0), galley, TEXT);
}

/// A big clickable card with an icon, a bold title and a gray explanation: the welcome and sheet choices.
fn choice(ui: &mut egui::Ui, icon: Icon, name: &str, desc: &str, width: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 112.0), Sense::click());
    let lit = resp.hovered() || resp.has_focus();
    let p = ui.painter();
    p.rect(rect, CornerRadius::same(14), if lit { Color32::from_rgb(22, 27, 37) } else { FIELD }, Stroke::new(1.5_f32, if lit { MUTED } else { LINE }), egui::StrokeKind::Inside);
    draw_icon(p, icon, Rect::from_min_size(rect.min + Vec2::new(16.0, 16.0), Vec2::splat(18.0)), TEXT);
    p.text(rect.min + Vec2::new(44.0, 25.0), egui::Align2::LEFT_CENTER, name, bold(15.0), TEXT);
    let galley = p.layout(desc.into(), egui::FontId::proportional(12.5), GRAY, width - 32.0);
    p.galley(rect.min + Vec2::new(16.0, 46.0), galley, GRAY);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A speaker: sound waves when on, a cross when off.
fn paint_speaker(p: &egui::Painter, c: Pos2, on: bool, color: Color32) {
    p.rect_filled(Rect::from_center_size(c - Vec2::new(4.0, 0.0), Vec2::new(4.0, 6.0)), CornerRadius::same(1), color);
    let cone = vec![c + Vec2::new(-2.0, -3.0), c + Vec2::new(4.0, -7.0), c + Vec2::new(4.0, 7.0), c + Vec2::new(-2.0, 3.0)];
    p.add(egui::Shape::convex_polygon(cone, color, Stroke::NONE));
    let s = Stroke::new(1.6_f32, color);
    if on {
        for r in [6.0_f32, 10.0] {
            let pts: Vec<Pos2> = (0..=8).map(|k| (k as f32 / 8.0 - 0.5) * 1.6).map(|a| c + Vec2::new(4.0 + r * a.cos(), r * a.sin())).collect();
            p.add(egui::Shape::line(pts, s));
        }
    } else {
        let x = c + Vec2::new(11.0, 0.0);
        p.line_segment([x + Vec2::new(-3.5, -3.5), x + Vec2::new(3.5, 3.5)], s);
        p.line_segment([x + Vec2::new(-3.5, 3.5), x + Vec2::new(3.5, -3.5)], s);
    }
}

fn paint_pause(p: &egui::Painter, c: Pos2, paused: bool, color: Color32) {
    if paused {
        let pts = vec![c + Vec2::new(-4.0, -6.0), c + Vec2::new(6.0, 0.0), c + Vec2::new(-4.0, 6.0)];
        p.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
    } else {
        for dx in [-3.0, 3.0] {
            p.rect_filled(Rect::from_center_size(c + Vec2::new(dx, 0.0), Vec2::new(3.5, 12.0)), CornerRadius::same(1), color);
        }
    }
}

/// A button `width` wide with a drawn icon and a label, centered together.
/// `ink`: how far the drawn icon reaches left and right of the point it's drawn at, so the gap to the label is the
/// same for every icon.
fn icon_button(ui: &mut egui::Ui, width: f32, label: &str, ink: (f32, f32), icon: impl FnOnce(&egui::Painter, Pos2, Color32)) -> egui::Response {
    const GAP: f32 = 8.0;
    let galley = ui.painter().layout_no_wrap(label.into(), egui::FontId::proportional(13.0), SOFT);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 34.0), Sense::click());
    let lit = resp.hovered() || resp.has_focus();
    ui.painter().rect_filled(rect, CornerRadius::same(9), if lit { Color32::from_rgb(33, 40, 53) } else { Color32::from_rgb(24, 29, 39) });
    let color = if lit { TEXT } else { SOFT };
    // Icon and label centered together across; icon and capitals both centered on the button's middle line.
    let start = (rect.center().x - (ink.0 + ink.1 + GAP + galley.size().x) / 2.0).round();
    let mid = rect.center().y.round();
    icon(ui.painter(), pos2(start + ink.0, mid), color);
    let top = mid - cap_middle(&galley, 13.0);
    ui.painter().galley(pos2(start + ink.0 + ink.1 + GAP, top), galley, color);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Three dots, the "more actions" button on Library tiles.
fn more_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(30.0, 24.0), Sense::click());
    if resp.hovered() || resp.has_focus() {
        ui.painter().rect_filled(rect, CornerRadius::same(7), RAISED);
    }
    for dx in [-6.0, 0.0, 6.0] {
        ui.painter().circle_filled(rect.center() + Vec2::new(dx, 0.0), 1.8, SOFT);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn key_cap(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new().fill(FIELD).stroke(Stroke::new(1.0_f32, Color32::from_rgb(47, 55, 71))).corner_radius(CornerRadius::same(6)).inner_margin(egui::Margin::symmetric(7, 2)).show(ui, |ui| {
        ui.label(RichText::new(text).size(12.0));
    });
}

/// A link being added, for the screen it goes on: a Workshop item or a download, on its own thread.
struct Adding {
    screen: usize,
    link: String,
    /// Put it across all screens once it's ready.
    span: bool,
    job: JoinHandle<Added>,
}

/// A wallpaper just put on a screen: for a few seconds it can be taken back.
struct Trial {
    screen: usize,
    /// What the screen had before: file, Workshop wait, span, and the library's order.
    prev: (String, String, bool, Vec<String>),
    path: String,
    until: Instant,
}

struct Toast {
    text: String,
    until: Instant,
    /// Shows Undo, which takes back the pending delete.
    undo: bool,
}

struct App {
    cfg: Config,
    /// The config text last read or written here, to notice the background process's changes.
    seen: String,
    checked: Instant,
    screens: Vec<(RECT, RECT)>,
    selected: usize,
    page: Page,
    link: String,
    adding: Option<Adding>,
    /// A search result whose download failed, so its tile can offer to try again.
    failed_link: Option<String>,
    /// The frame rate slider's value while it is dragged; saved when let go.
    fps_drag: u32,
    autostart: bool,
    /// (cache bytes, downloads bytes), measured when Settings opens.
    storage: Option<(u64, u64)>,
    logo: Option<TextureHandle>,
    thumbs: HashMap<String, Thumb>,
    /// One worker makes thumbnails in order, so opening the window doesn't start many mpv processes at once.
    want_thumb: Sender<String>,
    got_thumb: Receiver<(String, Option<ColorImage>)>,
    /// The selected scene's settings (clock on/off, colors...), read once per wallpaper.
    scene_props: Option<(String, Vec<scene::UserProp>)>,
    /// When to save a change that's still moving (a drag, a slider); see save_soon.
    setting_changed: Option<Instant>,
    /// Wallpaper Engine wallpapers found on this PC, and when.
    installed: Option<(Instant, Vec<String>)>,
    scanning: Option<JoinHandle<Vec<String>>>,
    /// Wallpaper Engine's folder, when it's installed.
    we_dir: Option<PathBuf>,
    /// What the background process reports per screen (see status_path), its memory in MB, and whether it runs.
    status: Vec<String>,
    ram: u64,
    /// Its CPU and GPU use in percent, reported while this window is open.
    cpu: Option<f32>,
    gpu: Option<f32>,
    running: bool,
    /// Wallpaper search: the source picked (an index into SEARCH), what's typed, the results, the search running.
    source: usize,
    /// Per source, the choice picked in each of its filters (see `filters`).
    picks: [[usize; 6]; 4],
    /// The Wallpaper Engine "Hide anime" switch.
    no_anime: bool,
    query: String,
    hits: Option<Result<Vec<Hit>, String>>,
    /// The search running, and the page it asks for (above 1: "Load more", whose results are added at the end).
    searching: Option<(u32, JoinHandle<Result<Vec<Hit>, String>>)>,
    /// The last page shown, and whether the site has more after it.
    result_page: u32,
    more_pages: bool,
    /// The search the results on screen came from (source, words, filters, No anime), which "Load more" continues.
    shown_search: (usize, String, [usize; 6], bool),
    focus_search: bool,
    /// The Change wallpaper sheet, and whether its link box is open.
    sheet: bool,
    sheet_link: bool,
    /// The search result shown big.
    detail: Option<Hit>,
    /// A wallpaper deleted from the Library, done for real after a few seconds unless Undo is clicked, and the
    /// screens it was taken off (Undo puts it back there).
    pending_delete: Option<(String, Instant, Vec<usize>)>,
    trial: Option<Trial>,
    /// What the Keep bar and the message last showed, so they can animate away after they end.
    trial_seen: Option<(usize, String, u64)>,
    toast_seen: Option<(String, bool)>,
    toast: Option<Toast>,
    lib_filter: usize,
    more_options: bool,
    banner_closed: bool,
    /// Explaining, once, that closing the window keeps Wisp running.
    close_note: bool,
    /// Pictures' shapes (width / height), and the videos being measured (see picture_aspect).
    aspects: HashMap<String, f32>,
    aspect_asked: std::collections::HashSet<String>,
    aspect_done: Sender<(String, f32)>,
    got_aspect: Receiver<(String, f32)>,
    /// The fit mode under the pointer last frame, previewed before it's chosen.
    hover_fit: Option<usize>,
}

impl App {
    fn new(ctx: &egui::Context) -> App {
        let screens = monitors();
        let mut cfg = Config::load();
        cfg.fit_to(screens.len());
        let seen = Config::read_disk().unwrap_or_default();
        let fps_drag = if (10..=60).contains(&cfg.fps) { cfg.fps } else { 30 };
        let (want_thumb, jobs) = channel::<String>();
        let (done, got_thumb) = channel();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            for path in jobs {
                // Search results' pictures download side by side (only on-screen tiles ask, so a dozen or so at once).
                if path.starts_with("https://") {
                    let (done, repaint) = (done.clone(), repaint.clone());
                    std::thread::spawn(move || {
                        let img = make_thumb(&path);
                        let _ = done.send((path, img));
                        repaint.request_repaint();
                    });
                    continue;
                }
                let img = make_thumb(&path);
                if done.send((path, img)).is_err() {
                    break;
                }
                repaint.request_repaint();
            }
        });
        let logo = logo().map(|img| {
            let img = image::imageops::resize(&img, 72, 72, image::imageops::FilterType::Lanczos3);
            ctx.load_texture("logo", ColorImage::from_rgba_unmultiplied([72, 72], img.as_raw()), Default::default())
        });
        // steam_api64.dll sits in wallpaper_engine\bin.
        let we_dir = steam_api().and_then(|p| p.parent()?.parent().map(Path::to_path_buf));
        let (aspect_done, got_aspect) = channel();
        let mut app = App {
            aspects: HashMap::new(),
            aspect_asked: Default::default(),
            aspect_done,
            got_aspect,
            hover_fit: None,
            cfg,
            seen,
            checked: Instant::now(),
            screens,
            selected: 0,
            page: Page::Home,
            link: String::new(),
            adding: None,
            failed_link: None,
            fps_drag,
            autostart: reg(&["query", RUN_KEY, "/v", APP]),
            storage: None,
            logo,
            thumbs: HashMap::new(),
            want_thumb,
            got_thumb,
            scene_props: None,
            setting_changed: None,
            installed: None,
            scanning: None,
            // Without Wallpaper Engine, Discover opens on a site that works.
            source: if we_dir.is_some() { 0 } else { 2 },
            we_dir,
            status: Vec::new(),
            ram: 0,
            cpu: None,
            gpu: None,
            running: true,
            picks: [[0; 6]; 4],
            no_anime: false,
            query: String::new(),
            hits: None,
            searching: None,
            result_page: 1,
            more_pages: true,
            shown_search: (0, String::new(), [0; 6], false),
            focus_search: false,
            sheet: false,
            sheet_link: false,
            detail: None,
            pending_delete: None,
            trial: None,
            trial_seen: None,
            toast_seen: None,
            toast: None,
            lib_filter: 0,
            more_options: false,
            banner_closed: false,
            close_note: false,
        };
        app.read_status();
        app.go_to_asked_page();
        app
    }

    /// Saves in `ms` milliseconds unless more changes come first: moves and zoom apply live (40), scene options
    /// restart the scene, so they wait until the slider rests (400).
    fn save_soon(&mut self, ms: u64) {
        self.setting_changed = Some(Instant::now() + Duration::from_millis(ms));
    }

    fn commit(&mut self) {
        self.seen = self.cfg.save();
        wake_background();
    }

    /// The screen the buttons act on: Screen 1 while one wallpaper spans all screens.
    fn chosen(&self) -> usize {
        if self.cfg.span {
            0
        } else {
            self.selected.min(self.screens.len().saturating_sub(1))
        }
    }

    fn where_name(&self, i: usize) -> String {
        if self.screens.len() <= 1 {
            "your screen".into()
        } else if self.cfg.span && i == 0 {
            "both screens".into()
        } else {
            format!("Screen {}", i + 1)
        }
    }

    /// Puts a wallpaper on screen i and offers, for a few seconds, to go back.
    fn use_on(&mut self, i: usize, path: String, span: bool) {
        let prev = (self.cfg.files[i].clone(), self.cfg.waits[i].clone(), self.cfg.span, self.cfg.recent.clone());
        if span {
            self.cfg.span = true;
        }
        self.cfg.set_file(i, path.clone());
        self.commit();
        self.trial = Some(Trial { screen: i, prev, path, until: Instant::now() + Duration::from_secs(15) });
        self.sheet = false;
        self.detail = None;
    }

    fn show_toast(&mut self, text: String, undo: bool) {
        // Longer messages stay longer.
        let secs = if undo { 7 } else { 4 + text.len() as u64 / 25 };
        self.toast = Some(Toast { text, until: Instant::now() + Duration::from_secs(secs), undo });
    }

    /// Reads what the background process wrote (see status_path). Not updated for a few seconds means it isn't running.
    fn read_status(&mut self) {
        let p = status_path();
        self.running = fs::metadata(&p).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|d| d.as_secs() < 6);
        self.status.clear();
        (self.cpu, self.gpu) = (None, None);
        for line in fs::read_to_string(&p).unwrap_or_default().lines() {
            match line.split_once('=') {
                Some(("ram", v)) => self.ram = v.parse().unwrap_or(0),
                Some(("cpu", v)) => self.cpu = v.parse().ok(),
                Some(("gpu", v)) => self.gpu = v.parse().ok(),
                Some((k, v)) if k.starts_with("screen") => {
                    if let Ok(i) = k[6..].parse::<usize>() {
                        if i < 16 {
                            if self.status.len() <= i {
                                self.status.resize(i + 1, String::new());
                            }
                            self.status[i] = v.to_string();
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// The tray menu's "Settings" and "Change wallpaper…" leave a note saying where to go.
    fn go_to_asked_page(&mut self) {
        let Ok(page) = fs::read_to_string(goto_path()) else { return };
        let _ = fs::remove_file(goto_path());
        match page.trim() {
            "settings" => self.page = Page::Settings,
            "change" => {
                self.page = Page::Home;
                self.sheet = true;
                self.sheet_link = false;
            }
            _ => {}
        }
    }

    /// What a screen is doing: a short form for under the screen, the full sentence, and its dot's color.
    fn screen_state(&self, i: usize) -> (&'static str, &'static str, Color32) {
        if self.adding.as_ref().is_some_and(|a| a.screen == i) {
            return ("Downloading…", "Downloading. It starts by itself when done.", BLUE);
        }
        if !self.cfg.waits[i].is_empty() {
            return ("Steam downloading…", "Steam is downloading it. It starts by itself when done.", BLUE);
        }
        if self.cfg.files[i].is_empty() {
            return ("No wallpaper", "No wallpaper yet. Choose one and it starts right away.", MUTED);
        }
        if !self.running {
            return ("Not running", "Wisp isn't running, so nothing plays.", MUTED);
        }
        // Spanning plays one wallpaper, reported as screen 0.
        let k = if self.cfg.span { 0 } else { i };
        match self.status.get(k).map(String::as_str).unwrap_or("starting") {
            "playing" => ("Playing", "Playing", GREEN),
            "covered" => ("Paused · window covers it", "Paused while a window covers this screen. It starts again by itself.", AMBER),
            "game" => ("Paused · game open", "Paused while a game or full-screen app is open. It starts again by itself.", AMBER),
            "battery" => ("Paused · on battery", "Paused on battery. It starts again when you plug in.", AMBER),
            "paused" => ("Paused by you", "Paused by you. Click Resume all at the bottom to play.", AMBER),
            "broken" => ("Can't play", "Wisp couldn't play this wallpaper.", RED),
            _ => ("Starting…", "Starting…", MUTED),
        }
    }

    /// One line about everything, for the bottom bar.
    fn overall(&self) -> (String, Color32) {
        if !self.running {
            return ("Wallpapers aren't running.".into(), MUTED);
        }
        if let Some(a) = &self.adding {
            return (format!("Downloading to {}… it starts by itself when done", self.where_name(a.screen)), BLUE);
        }
        let n = if self.cfg.span { 1 } else { self.screens.len() };
        let states: Vec<&str> = (0..n).filter(|&i| !self.cfg.files[i].is_empty()).map(|i| self.status.get(i).map(String::as_str).unwrap_or("starting")).collect();
        if states.is_empty() {
            return ("Pick a wallpaper to get started".into(), MUTED);
        }
        if self.cfg.paused {
            return ("All screens paused".into(), AMBER);
        }
        if states.contains(&"game") {
            return ("Paused while a game or full-screen app is open · starts again by itself".into(), AMBER);
        }
        if states.contains(&"battery") {
            return ("Paused on battery · starts again when you plug in".into(), AMBER);
        }
        if let Some(i) = (0..n).find(|&i| self.status.get(i).is_some_and(|s| s == "broken")) {
            return (format!("{} can't play its wallpaper", self.where_name(i)), RED);
        }
        let playing = states.iter().filter(|s| **s == "playing").count();
        match playing {
            0 if states.iter().all(|s| *s == "covered") => ("Paused while windows cover the desktop".into(), AMBER),
            0 => ("Starting…".into(), MUTED),
            1 if n == 1 => ("Playing".into(), GREEN),
            k => (format!("Playing on {k} screen{}", if k == 1 { "" } else { "s" }), GREEN),
        }
    }

    fn thumb(&mut self, path: &str) -> Option<TextureHandle> {
        if path.is_empty() {
            return None;
        }
        match self.thumbs.get(path) {
            Some(Thumb::Ready(t)) => Some(t.clone()),
            Some(_) => None,
            None => {
                self.thumbs.insert(path.to_string(), Thumb::Loading);
                let _ = self.want_thumb.send(path.to_string());
                None
            }
        }
    }

    /// Draws a thumbnail cropped to fill `size` (like the Fill mode), or a placeholder.
    fn preview(&mut self, ui: &mut egui::Ui, path: &str, size: Vec2, ring: bool) -> egui::Response {
        let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
        self.paint_thumb(ui, rect, path, ring || resp.has_focus(), resp.hovered());
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    /// Paints a thumbnail into `rect`, cropped to fill like the Fill mode, or a placeholder.
    fn paint_thumb(&mut self, ui: &mut egui::Ui, rect: Rect, path: &str, ring: bool, hovered: bool) {
        let radius = CornerRadius::same(12);
        // Only tiles on screen ask for a thumbnail, so a long Library doesn't load every picture at once.
        let tex = if ui.is_rect_visible(rect) { self.thumb(path) } else { None };
        match tex {
            Some(tex) => {
                let [tw, th] = tex.size().map(|v| v as f32);
                let (img_aspect, box_aspect) = (tw / th, rect.aspect_ratio());
                let uv = if img_aspect > box_aspect {
                    let keep = box_aspect / img_aspect;
                    Rect::from_min_max(pos2((1.0 - keep) / 2.0, 0.0), pos2((1.0 + keep) / 2.0, 1.0))
                } else {
                    let keep = img_aspect / box_aspect;
                    Rect::from_min_max(pos2(0.0, (1.0 - keep) / 2.0), pos2(1.0, (1.0 + keep) / 2.0))
                };                egui::Image::new(&tex).uv(uv).corner_radius(radius).paint_at(ui, rect);
            }
            None => {
                ui.painter().rect_filled(rect, radius, RAISED);
                let loading = matches!(self.thumbs.get(path), Some(Thumb::Loading));
                if path.is_empty() {
                    let c = rect.center();
                    let s = Stroke::new(2.0_f32, MUTED);
                    ui.painter().line_segment([c - Vec2::new(10.0, 0.0), c + Vec2::new(10.0, 0.0)], s);
                    ui.painter().line_segment([c - Vec2::new(0.0, 10.0), c + Vec2::new(0.0, 10.0)], s);
                } else {
                    let text = if loading { "Loading…" } else { "No preview" };
                    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(13.0), MUTED);
                }
            }
        }
        if ring {
            ui.painter().rect_stroke(rect.expand(3.0), CornerRadius::same(15), Stroke::new(2.0_f32, TEXT), egui::StrokeKind::Inside);
        } else if hovered {
            ui.painter().rect_stroke(rect, radius, Stroke::new(1.5_f32, Color32::from_white_alpha(120)), egui::StrokeKind::Inside);
        }
    }

    /// What deleting a wallpaper means for it, as the menu says it; None when it can't be deleted from here.
    fn delete_label(&self, path: &str) -> Option<&'static str> {
        if workshop_item(path).is_some() {
            Some("Unsubscribe and delete from PC")
        } else if Path::new(path).starts_with(downloads_dir()) {
            Some("Delete from PC")
        } else if self.cfg.recent.iter().any(|r| r == path) {
            // Your own files are only taken off the list.
            Some("Remove from library")
        } else {
            None
        }
    }

    /// Hides a wallpaper at once; it's deleted for real a few seconds later, unless Undo is clicked.
    fn delete(&mut self, path: String) {
        if let Some((old, _, _)) = self.pending_delete.take() {
            self.delete_now(&old);
        }
        // A file that's leaving the PC comes off the screens playing it now, so its player lets go of it before
        // it's deleted. (Your own files are only taken off the list and keep playing.)
        let leaves = workshop_item(&path).is_some() || Path::new(&path).starts_with(downloads_dir());
        let screens: Vec<usize> = if leaves { (0..self.cfg.files.len()).filter(|&i| self.cfg.files[i] == path).collect() } else { Vec::new() };
        if !screens.is_empty() {
            for &i in &screens {
                self.cfg.files[i].clear();
            }
            self.trial = None;
            self.commit();
        }
        let done = if workshop_item(&path).is_some() {
            "Unsubscribed and deleted"
        } else if Path::new(&path).starts_with(downloads_dir()) {
            "Deleted"
        } else {
            "Removed from library:"
        };
        self.show_toast(format!("{done} “{}”", file_name(&path)), true);
        self.pending_delete = Some((path, Instant::now(), screens));
    }

    /// Takes a wallpaper out of the library. Wisp's downloads and Workshop items also leave the PC (Workshop:
    /// unsubscribed, then deleted); your own files are only taken off the list.
    fn delete_now(&mut self, path: &str) {
        self.cfg.recent.retain(|r| r != path);
        self.commit();
        // Files a screen still plays stay until that screen gets another wallpaper.
        if !self.cfg.files.iter().any(|f| f == path) {
            if Path::new(path).starts_with(downloads_dir()) {
                let _ = fs::remove_file(path);
            } else if workshop_item(path).is_some() {
                // Its own process: unsubscribing waits on Steam, and closing the window mustn't cut it short.
                spawn_self(&["--remove", path]);
                if let Some((_, list)) = self.installed.as_mut() {
                    list.retain(|p| p != path);
                }
            }
        }
    }

    /// The library: every wallpaper used, then the Wallpaper Engine ones on this PC that weren't used yet.
    fn library_items(&self) -> Vec<String> {
        let mut all = self.cfg.recent.clone();
        let known: std::collections::HashSet<String> = all.iter().map(|r| r.to_lowercase()).collect();
        if let Some((_, list)) = &self.installed {
            all.extend(list.iter().filter(|p| !known.contains(&p.to_lowercase())).cloned());
        }
        all.retain(|p| self.pending_delete.as_ref().is_none_or(|(d, _, _)| d != p));
        all
    }

    /// Looks for Wallpaper Engine wallpapers on this PC every half minute (new subscriptions show up).
    fn poll_installed(&mut self, ctx: &egui::Context) {
        if self.scanning.as_ref().is_some_and(|t| t.is_finished()) {
            let found = self.scanning.take().unwrap().join().unwrap_or_default();
            self.installed = Some((Instant::now(), found));
        }
        if self.scanning.is_none() && self.installed.as_ref().is_none_or(|(t, _)| t.elapsed().as_secs() >= 30) {
            let repaint = ctx.clone();
            self.scanning = Some(std::thread::spawn(move || {
                let found = installed_wallpapers();
                repaint.request_repaint();
                found
            }));
        }
    }

    /// The menu of a wallpaper tile, from its "⋯" button or a right-click.
    fn tile_menu(&mut self, ui: &mut egui::Ui, path: &str) {
        ui.set_min_width(220.0);
        if self.screens.len() > 1 && !self.cfg.span {
            for s in 0..self.screens.len() {
                if ui.button(format!("Put on Screen {}", s + 1)).clicked() {
                    self.use_on(s, path.to_string(), false);
                    ui.close();
                }
            }
        } else if ui.button("Use as wallpaper").clicked() {
            self.use_on(0, path.to_string(), false);
            ui.close();
        }
        if ui.button("Show in folder").clicked() {
            let _ = Command::new("explorer").arg(format!("/select,{path}")).spawn();
            ui.close();
        }
        if let Some(label) = self.delete_label(path) {
            ui.separator();
            if ui.button(RichText::new(label).color(Color32::from_rgb(255, 143, 143))).clicked() {
                self.delete(path.to_string());
                ui.close();
            }
        }
    }

    /// A wallpaper tile: picture, bold name, gray source, and a "⋯" menu. Click the picture to use it.
    fn tile(&mut self, ui: &mut egui::Ui, path: &str, size: Vec2) {
        let i = self.chosen();
        let current = self.cfg.files[i] == path;
        ui.allocate_ui(Vec2::new(size.x, size.y + 44.0), |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                let resp = self.preview(ui, path, size, current);
                badge_on(ui, resp.rect, if is_scene(path) { "Scene" } else { "Video" });
                let resp = resp.on_hover_text(if current { format!("On {} now", self.where_name(i)) } else { format!("Put on {}", self.where_name(i)) });
                if resp.clicked() && !current {
                    self.use_on(i, path.to_string(), false);
                }
                let p = path.to_string();
                resp.context_menu(|ui| self.tile_menu(ui, &p));
                // Right to left: the "⋯" sits at the tile's right edge and the name gets the rest.
                ui.allocate_ui_with_layout(Vec2::new(size.x, 22.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let more = more_button(ui).on_hover_text("More");
                    egui::Popup::menu(&more).show(|ui| self.tile_menu(ui, &p));
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(egui::Label::new(RichText::new(file_name(path)).font(bold(13.5))).truncate());
                    });
                });
                ui.label(small(source(path)));
            });
        });
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(18.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            if let Some(logo) = &self.logo {
                ui.add(egui::Image::new(logo).fit_to_exact_size(Vec2::splat(28.0)));
            }
            ui.label(RichText::new(APP).font(bold(18.0)));
        });
        ui.add_space(16.0);
        let count = self.library_items().len();
        for (page, icon, name, badge) in [
            (Page::Home, Icon::Screen, "My desktop", 0),
            (Page::Discover, Icon::Search, "Discover", 0),
            (Page::Library, Icon::Library, "Library", count),
            (Page::Settings, Icon::Gear, "Settings", 0),
        ] {
            self.nav(ui, page, icon, name, badge);
        }
        // The player sits in the sidebar's bottom corner.
        ui.add_space((ui.available_height() - 218.0).max(12.0));
        self.now_playing(ui);
    }

    /// Once, the first time the window opens: whether Wallpaper Engine is here, and what that means.
    fn we_note(&mut self, ctx: &egui::Context) {
        if self.cfg.told_we || self.close_note {
            return;
        }
        let found = self.we_dir.is_some();
        let count = self.installed.as_ref().map(|(_, l)| l.len()).filter(|n| *n > 0);
        let mut done = false;
        let modal = egui::Modal::new(egui::Id::new("we-note")).show(ctx, |ui| {
            ui.set_width(440.0);
            let (head, text) = if found {
                let n = count.map_or("the".to_string(), |n| n.to_string());
                ("Wallpaper Engine found", format!("Wisp can play {n} wallpapers you have in it, and Workshop wallpapers you find in Discover. They show up in your Library."))
            } else {
                ("No Wallpaper Engine on this PC", "That's fine: videos and GIFs from MoeWalls, MotionBGs or your own files work right away. Workshop scenes need Wallpaper Engine, a paid app on Steam.".to_string())
            };
            ui.label(RichText::new(head).font(bold(18.0)));
            ui.add_space(4.0);
            ui.add(egui::Label::new(RichText::new(text).size(13.5).color(GRAY)).wrap());
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if found {
                    if primary_button(ui, "Show them").clicked() {
                        self.page = Page::Library;
                        done = true;
                    }
                } else if primary_button(ui, "Browse free wallpapers").clicked() {
                    self.page = Page::Discover;
                    done = true;
                }
                if soft_button(ui, "OK").clicked() {
                    done = true;
                }
            });
        });
        if done || modal.should_close() {
            self.cfg.told_we = true;
            self.commit();
        }
    }

    fn nav(&mut self, ui: &mut egui::Ui, page: Page, icon: Icon, name: &str, count: usize) {
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 40.0), Sense::click());
        let on = self.page == page;
        if on || resp.hovered() || resp.has_focus() {
            ui.painter().rect_filled(rect, CornerRadius::same(10), if on { RAISED } else { Color32::from_rgb(23, 28, 38) });
        }
        let color = if on || resp.hovered() { TEXT } else { GRAY };
        draw_icon(ui.painter(), icon, Rect::from_center_size(pos2(rect.left() + 22.0, rect.center().y), Vec2::splat(18.0)), color);
        ui.painter().text(pos2(rect.left() + 42.0, rect.center().y), egui::Align2::LEFT_CENTER, name, if on { bold(14.0) } else { egui::FontId::proportional(14.0) }, color);
        if count > 0 {
            ui.painter().text(pos2(rect.right() - 12.0, rect.center().y), egui::Align2::RIGHT_CENTER, count.to_string(), egui::FontId::proportional(12.0), MUTED);
        }
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            if page == Page::Settings {
                self.storage = None; // measure again
            }
            self.page = page;
        }
        ui.add_space(2.0);
    }

    fn home(&mut self, ui: &mut egui::Ui) {
        let nothing = self.cfg.files.iter().all(String::is_empty) && self.cfg.waits.iter().all(String::is_empty) && self.cfg.recent.is_empty() && self.adding.is_none();
        if nothing {
            self.welcome(ui);
            return;
        }
        title(ui, "My desktop");
        sub(
            ui,
            if self.screens.len() > 1 {
                "Click a screen to choose what plays on it. You can also drag a video or GIF onto this window."
            } else {
                "This is what plays on your screen. You can also drag a video or GIF onto this window."
            },
        );
        ui.horizontal_top(|ui| {
            let left_w = (ui.available_width() * 0.46).clamp(300.0, 520.0);
            ui.allocate_ui_with_layout(Vec2::new(left_w, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(left_w);
                egui::Frame::new().fill(Color32::from_rgb(15, 19, 27)).corner_radius(CornerRadius::same(14)).inner_margin(14.0).show(ui, |ui| {
                    self.screens_canvas(ui, left_w - 28.0);
                });
            });
            ui.add_space(12.0);
            egui::Frame::new().fill(PANEL).corner_radius(CornerRadius::same(14)).inner_margin(16.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                // The row this sits in is horizontal; the card reads top to bottom.
                ui.vertical(|ui| self.screen_card(ui));
            });
        });
        self.scene_settings(ui);

        let recent: Vec<String> = self.cfg.recent.iter().take(RECENT_SHOWN).cloned().collect();
        if !recent.is_empty() {
            ui.add_space(26.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Recently used").font(bold(16.0)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ghost_button(ui, "See all").clicked() {
                        self.page = Page::Library;
                    }
                });
            });
            egui::ScrollArea::horizontal().id_salt("recent").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 16.0;
                    for path in recent {
                        self.tile(ui, &path, Vec2::new(200.0, 112.0));
                    }
                });
            });
        }
    }

    /// The screens as Windows arranges them: their real places and sizes, scaled to fit, each with what it's doing.
    fn screens_canvas(&mut self, ui: &mut egui::Ui, width: f32) {
        let rects: Vec<RECT> = self.screens.iter().map(|(r, _)| *r).collect();
        let (left, top) = (rects.iter().map(|r| r.left).min().unwrap_or(0), rects.iter().map(|r| r.top).min().unwrap_or(0));
        let (right, bottom) = (rects.iter().map(|r| r.right).max().unwrap_or(1), rects.iter().map(|r| r.bottom).max().unwrap_or(1));
        let (all_w, all_h) = ((right - left).max(1) as f32, (bottom - top).max(1) as f32);
        let scale = (width / all_w).min(230.0 / all_h);
        let (canvas, _) = ui.allocate_exact_size(Vec2::new(width, all_h * scale + 26.0), Sense::hover());
        // Centered across the panel.
        let origin = canvas.min + Vec2::new((width - all_w * scale) / 2.0, 0.0);
        for (i, m) in rects.iter().enumerate() {
            let at = |x: i32, y: i32| origin + Vec2::new((x - left) as f32 * scale, (y - top) as f32 * scale);
            let tile = Rect::from_min_max(at(m.left, m.top), at(m.right, m.bottom)).shrink(4.0);
            let resp = ui.interact(tile, ui.id().with(("screen", i)), Sense::click());
            // Drawn as it plays: fit mode, the part picked and the zoom. Spanning: Screen 1's wallpaper is one
            // picture over the whole desktop, so each tile shows its own piece of it.
            let owner = if self.cfg.span { 0 } else { i };
            let area = if self.cfg.span { Rect::from_min_max(at(left, top), at(right, bottom)) } else { tile };
            let path = self.cfg.files[owner].clone();
            let fit = self.cfg.fits[owner].clone();
            self.paint_placed(ui, tile, area, &path, &fit, self.cfg.pans[owner], self.cfg.zooms[owner], None);
            let ring = (self.cfg.span || self.selected == i) && rects.len() > 1;
            if ring || resp.has_focus() {
                ui.painter().rect_stroke(tile.expand(3.0), CornerRadius::same(11), Stroke::new(2.0_f32, TEXT), egui::StrokeKind::Inside);
            } else if resp.hovered() {
                ui.painter().rect_stroke(tile, CornerRadius::same(8), Stroke::new(1.5_f32, Color32::from_white_alpha(120)), egui::StrokeKind::Inside);
            }
            let (short, long, color) = self.screen_state(i);
            if short == "Can't play" {
                ui.painter().rect_filled(tile, CornerRadius::same(12), Color32::from_black_alpha(200));
                ui.painter().text(tile.center(), egui::Align2::CENTER_CENTER, "Can't play this", bold(13.0), Color32::from_rgb(255, 179, 179));
            }
            // A big number, like Windows' display settings, with a shadow so it reads on any picture.
            if rects.len() > 1 {
                let spot = tile.left_top() + Vec2::new(10.0, 4.0);
                ui.painter().text(spot + Vec2::splat(1.5), egui::Align2::LEFT_TOP, (i + 1).to_string(), bold(22.0), Color32::from_black_alpha(160));
                ui.painter().text(spot, egui::Align2::LEFT_TOP, (i + 1).to_string(), bold(22.0), TEXT);
            }
            // What it's doing, under it, cut to its width.
            let chip = Rect::from_min_size(pos2(tile.left(), tile.bottom() + 6.0), Vec2::new(tile.width().max(90.0), 18.0));
            let p = ui.painter().with_clip_rect(chip);
            let galley = p.layout_no_wrap(short.to_string(), egui::FontId::proportional(12.0), SOFT);
            let top = chip.center().y - galley.size().y / 2.0;
            p.galley(pos2(chip.left() + 13.0, top), galley.clone(), SOFT);
            status_dot(ui, ui.id().with(("chip", i)), pos2(chip.left() + 4.0, top + cap_middle(&galley, 12.0)), color);
            let tip = if self.cfg.span { format!("One picture across all screens · {long}") } else { format!("Screen {} · {long}", i + 1) };
            if resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                if !self.cfg.span {
                    self.selected = i;
                }
                // An empty screen: straight to choosing.
                if path.is_empty() {
                    self.sheet = true;
                    self.sheet_link = false;
                }
            }
        }
    }

    /// A picture's shape (width / height). Scenes say it in scene.json; for videos it's measured on a thread
    /// (Workshop previews can be square), with the thumbnail's shape until then.
    fn picture_aspect(&mut self, path: &str, tex: &TextureHandle) -> f32 {
        if let Some(a) = self.aspects.get(path) {
            return *a;
        }
        if is_scene(path) {
            let a = scene::scene_aspect(Path::new(path).parent().unwrap_or(Path::new(""))).unwrap_or(16.0 / 9.0);
            self.aspects.insert(path.to_string(), a);
            return a;
        }
        if self.aspect_asked.insert(path.to_string()) {
            let (p, done) = (path.to_string(), self.aspect_done.clone());
            std::thread::spawn(move || {
                if let Some((w, h)) = video_size(&p).filter(|(w, h)| *w > 0 && *h > 0) {
                    let _ = done.send((p, w as f32 / h as f32));
                }
            });
        }
        let [tw, th] = tex.size().map(|v| v as f32);
        tw / th.max(1.0)
    }

    /// Draws a wallpaper on a screen as it plays: `screen` is the screen's rectangle, `area` the space the picture
    /// is placed in (the screen, or the whole desktop when spanning). With `faint`, the parts cut off show dimmed
    /// within that rectangle, so it's clear what dragging can bring into view.
    #[allow(clippy::too_many_arguments)]
    fn paint_placed(&mut self, ui: &mut egui::Ui, screen: Rect, area: Rect, path: &str, fit: &str, pan: (f32, f32), zoom: f32, faint: Option<Rect>) {
        ui.painter().rect_filled(screen, CornerRadius::same(8), Color32::BLACK);
        let tex = if ui.is_rect_visible(screen) { self.thumb(path) } else { None };
        let Some(tex) = tex else {
            let c = screen.center();
            if path.is_empty() {
                let s = Stroke::new(2.0_f32, MUTED);
                ui.painter().line_segment([c - Vec2::new(10.0, 0.0), c + Vec2::new(10.0, 0.0)], s);
                ui.painter().line_segment([c - Vec2::new(0.0, 10.0), c + Vec2::new(0.0, 10.0)], s);
            } else {
                let text = if matches!(self.thumbs.get(path), Some(Thumb::Loading)) { "Loading…" } else { "No preview" };
                ui.painter().text(c, egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(12.5), MUTED);
            }
            return;
        };
        let pic = self.picture_aspect(path, &tex);
        let [l, t, w, h] = place(pic, area.aspect_ratio(), fit, pan, zoom);
        let r = Rect::from_min_size(area.min + Vec2::new(l * area.width(), t * area.height()), Vec2::new(w * area.width(), h * area.height()));
        // The thumbnail cropped to the picture's real shape.
        let [tw, th] = tex.size().map(|v| v as f32);
        let ta = tw / th.max(1.0);
        let uv = if ta > pic {
            let keep = pic / ta;
            Rect::from_min_max(pos2((1.0 - keep) / 2.0, 0.0), pos2((1.0 + keep) / 2.0, 1.0))
        } else {
            let keep = ta / pic;
            Rect::from_min_max(pos2(0.0, (1.0 - keep) / 2.0), pos2(1.0, (1.0 + keep) / 2.0))
        };
        if let Some(clip) = faint {
            ui.painter_at(clip).image(tex.id(), r, uv, Color32::from_white_alpha(60));
        }
        ui.painter_at(screen).image(tex.id(), r, uv, Color32::WHITE);
    }

    /// How the wallpaper sits on the screen, live. Pick Fill, Fit or Stretch (hovering one previews it); with Fill,
    /// drag the picture to choose which part shows, or zoom in.
    fn fit_preview(&mut self, ui: &mut egui::Ui, i: usize, file: &str) {
        let at = FITS.iter().position(|f| *f == self.cfg.fits[i]).unwrap_or(0);
        let shown = self.hover_fit.unwrap_or(at);
        let fit = FITS[shown];
        // The screen's shape; when spanning, the whole desktop's.
        let area = if self.cfg.span {
            let r: Vec<RECT> = self.screens.iter().map(|(m, _)| *m).collect();
            RECT { left: r.iter().map(|m| m.left).min().unwrap_or(0), top: r.iter().map(|m| m.top).min().unwrap_or(0), right: r.iter().map(|m| m.right).max().unwrap_or(1), bottom: r.iter().map(|m| m.bottom).max().unwrap_or(1) }
        } else {
            self.screens[i].0
        };
        let (sw, sh) = size(&area);
        let scr = sw.max(1) as f32 / sh.max(1) as f32;
        ui.label(RichText::new("How it looks").font(bold(13.0)));
        ui.add_space(2.0);
        let avail = ui.available_width();
        let max_h = 210.0;
        let (bw, bh) = if avail / scr <= max_h { (avail, avail / scr) } else { (max_h * scr, max_h) };
        let (row, _) = ui.allocate_exact_size(Vec2::new(avail, bh + 6.0), Sense::hover());
        let screen = Rect::from_center_size(row.center(), Vec2::new(bw, bh));
        let resp = ui.interact(screen, ui.id().with(("place", i)), Sense::drag());
        let (pan, zoom) = (self.cfg.pans[i], self.cfg.zooms[i]);
        // How much is cut off each way, from the picture's shape.
        let pic = match self.thumb(file) {
            Some(tex) => self.picture_aspect(file, &tex),
            None => 16.0 / 9.0,
        };
        let [_, _, w, h] = place(pic, scr, fit, pan, zoom);
        let movable = fit == "fill" && shown == at && (w > 1.001 || h > 1.001);
        let busy = movable && (resp.hovered() || resp.dragged());
        self.paint_placed(ui, screen, screen, file, fit, pan, zoom, busy.then_some(row));
        ui.painter().rect_stroke(screen, CornerRadius::same(8), Stroke::new(1.5_f32, if busy { TEXT } else { LINE }), egui::StrokeKind::Outside);
        if movable {
            if resp.dragged() {
                let d = resp.drag_delta();
                let (sx, sy) = ((w - 1.0) / 2.0, (h - 1.0) / 2.0);
                let (mut x, mut y) = pan;
                if sx > 0.001 {
                    x = (x + d.x / bw / sx).clamp(-1.0, 1.0);
                }
                if sy > 0.001 {
                    y = (y + d.y / bh / sy).clamp(-1.0, 1.0);
                }
                self.cfg.pans[i] = (x, y);
                // Saved once it rests, so the wallpaper isn't restarted on every step.
                self.save_soon(40);
            }
            resp.on_hover_cursor(if ui.input(|i| i.pointer.primary_down()) { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab });
        }
        ui.add_space(8.0);
        // The three modes as one plain switch. Hovering one shows it above before choosing.
        let mut hovered = None;
        egui::Frame::new().fill(FIELD).corner_radius(CornerRadius::same(10)).inner_margin(3.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for k in 0..3 {
                    let on = k == at;
                    let text = RichText::new(FIT_LABELS[k]).size(13.0).color(if on { TEXT } else { GRAY });
                    let b = egui::Button::new(text).fill(if on { SELECTED } else { Color32::TRANSPARENT }).corner_radius(CornerRadius::same(8)).min_size(Vec2::new(72.0, 28.0));
                    let resp = ui.add(b).on_hover_cursor(egui::CursorIcon::PointingHand);
                    if resp.hovered() {
                        hovered = Some(k);
                    }
                    if resp.clicked() && !on {
                        self.cfg.fits[i] = FITS[k].to_string();
                        self.commit();
                    }
                }
            });
        });
        self.hover_fit = hovered;
        let caption = match fit {
            "fill" if movable || shown != at => "Fill: covers the whole screen and cuts off what doesn't fit. Drag the picture to choose which part shows.",
            "fill" => "Fill: covers the whole screen. The picture's shape matches, so nothing is cut off; zoom in to move it.",
            "fit" => "Fit: shows the whole picture, with black bars where it doesn't reach the edges.",
            _ => "Stretch: covers the whole screen by squashing the picture to its shape.",
        };
        ui.add(egui::Label::new(small(caption)).wrap());
        if at == 0 && shown == 0 {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Zoom").font(bold(13.0)));
                ui.spacing_mut().slider_width = (ui.available_width() - 140.0).clamp(80.0, 260.0);
                let mut z = (zoom * 100.0).round() as u32;
                if ui.add(egui::Slider::new(&mut z, 100..=(MAX_ZOOM * 100.0) as u32).suffix("%")).changed() {
                    self.cfg.zooms[i] = z as f32 / 100.0;
                    self.save_soon(40);
                }
                if (pan, zoom) != ((0.0, 0.0), 1.0) && ghost_button(ui, "Reset").clicked() {
                    self.cfg.pans[i] = (0.0, 0.0);
                    self.cfg.zooms[i] = 1.0;
                    self.save_soon(40);
                }
            });
        }
    }

    /// The selected screen (or the span): what's on it and everything to change about it.
    fn screen_card(&mut self, ui: &mut egui::Ui) {
        let i = self.chosen();
        let (w, h) = size(&self.screens[i].0);
        let heading = if self.screens.len() == 1 {
            format!("YOUR SCREEN · {w} × {h}")
        } else if self.cfg.span {
            "ALL SCREENS · ONE PICTURE".to_string()
        } else {
            format!("SCREEN {} · {w} × {h}{}", i + 1, if i == 0 { " · MAIN" } else { "" })
        };
        ui.label(RichText::new(heading).font(bold(11.5)).color(GRAY));
        ui.add_space(4.0);
        let file = self.cfg.files[i].clone();
        let (_, long, color) = self.screen_state(i);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                let name = if file.is_empty() { "No wallpaper yet".to_string() } else { file_name(&file) };
                ui.add(egui::Label::new(RichText::new(name).font(bold(15.0))).truncate());
                if !file.is_empty() {
                    ui.label(small(format!("{} · {}", source(&file), if is_scene(&file) { "Scene" } else { "Video" })));
                }
                status_line(ui, long, color, 12.5);
            });
        });
        if self.screen_state(i).0 == "Can't play" {
            ui.add_space(8.0);
            egui::Frame::new().fill(Color32::from_rgb(42, 21, 23)).stroke(Stroke::new(1.0_f32, Color32::from_rgb(77, 38, 41))).corner_radius(CornerRadius::same(10)).inner_margin(10.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                let why = if is_scene(&file) {
                    "This scene uses effects Wisp can't show yet. It's not your PC, and most other scenes work."
                } else {
                    "The file may be damaged, or in a format Wisp can't play. It's not your PC."
                };
                ui.add(egui::Label::new(RichText::new(why).size(13.0).color(Color32::from_rgb(255, 214, 214))).wrap());
                ui.horizontal(|ui| {
                    if primary_button(ui, "Pick another").clicked() {
                        self.sheet = true;
                        self.sheet_link = false;
                    }
                    if soft_button(ui, "Copy details for a bug report").clicked() {
                        ui.ctx().copy_text(format!("Wisp {} couldn't play: {file}", env!("CARGO_PKG_VERSION")));
                        self.show_toast("Details copied. Paste them into a bug report.".into(), false);
                    }
                });
            });
        }
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if primary_button(ui, "Change wallpaper").clicked() {
                self.sheet = true;
                self.sheet_link = false;
            }
            if !file.is_empty() && ghost_button(ui, "Remove from screen").clicked() {
                self.cfg.files[i].clear();
                self.cfg.waits[i].clear();
                self.commit();
            }
        });
        if !file.is_empty() {
            divider(ui);
            self.fit_preview(ui, i, &file);
        }
        if self.screens.len() > 1 {
            divider(ui);
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(RichText::new("One picture across all screens").font(bold(13.0)));
                    ui.label(small("Stretches Screen 1's wallpaper over the whole desktop."));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if toggle(ui, &mut self.cfg.span).changed() {
                        if self.cfg.span {
                            self.selected = 0;
                        }
                        self.commit();
                    }
                });
            });
        }
    }

    /// First time, nothing set up: three plain ways to start.
    fn welcome(&mut self, ui: &mut egui::Ui) {
        egui::Frame::new().fill(Color32::from_rgb(22, 28, 41)).corner_radius(CornerRadius::same(16)).inner_margin(26.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            title(ui, "Welcome to Wisp");
            ui.add(egui::Label::new(RichText::new("Pick a moving wallpaper for your desktop. You can change it any time, and Wisp pauses it while a game or full-screen app is open.").size(13.5).color(GRAY)).wrap());
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                let w = (ui.available_width() - 28.0) / 3.0;
                if choice(ui, Icon::Search, "Find one online", "Browse thousands of free wallpapers. Click one and it's on your screen.", w).clicked() {
                    self.page = Page::Discover;
                }
                ui.add_space(6.0);
                if choice(ui, Icon::Folder, "Use a file from my PC", "A video or GIF you already have: MP4, WEBM, MKV or GIF.", w).clicked() {
                    if let Some(f) = pick_file() {
                        self.use_on(self.chosen(), f, false);
                    }
                }
                ui.add_space(6.0);
                if choice(ui, Icon::Link, "Paste a link", "A Steam Workshop page, or a direct link to a video or GIF.", w).clicked() {
                    self.sheet = true;
                    self.sheet_link = true;
                }
            });
        });
        let found: Vec<String> = self.installed.as_ref().map(|(_, l)| l.iter().take(12).cloned().collect()).unwrap_or_default();
        if !found.is_empty() {
            ui.add_space(26.0);
            ui.label(RichText::new("Already on your PC").font(bold(16.0)));
            ui.label(small("Wisp found these in your Wallpaper Engine. Click one to use it."));
            ui.add_space(6.0);
            egui::ScrollArea::horizontal().id_salt("found").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 16.0;
                    for path in found {
                        self.tile(ui, &path, Vec2::new(200.0, 112.0));
                    }
                });
            });
        }
    }

    /// The selected screen's scene settings, like turning its clock off. Nothing for videos or scenes without any.
    fn scene_settings(&mut self, ui: &mut egui::Ui) {
        let file = self.cfg.files[self.chosen()].clone();
        if file.is_empty() || self.screen_state(self.chosen()).0 == "Can't play" {
            return;
        }
        if !is_scene(&file) {
            ui.add_space(16.0);
            ui.label(small("Videos have no extra options. Wallpaper Engine scenes often do, like a clock or colors."));
            return;
        }
        if self.scene_props.as_ref().is_none_or(|(f, _)| *f != file) {
            self.scene_props = Some((file.clone(), scene::user_props(Path::new(&file).parent().unwrap_or(Path::new("")))));
        }
        let props = std::mem::take(&mut self.scene_props.as_mut().unwrap().1);
        if !props.is_empty() {
            ui.add_space(24.0);
            let mut changes = Vec::new();
            let mut reset = false;
            let saved = self.cfg.settings.get(&file).cloned().unwrap_or_default();
            let now = |p: &scene::UserProp| saved.get(&p.name).and_then(|t| serde_json::from_str(t).ok()).unwrap_or_else(|| p.value.clone());
            // Settings that only matter while another is on (clock format while the clock shows) are left out.
            let shown: Vec<&scene::UserProp> = props.iter().filter(|p| p.needs.as_ref().and_then(|d| props.iter().find(|q| q.name == *d)).is_none_or(|dep| is_on(&now(dep)))).collect();
            const FIRST: usize = 4;
            let hidden = shown.len().saturating_sub(FIRST);
            let more_options = self.more_options;
            group(ui, &format!("Options for {}", file_name(&file)), |ui| {
                ui.spacing_mut().slider_width = 180.0;
                for p in shown.iter().take(if more_options { usize::MAX } else { FIRST }) {
                    let value = now(p);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&p.label).font(bold(14.0)));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| match p.kind.as_str() {
                            "bool" => {
                                let mut on = is_on(&value);
                                if toggle(ui, &mut on).changed() {
                                    changes.push((*p, serde_json::Value::Bool(on)));
                                }
                            }
                            "slider" => {
                                let mut x = number(&value);
                                let s = egui::Slider::new(&mut x, p.min..=p.max).step_by(p.step);
                                if ui.add(s).changed() {
                                    changes.push((*p, serde_json::json!(x)));
                                }
                            }
                            "color" => {
                                let c: Vec<f32> = value.as_str().unwrap_or("").split_whitespace().filter_map(|s| s.parse().ok()).collect();
                                let mut rgb = [0, 1, 2].map(|k| c.get(k).copied().unwrap_or(1.0));
                                if ui.color_edit_button_rgb(&mut rgb).changed() {
                                    changes.push((*p, serde_json::Value::String(format!("{} {} {}", rgb[0], rgb[1], rgb[2]))));
                                }
                            }
                            "combo" => {
                                let same = |v: &serde_json::Value| v.to_string().trim_matches('"') == value.to_string().trim_matches('"');
                                let at = p.options.iter().position(|(_, v)| same(v)).unwrap_or(0);
                                let labels: Vec<&str> = p.options.iter().map(|(l, _)| l.as_str()).collect();
                                let picked = if labels.len() <= 4 {
                                    segmented(ui, &labels, at)
                                } else {
                                    let mut k = at;
                                    egui::ComboBox::from_id_salt(&p.name).selected_text(labels.get(at).copied().unwrap_or("")).show_ui(ui, |ui| {
                                        for (i, l) in labels.iter().enumerate() {
                                            ui.selectable_value(&mut k, i, *l);
                                        }
                                    });
                                    (k != at).then_some(k)
                                };
                                if let Some(k) = picked {
                                    changes.push((*p, p.options[k].1.clone()));
                                }
                            }
                            _ => {}
                        });
                    });
                }
                ui.horizontal(|ui| {
                    if hidden > 0 {
                        let label = if more_options { "Show fewer options".to_string() } else { format!("Show {hidden} more option{}", if hidden == 1 { "" } else { "s" }) };
                        if ghost_button(ui, &label).clicked() {
                            changes.clear();
                            self.more_options = !more_options;
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if !saved.is_empty() && ghost_button(ui, "Reset to default").clicked() {
                            reset = true;
                        }
                    });
                });
            });
            if reset {
                self.cfg.settings.remove(&file);
                self.save_soon(400);
            }
            for (p, v) in changes {
                let values = self.cfg.settings.entry(file.clone()).or_default();
                // Back at the scene's default: forget it, so the config only holds real changes.
                if v == p.value {
                    values.remove(&p.name);
                } else {
                    values.insert(p.name.clone(), v.to_string());
                }
                self.save_soon(400);
            }
        }
        self.scene_props.as_mut().unwrap().1 = props;
    }

    /// Workshop lookups and downloads go over the network, so they run on a thread and the window stays responsive.
    fn add_link(&mut self, screen: usize, link: String, span: bool) {
        if self.adding.is_some() {
            self.show_toast("Wait for the wallpaper being added to finish.".into(), false);
            return;
        }
        let link = link.trim().to_string();
        let workshop = link.contains("steamcommunity.com") || (!link.is_empty() && link.chars().all(|c| c.is_ascii_digit()));
        let l = link.clone();
        let job = if workshop {
            std::thread::spawn(move || match from_workshop(&l) {
                Workshop::Ready(f) => Added::File(f, None),
                Workshop::Waiting(id) => Added::Wait(id),
                Workshop::Nothing => Added::Failed(None), // from_workshop already said why
            })
        } else if link.starts_with("https://") || link.starts_with("http://") {
            std::thread::spawn(move || match download(l) {
                Ok((f, note)) => Added::File(f, note),
                Err(e) => Added::Failed(Some(e)),
            })
        } else {
            self.show_toast("Paste a Steam Workshop link, or a link to a video or GIF that starts with https://".into(), false);
            return;
        };
        if self.failed_link.as_ref() == Some(&link) {
            self.failed_link = None;
        }
        self.adding = Some(Adding { screen, link, span, job });
        self.sheet = false;
        self.detail = None;
    }

    /// A link being added finished: put it on its screen, or say why not.
    fn finish_adding(&mut self) {
        if !self.adding.as_ref().is_some_and(|a| a.job.is_finished()) {
            return;
        }
        let a = self.adding.take().unwrap();
        match a.job.join() {
            Ok(Added::File(f, note)) => {
                self.use_on(a.screen, f, a.span);
                if let Some(n) = note {
                    self.show_toast(n, false);
                }
            }
            Ok(Added::Wait(id)) => {
                if a.span {
                    self.cfg.span = true;
                }
                self.cfg.waits[a.screen] = id;
                self.commit();
                let msg = format!("Steam is downloading it. It starts on {} by itself when done.", self.where_name(a.screen));
                self.show_toast(msg, false);
            }
            Ok(Added::Failed(Some(e))) => {
                self.failed_link = Some(a.link);
                self.show_toast(e, false);
            }
            Ok(Added::Failed(None)) | Err(_) => {}
        }
    }

    /// One search source's tab: its name and what it needs.
    fn source_tab(&mut self, ui: &mut egui::Ui, k: usize) -> bool {
        let (badge, color) = match k {
            0 if self.we_dir.is_some() => ("Steam Workshop", BLUE),
            0 => ("Needs Wallpaper Engine", AMBER),
            1 => ("Free · needs Steam", GREEN),
            _ => ("Free · works now", GREEN),
        };
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(170.0, 52.0), Sense::click());
        let on = self.source == k;
        let lit = resp.hovered() || resp.has_focus();
        let p = ui.painter();
        p.rect(rect, CornerRadius::same(11), if on { Color32::from_rgb(27, 33, 44) } else { FIELD }, Stroke::new(1.5_f32, if on { TEXT } else if lit { MUTED } else { LINE }), egui::StrokeKind::Inside);
        p.text(rect.min + Vec2::new(13.0, 15.0), egui::Align2::LEFT_CENTER, SEARCH[k], bold(13.5), TEXT);
        let galley = p.layout_no_wrap(badge.into(), bold(11.0), color);
        let b = Rect::from_min_size(rect.min + Vec2::new(12.0, 28.0), galley.size() + Vec2::new(10.0, 4.0));
        p.rect_filled(b, CornerRadius::same(5), color.gamma_multiply(0.16));
        p.galley(b.min + Vec2::new(5.0, 2.0), galley, color);
        resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !on
    }

    /// Search the Workshop and wallpaper sites. A click shows a result big; "Put on …" adds it right away.
    fn discover(&mut self, ui: &mut egui::Ui) {
        let i = self.chosen();
        title(ui, "Discover");
        sub(ui, "Click a wallpaper to see it bigger and choose where it goes.");
        // The first visit shows the picked site's popular or newest wallpapers.
        let locked = self.we_dir.is_none() && self.source == 0;
        let mut go = self.hits.is_none() && self.searching.is_none() && !locked;
        let order = if self.we_dir.is_some() { [0, 1, 2, 3] } else { [2, 3, 1, 0] };
        ui.horizontal_wrapped(|ui| {
            for k in order {
                if self.source_tab(ui, k) {
                    self.source = k;
                    go = !(self.we_dir.is_none() && k == 0);
                }
            }
        });
        if self.we_dir.is_none() && !self.banner_closed {
            ui.add_space(10.0);
            egui::Frame::new().fill(Color32::from_rgb(29, 26, 18)).stroke(Stroke::new(1.0_f32, Color32::from_rgb(61, 52, 32))).corner_radius(CornerRadius::same(12)).inner_margin(12.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.allocate_ui_with_layout(Vec2::new(ui.available_width() - 90.0, 0.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.add(egui::Label::new(RichText::new("Wallpaper Engine isn't installed. MoeWalls and MotionBGs are free and work right away. Workshop wallpapers need Wallpaper Engine, a paid app on Steam.").size(13.0).color(Color32::from_rgb(231, 220, 194))).wrap());
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ghost_button(ui, "Got it").clicked() {
                            self.banner_closed = true;
                        }
                    });
                });
            });
        }
        if self.we_dir.is_none() && self.source == 0 {
            ui.add_space(14.0);
            egui::Frame::new().fill(PANEL).corner_radius(CornerRadius::same(14)).inner_margin(28.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("These need Wallpaper Engine").font(bold(17.0)));
                    ui.add_space(4.0);
                    ui.add(egui::Label::new(RichText::new("Workshop wallpapers are made for Wallpaper Engine, a paid app on Steam. Once you own it, Wisp plays them for you, using less memory.").size(13.0).color(GRAY)).wrap());
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.add_space((ui.available_width() - 420.0).max(0.0) / 2.0);
                        if primary_button(ui, "Get Wallpaper Engine on Steam ↗").clicked() {
                            open_url("https://store.steampowered.com/app/431960/");
                        }
                        if soft_button(ui, "Browse free wallpapers").clicked() {
                            self.source = 2;
                            self.start_search(ui.ctx(), 1);
                        }
                    });
                });
            });
            return;
        }
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text(format!("Search {}, like “rain” or “city”  ·  Ctrl+F", SEARCH[self.source]))
                    .desired_width(ui.available_width() - 130.0)
                    .margin(Vec2::new(12.0, 9.0)),
            );
            if self.focus_search {
                field.request_focus();
                self.focus_search = false;
            }
            let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let clicked = ui.add(egui::Button::new(RichText::new("Search").font(bold(13.5)).color(BG_BOTTOM)).fill(TEXT).min_size(Vec2::new(120.0, 36.0)).corner_radius(CornerRadius::same(9))).clicked();
            go |= clicked || enter;
        });
        ui.add_space(6.0);
        // Filters: a dropdown each. Site categories only work without a search, so they rest while one is typed.
        let searching_text = !self.query.trim().is_empty();
        ui.horizontal(|ui| {
            // egui sizes each dropdown's inner row by interact_size; smaller than our buttons, each one sat a little
            // lower than the last. Matching it keeps them on one line.
            ui.spacing_mut().interact_size.y = 34.0;
            for (k, (name, choices)) in filters(self.source).iter().enumerate() {
                let picked = &mut self.picks[self.source][k];
                let label = |c: usize| if c == 0 && *name == "Sort" && searching_text { "Best match" } else { choices[c].0 };
                let usable = !(searching_text && *name == "Category");
                let before = *picked;
                // Each in a cell of the same size, so they line up instead of stepping down.
                ui.allocate_ui_with_layout(Vec2::new(140.0, 34.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    if !usable {
                        ui.disable();
                    }
                    egui::ComboBox::from_id_salt(("filter", self.source, k)).selected_text(label(*picked)).width(130.0).show_ui(ui, |ui| {
                        for c in 0..choices.len() {
                            ui.selectable_value(picked, c, label(c));
                        }
                    });
                })
                .response
                .on_disabled_hover_text("Categories work when the search box is empty");
                go |= *picked != before;
            }
            // Wallpaper Engine only: a switch that hides everything tagged Anime, whatever the genre.
            if self.source == 0 {
                ui.add_space(10.0);
                go |= toggle(ui, &mut self.no_anime).on_hover_text("Hide wallpapers tagged Anime").changed();
                ui.label(RichText::new("Hide anime").size(13.5));
            }
        });
        if go {
            self.start_search(ui.ctx(), 1);
        }
        if self.searching.as_ref().is_some_and(|(_, t)| t.is_finished()) {
            let (page, t) = self.searching.take().unwrap();
            let found = t.join().unwrap_or_else(|_| Err("The search failed.".into()));
            match (found, &mut self.hits) {
                // "Load more": add what's new; nothing new means the site has no more pages.
                (Ok(new), Some(Ok(shown))) if page > 1 => {
                    let before = shown.len();
                    for hit in new {
                        if !shown.iter().any(|s| s.link == hit.link) {
                            shown.push(hit);
                        }
                    }
                    self.more_pages = shown.len() > before;
                    self.result_page = page;
                }
                (Err(e), Some(Ok(_))) if page > 1 => {
                    self.show_toast(e, false);
                    self.more_pages = false;
                }
                (found, _) => {
                    self.more_pages = matches!(&found, Ok(h) if !h.is_empty());
                    self.hits = Some(found);
                    self.result_page = 1;
                }
            }
        }
        ui.add_space(8.0);
        // The last results stay on screen while a new search runs, so the page doesn't jump.
        let status = match (&self.searching, &self.hits) {
            (Some((1, _)), _) => Some("Searching…".to_string()),
            (Some(_), _) => None,
            (None, Some(Err(e))) => Some(e.clone()),
            (None, Some(Ok(h))) if h.is_empty() => Some("Nothing found. Try other words or filters.".to_string()),
            _ => None,
        };
        // The status line always takes its space, so the results never shift when it appears.
        ui.label(RichText::new(status.unwrap_or_default()).color(MUTED));
        if let Some(Ok(hits)) = self.hits.clone() {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(16.0, 18.0);
                for hit in hits {
                    self.result_tile(ui, &hit);
                }
            });
            if self.more_pages {
                ui.add_space(14.0);
                let loading = matches!(self.searching, Some((p, _)) if p > 1);
                let button = egui::Button::new(if loading { "Loading…" } else { "Show more" }).min_size(Vec2::new(180.0, 36.0)).corner_radius(CornerRadius::same(9));
                if ui.add_enabled(self.searching.is_none(), button).clicked() {
                    self.start_search(ui.ctx(), self.result_page + 1);
                }
            }
        }

        ui.add_space(30.0);
        ui.label(RichText::new("Have a link?").font(bold(16.0)));
        ui.label(small("Paste a Steam Workshop link, a video or GIF link, or a wallpaper page from sites like MotionBGs and MoeWalls."));
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let button_w = 170.0;
            let field = ui.add(egui::TextEdit::singleline(&mut self.link).hint_text("https://…").desired_width(ui.available_width() - button_w - 10.0).margin(Vec2::new(12.0, 9.0)));
            let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let busy = self.adding.is_some();
            let label = if busy { "Adding…".to_string() } else { format!("Put on {}", self.where_name(i)) };
            let clicked = ui.add_enabled(!busy, egui::Button::new(label).min_size(Vec2::new(button_w, 36.0)).corner_radius(CornerRadius::same(9))).clicked();
            if (clicked || enter) && !busy && !self.link.trim().is_empty() {
                let link = std::mem::take(&mut self.link);
                self.add_link(i, link, false);
            }
        });
        // Pexels can't be searched from here (it blocks programs and its search needs a key): browse it there.
        ui.add_space(4.0);
        if ghost_button(ui, "Free stock videos on Pexels ↗").clicked() {
            open_url("https://www.pexels.com/videos/");
        }
    }

    /// A search result: click to see it big.
    fn result_tile(&mut self, ui: &mut egui::Ui, hit: &Hit) {
        let i = self.chosen();
        ui.allocate_ui(Vec2::new(208.0, 117.0 + 26.0), |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                let resp = self.preview(ui, &hit.thumb, Vec2::new(208.0, 117.0), false);
                let rect = resp.rect;
                let busy = self.adding.as_ref().is_some_and(|a| a.link == hit.link);
                let failed = self.failed_link.as_ref() == Some(&hit.link);
                if busy || failed {
                    ui.painter().rect_filled(rect, CornerRadius::same(12), Color32::from_black_alpha(if failed { 210 } else { 160 }));
                    let (a, b) = if busy { ("Downloading…", "It starts by itself when done") } else { ("Download failed", "Click to try again") };
                    if busy && anim(ui.ctx(), 1.0) > 0.0 {
                        egui::Spinner::new().size(18.0).color(TEXT).paint_at(ui, Rect::from_center_size(rect.center() - Vec2::new(0.0, 32.0), Vec2::splat(18.0)));
                    }
                    ui.painter().text(rect.center() - Vec2::new(0.0, 8.0), egui::Align2::CENTER_CENTER, a, bold(13.0), if failed { Color32::from_rgb(255, 190, 190) } else { TEXT });
                    ui.painter().text(rect.center() + Vec2::new(0.0, 10.0), egui::Align2::CENTER_CENTER, b, egui::FontId::proportional(12.0), SOFT);
                }
                if resp.clicked() && !busy {
                    if failed {
                        self.add_link(i, hit.link.clone(), false);
                    } else {
                        self.detail = Some(hit.clone());
                    }
                }
                ui.add(egui::Label::new(RichText::new(&hit.title).font(bold(13.5))).truncate());
            });
        });
    }

    /// Runs a search for `page` on its own thread. A newer search replaces one still running (its result is dropped).
    /// Page 1 is a new search from what's typed and picked now; later pages continue the search on screen, even if
    /// the box or filters changed since without searching.
    fn start_search(&mut self, ctx: &egui::Context, page: u32) {
        if page == 1 {
            self.shown_search = (self.source, self.query.clone(), self.picks[self.source], self.no_anime);
            // The last results' pictures are done with: free them, so browsing doesn't keep growing the window's memory.
            self.thumbs.retain(|k, _| !k.starts_with("https://"));
        }
        let (source, query, picks, no_anime) = self.shown_search.clone();
        let repaint = ctx.clone();
        self.searching = Some((
            page,
            std::thread::spawn(move || {
                let found = search(source, &query, &picks, no_anime, page);
                repaint.request_repaint();
                found
            }),
        ));
    }

    fn library(&mut self, ui: &mut egui::Ui) {
        let i = self.chosen();
        ui.horizontal(|ui| {
            title(ui, "Library");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if soft_button(ui, "+ Add wallpaper").clicked() {
                    self.sheet = true;
                    self.sheet_link = false;
                }
            });
        });
        sub(ui, &format!("Every wallpaper on this PC: ones you downloaded, added, or have in Wallpaper Engine. Click one to put it on {}.", self.where_name(i)));
        let all = self.library_items();
        if all.is_empty() {
            egui::Frame::new().fill(PANEL).corner_radius(CornerRadius::same(14)).inner_margin(28.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("Nothing here yet").font(bold(17.0)));
                    let note = if self.installed.is_none() { "Looking for Wallpaper Engine wallpapers…" } else { "Wallpapers you add or download show up here, so you can switch back any time." };
                    ui.label(RichText::new(note).size(13.0).color(GRAY));
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.add_space((ui.available_width() - 330.0).max(0.0) / 2.0);
                        if primary_button(ui, "Find wallpapers").clicked() {
                            self.page = Page::Discover;
                        }
                        if soft_button(ui, "Use a file from my PC").clicked() {
                            if let Some(f) = pick_file() {
                                self.use_on(i, f, false);
                            }
                        }
                    });
                });
            });
            return;
        }
        ui.horizontal(|ui| {
            for (k, name) in ["All", "Scenes", "Videos"].iter().enumerate() {
                if chip(ui, name, self.lib_filter == k).clicked() {
                    self.lib_filter = k;
                }
            }
        });
        ui.add_space(12.0);
        let shown: Vec<String> = all.into_iter().filter(|p| self.lib_filter == 0 || is_scene(p) == (self.lib_filter == 1)).collect();
        if shown.is_empty() {
            ui.label(RichText::new(if self.lib_filter == 1 { "No scenes yet." } else { "No videos yet." }).color(MUTED));
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(16.0, 18.0);
            for path in shown {
                self.tile(ui, &path, Vec2::new(200.0, 112.0));
            }
        });
        if self.installed.is_none() {
            ui.add_space(10.0);
            ui.label(RichText::new("Looking for Wallpaper Engine wallpapers…").color(MUTED));
        }
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        title(ui, "Settings");
        sub(ui, "Changes apply right away.");
        group(ui, "Picture", |ui| {
            let at = QUALITIES.iter().position(|q| *q == self.cfg.quality).unwrap_or(1);
            let desc = [
                "Fast: uses the least graphics power. Good for laptops.",
                "Sharp: crisp scaling and smooth gradients. Recommended for most PCs.",
                "Anime: AI upscaling for drawn art. Uses more graphics power.",
            ][at];
            row(ui, "Quality", desc, |ui| {
                if let Some(k) = segmented(ui, &QUALITY_LABELS, at) {
                    self.cfg.quality = QUALITIES[k].to_string();
                    self.commit();
                }
            });
            let mut limit = self.cfg.fps > 0;
            row(ui, "Limit smoothness", "Plays wallpapers at fewer frames per second, which uses less power.", |ui| {
                if toggle(ui, &mut limit).changed() {
                    self.cfg.fps = if limit { self.fps_drag } else { 0 };
                    self.commit();
                }
            });
            if limit {
                row(ui, &format!("Frames per second · {}", self.fps_drag), "30 looks smooth for most wallpapers. Videos made with fewer play as they are.", |ui| {
                    ui.spacing_mut().slider_width = 220.0;
                    let resp = ui.add(egui::Slider::new(&mut self.fps_drag, 10..=60).show_value(false));
                    // Save when the drag ends, not on every step: each change makes new wallpaper copies.
                    if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                        self.cfg.fps = self.fps_drag;
                        self.commit();
                    }
                });
            }
            let mut game = self.cfg.game_pause;
            row(ui, "Pause while a game or full-screen app is open", "Saves power. The wallpaper starts again by itself.", |ui| {
                if toggle(ui, &mut game).changed() {
                    self.cfg.game_pause = game;
                    self.commit();
                }
            });
            let mut battery = self.cfg.battery_pause;
            row(ui, "Pause on battery", "On a laptop, wallpapers stop while it's unplugged and start again when you plug in.", |ui| {
                if toggle(ui, &mut battery).changed() {
                    self.cfg.battery_pause = battery;
                    self.commit();
                }
            });
            let use_now = if self.running { format!("{} MB memory", self.ram) } else { "Not running".to_string() };
            row(ui, "Right now", "How much memory Wisp's wallpapers use, as Task Manager counts it.", |ui| {
                ui.label(RichText::new(use_now).size(13.0).color(SOFT));
            });
        });
        group(ui, "Sound", |ui| {
            let mut on = self.cfg.audio;
            let desc = if self.screens.len() > 1 { "Sound comes from the first screen that's playing." } else { "Sound comes from your wallpaper." };
            row(ui, "Play wallpaper sound", desc, |ui| {
                if toggle(ui, &mut on).changed() {
                    self.cfg.audio = on;
                    self.commit();
                }
            });
        });
        group(ui, "Wallpaper Engine", |ui| match &self.we_dir {
            Some(dir) => {
                let n = self.installed.as_ref().map_or(String::new(), |(_, l)| format!(" · {} wallpapers", l.len()));
                row(ui, "Found", &format!("{}{n}", dir.display()), |ui| {
                    ui.label(RichText::new("Connected").font(bold(12.0)).color(BLUE));
                });
            }
            None => {
                row(ui, "Not found", "Wisp still plays videos and GIFs. Workshop scenes need Wallpaper Engine (paid, on Steam).", |ui| {
                    if soft_button(ui, "Get it on Steam ↗").clicked() {
                        open_url("https://store.steampowered.com/app/431960/");
                    }
                });
            }
        });
        group(ui, "General", |ui| {
            let mut on = self.autostart;
            row(ui, "Start with Windows", "Your wallpaper comes back after a restart. Wisp starts quietly in the tray.", |ui| {
                if toggle(ui, &mut on).changed() {
                    self.autostart = on;
                    set_autostart(on);
                }
            });
            row(ui, "Closing this window keeps Wisp running", "Wallpapers keep playing. Open Wisp again, or quit it, from its icon next to the clock.", |_| {});
        });
        group(ui, "Keyboard", |ui| {
            for (what, keys) in [("Search wallpapers", &["Ctrl", "F"][..]), ("Move between wallpapers", &["←", "↑", "↓", "→"][..]), ("Use the selected wallpaper", &["Enter"][..]), ("Close a window or menu", &["Esc"][..])] {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(what).font(bold(14.0)));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        for k in keys.iter().rev() {
                            key_cap(ui, k);
                        }
                    });
                });
            }
        });
        let (cache, downloads) = *self.storage.get_or_insert_with(|| (dir_size(&cache_dir()), dir_size(&downloads_dir())));
        let mb = |b: u64| format!("{:.0} MB", b as f64 / 1_048_576.0);
        group(ui, "Storage", |ui| {
            row(ui, &format!("Downloaded wallpapers · {}", mb(downloads)), "Videos and GIFs you got from Discover or links.", |ui| {
                if soft_button(ui, "Open folder").clicked() {
                    let _ = fs::create_dir_all(downloads_dir());
                    let _ = Command::new("explorer").arg(downloads_dir()).spawn();
                }
            });
            row(ui, &format!("Temporary files · {}", mb(cache)), "Smaller copies of big videos so they use less memory. Safe to clear; they're made again when needed.", |ui| {
                if soft_button(ui, "Clear").clicked() {
                    // Files in use by a playing wallpaper stay; the rest are made again when needed.
                    for e in fs::read_dir(cache_dir()).into_iter().flatten().flatten() {
                        let _ = if e.path().is_dir() { fs::remove_dir_all(e.path()) } else { fs::remove_file(e.path()) };
                    }
                    self.thumbs.clear();
                    self.storage = None;
                }
            });
        });
        group(ui, "About", |ui| {
            row(ui, &format!("Wisp {}", env!("CARGO_PKG_VERSION")), "New versions install themselves.", |ui| {
                if soft_button(ui, "Settings folder").clicked() {
                    let _ = Command::new("explorer").arg(Config::path().parent().unwrap()).spawn();
                }
            });
        });
    }

    /// The sidebar's corner: what Wisp is doing and how much of the PC it uses, then pause and sound for everything.
    fn now_playing(&mut self, ui: &mut egui::Ui) {
        let w = ui.available_width();
        egui::Frame::new().fill(FIELD).corner_radius(CornerRadius::same(12)).inner_margin(12.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;
            let (text, color) = self.overall();
            status_line(ui, &text, color, 12.5);
            if !self.running {
                if primary_button(ui, "Start Wisp").clicked() {
                    spawn_self(&[]);
                }
                return;
            }
            let fmt = |v: Option<f32>, unit: &str| v.map_or("…".to_string(), |v| if unit == "%" && v < 10.0 { format!("{v:.1}%") } else { format!("{v:.0}{unit}") });
            for (name, value, tip) in [
                ("Memory", format!("{} MB", self.ram), "Memory Wisp's wallpapers use, as Task Manager counts it"),
                ("CPU", fmt(self.cpu, "%"), "Share of your whole processor"),
                ("GPU", fmt(self.gpu, "%"), "Share of your graphics card, as Task Manager counts it"),
            ] {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(name).size(12.5).color(GRAY));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(value).font(bold(12.5)).color(TEXT));
                    });
                })
                .response
                .on_hover_text(tip);
            }
        });
        ui.add_space(8.0);
        // Two equal buttons filling the row, each with its icon and label centered.
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let half = (w - 4.0) / 2.0;
            let paused = self.cfg.paused;
            // The pause bars reach 4.75 px each way; the play triangle 4 left, 6 right.
            let ink = if paused { (4.0, 6.0) } else { (4.75, 4.75) };
            if icon_button(ui, half, if paused { "Resume all" } else { "Pause all" }, ink, |p, c, col| paint_pause(p, c, paused, col)).clicked() {
                self.cfg.paused = !paused;
                self.commit();
            }
            let audio = self.cfg.audio;
            // The speaker's box starts 6 px left; its waves reach 14, the cross 14.5 px right.
            if icon_button(ui, half, if audio { "Sound on" } else { "Sound off" }, (6.0, 14.5), |p, c, col| paint_speaker(p, c, audio, col)).on_hover_text("Plays from the first screen that's playing").clicked() {
                self.cfg.audio = !audio;
                self.commit();
            }
        });
        ui.spacing_mut().slider_width = w;
        let mut volume = self.cfg.volume;
        let slider = ui.add_enabled(self.cfg.audio, egui::Slider::new(&mut volume, 0..=100).show_value(false));
        if slider.on_hover_text(format!("Volume {volume}%")).changed() {
            self.cfg.volume = volume;
            // Saved once the slider rests, like scene settings.
            self.save_soon(40);
        }
    }

    /// The Change wallpaper sheet: three ways to get one, and the library.
    fn sheet(&mut self, ctx: &egui::Context) {
        if !self.sheet {
            return;
        }
        let i = self.chosen();
        let lib: Vec<String> = self.library_items().into_iter().take(8).collect();
        let modal = egui::Modal::new(egui::Id::new("change-sheet")).show(ctx, |ui| {
            ui.set_width(660.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Change wallpaper on {}", self.where_name(i))).font(bold(20.0)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ghost_button(ui, "Close").clicked() {
                        self.sheet = false;
                    }
                });
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let w = (660.0 - 28.0) / 3.0;
                if choice(ui, Icon::Search, "Find one online", "Free wallpapers from MoeWalls, MotionBGs and the Steam Workshop.", w).clicked() {
                    self.sheet = false;
                    self.page = Page::Discover;
                }
                ui.add_space(6.0);
                if choice(ui, Icon::Folder, "Use a file from my PC", "MP4, WEBM, MKV or GIF.", w).clicked() {
                    if let Some(f) = pick_file() {
                        self.use_on(i, f, false);
                    }
                }
                ui.add_space(6.0);
                if choice(ui, Icon::Link, "Paste a link", "A Steam Workshop page, or a direct video or GIF link.", w).clicked() {
                    self.sheet_link = true;
                }
            });
            if self.sheet_link {
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let field = ui.add(egui::TextEdit::singleline(&mut self.link).hint_text("https://steamcommunity.com/sharedfiles/filedetails/?id=…").desired_width(560.0).margin(Vec2::new(12.0, 9.0)));
                    let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (primary_button(ui, "Add").clicked() || enter) && !self.link.trim().is_empty() {
                        let link = std::mem::take(&mut self.link);
                        self.add_link(i, link, false);
                    }
                });
            }
            if !lib.is_empty() {
                ui.add_space(16.0);
                ui.label(RichText::new("From your library").font(bold(15.0)));
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(12.0, 12.0);
                    for path in lib {
                        ui.allocate_ui(Vec2::new(156.0, 88.0 + 24.0), |ui| {
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 3.0;
                                if self.preview(ui, &path, Vec2::new(156.0, 88.0), false).clicked() {
                                    self.use_on(i, path.clone(), false);
                                }
                                ui.add(egui::Label::new(RichText::new(file_name(&path)).size(12.5)).truncate());
                            });
                        });
                    }
                });
            }
        });
        if modal.should_close() {
            self.sheet = false;
        }
    }

    /// A search result shown big, with where to put it.
    fn detail(&mut self, ctx: &egui::Context) {
        let Some(hit) = self.detail.clone() else { return };
        let from = self.shown_search.0;
        let modal = egui::Modal::new(egui::Id::new("detail")).show(ctx, |ui| {
            ui.set_width(720.0);
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(Vec2::new(620.0, 28.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::Label::new(RichText::new(&hit.title).font(bold(20.0))).truncate());
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ghost_button(ui, "Close").clicked() {
                        self.detail = None;
                    }
                });
            });
            ui.add_space(8.0);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(720.0, 405.0), Sense::hover());
            self.paint_thumb(ui, rect, &hit.thumb, false, false);
            ui.add_space(10.0);
            let needs = match from {
                0 => "Wallpaper Engine",
                1 => "Steam (free)",
                _ => "Nothing. It's a video",
            };
            ui.horizontal(|ui| {
                for (k, v) in [("From", SEARCH[from]), ("Needs", needs)] {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(small(k));
                        ui.label(RichText::new(v).font(bold(13.5)));
                    });
                    ui.add_space(30.0);
                }
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if self.screens.len() > 1 {
                    for s in 0..self.screens.len() {
                        let label = format!("Put on Screen {}", s + 1);
                        let clicked = if s == self.chosen() { primary_button(ui, &label).clicked() } else { soft_button(ui, &label).clicked() };
                        if clicked {
                            self.add_link(s, hit.link.clone(), false);
                        }
                    }
                    if soft_button(ui, "Across all screens").clicked() {
                        self.add_link(0, hit.link.clone(), true);
                    }
                } else if primary_button(ui, "Use as wallpaper").clicked() {
                    self.add_link(0, hit.link.clone(), false);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ghost_button(ui, "Open page ↗").clicked() {
                        open_url(&hit.link);
                    }
                });
            });
        });
        if modal.should_close() {
            self.detail = None;
        }
    }

    /// "Now on Screen 1: … · Keep · Go back", for a few seconds after a change. Slides down from the top when it
    /// comes and back up when it goes.
    fn trial_bar(&mut self, ctx: &egui::Context) {
        if self.trial.as_ref().is_some_and(|t| Instant::now() >= t.until) {
            self.trial = None;
        }
        if let Some(t) = &self.trial {
            let left = t.until.saturating_duration_since(Instant::now()).as_secs_f32().ceil() as u64;
            self.trial_seen = Some((t.screen, t.path.clone(), left));
        }
        let shown = ctx.animate_bool_with_time(egui::Id::new("trial-shown"), self.trial.is_some(), anim(ctx, 0.22));
        let Some((screen, path, left)) = self.trial_seen.clone() else { return };
        if shown <= 0.0 {
            self.trial_seen = None;
            return;
        }
        let eased = egui::emath::easing::cubic_out(shown);
        let text = format!("Now on {}: “{}”", self.where_name(screen), file_name(&path));
        let mut keep = false;
        let mut back = false;
        egui::Area::new(egui::Id::new("trial")).order(egui::Order::Foreground).fade_in(false).anchor(egui::Align2::CENTER_TOP, Vec2::new(112.0, 14.0 - (1.0 - eased) * 70.0)).show(ctx, |ui| {
            ui.multiply_opacity(shown);
            egui::Frame::new()
                .fill(Color32::from_rgb(35, 42, 55))
                .stroke(Stroke::new(1.0_f32, Color32::from_rgb(52, 60, 77)))
                .corner_radius(CornerRadius::same(14))
                .inner_margin(8.0)
                .shadow(egui::Shadow { offset: [0, 10], blur: 30, spread: 0, color: Color32::from_black_alpha(120) })
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        self.preview(ui, &path, Vec2::new(64.0, 36.0), false);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.label(RichText::new(&text).font(bold(13.5)));
                            ui.label(small(format!("Kept in {left} s unless you go back.")));
                        });
                        ui.add_space(6.0);
                        keep = primary_button(ui, "Keep").clicked();
                        back = soft_button(ui, "Go back").clicked();
                    });
                });
        });
        // While it slides away the buttons do nothing.
        if self.trial.is_none() {
            return;
        }
        if keep {
            self.trial = None;
        } else if back {
            let t = self.trial.take().unwrap();
            let (file, wait, span, recent) = t.prev;
            self.cfg.files[t.screen] = file;
            self.cfg.waits[t.screen] = wait;
            self.cfg.span = span;
            self.cfg.recent = recent;
            self.commit();
        }
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    /// A short message at the bottom (with Undo after a delete). Rises and fades in, sinks and fades out.
    fn toast(&mut self, ctx: &egui::Context) {
        if self.toast.as_ref().is_some_and(|t| Instant::now() >= t.until) {
            self.toast = None;
        }
        if let Some(t) = &self.toast {
            self.toast_seen = Some((t.text.clone(), t.undo));
        }
        let shown = ctx.animate_bool_with_time(egui::Id::new("toast-shown"), self.toast.is_some(), anim(ctx, 0.2));
        let Some((text, undo)) = self.toast_seen.clone() else { return };
        if shown <= 0.0 {
            self.toast_seen = None;
            return;
        }
        let eased = egui::emath::easing::cubic_out(shown);
        let mut undone = false;
        egui::Area::new(egui::Id::new("toast")).order(egui::Order::Foreground).fade_in(false).anchor(egui::Align2::CENTER_BOTTOM, Vec2::new(112.0, -24.0 + (1.0 - eased) * 24.0)).show(ctx, |ui| {
            ui.multiply_opacity(shown);
            egui::Frame::new().fill(TEXT).corner_radius(CornerRadius::same(12)).inner_margin(egui::Margin { left: 16, right: 10, top: 10, bottom: 10 }).show(ui, |ui| {
                ui.set_max_width(560.0);
                ui.horizontal(|ui| {
                    ui.add(egui::Label::new(RichText::new(&text).size(13.5).color(BG_BOTTOM)).wrap());
                    if undo {
                        let b = egui::Button::new(RichText::new("Undo").font(bold(13.0)).color(TEXT)).fill(BG_BOTTOM).corner_radius(CornerRadius::same(8));
                        undone = ui.add(b).on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
                    }
                });
            });
        });
        if undone && self.toast.is_some() {
            if let Some((path, _, screens)) = self.pending_delete.take() {
                if !screens.is_empty() {
                    // Only where nothing else was put since, so a newer choice isn't thrown away.
                    for i in screens {
                        if self.cfg.files[i].is_empty() {
                            self.cfg.files[i] = path.clone();
                        }
                    }
                    self.commit();
                }
            }
            self.toast = None;
        }
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    /// A file dragged over the window: show where it goes; dropped: use it.
    fn drop_files(&mut self, ctx: &egui::Context) {
        let i = self.chosen();
        let dropped: Vec<PathBuf> = ctx.input(|inp| inp.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if let Some(p) = dropped.first() {
            if is_video(p) {
                self.use_on(i, p.display().to_string(), false);
            } else {
                self.show_toast("Wisp plays videos and GIFs: MP4, WEBM, MKV, MOV, AVI, WEBP and GIF.".into(), false);
            }
        }
        if ctx.input(|inp| inp.raw.hovered_files.is_empty()) {
            return;
        }
        let screen = ctx.screen_rect();
        let target = self.where_name(i);
        egui::Area::new(egui::Id::new("drop")).order(egui::Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
            let p = ui.painter();
            p.rect_filled(screen, CornerRadius::ZERO, Color32::from_rgba_unmultiplied(10, 14, 22, 235));
            let box_ = Rect::from_center_size(screen.center(), Vec2::new(420.0, 180.0));
            p.rect_stroke(box_, CornerRadius::same(16), Stroke::new(2.0_f32, BLUE), egui::StrokeKind::Inside);
            p.text(box_.center() - Vec2::new(0.0, 22.0), egui::Align2::CENTER_CENTER, format!("Drop to put it on {target}"), bold(19.0), TEXT);
            p.text(box_.center() + Vec2::new(0.0, 8.0), egui::Align2::CENTER_CENTER, "MP4, WEBM, MKV or GIF", egui::FontId::proportional(13.0), GRAY);
            if self.screens.len() > 1 {
                p.text(box_.center() + Vec2::new(0.0, 30.0), egui::Align2::CENTER_CENTER, "For another screen, pick it on My desktop first.", egui::FontId::proportional(12.5), MUTED);
            }
        });
    }

    /// Closing the window the first time: say once that Wisp keeps running in the tray.
    fn close_note(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) && !self.cfg.told_tray {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.close_note = true;
        }
        if !self.close_note {
            return;
        }
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("close-note")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.label(RichText::new("Wisp keeps running").font(bold(18.0)));
            ui.add_space(4.0);
            ui.add(egui::Label::new(RichText::new("Closing this window doesn't stop your wallpapers. Open Wisp again, or quit it, from its icon next to the clock (it may be under the ^ arrow).").size(13.5).color(GRAY)).wrap());
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                close = primary_button(ui, "Got it, close the window").clicked();
                if soft_button(ui, "Keep it open").clicked() {
                    self.close_note = false;
                }
            });
        });
        if modal.should_close() {
            self.close_note = false;
        }
        if close {
            self.close_note = false;
            self.cfg.told_tray = true;
            self.commit();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Ctrl+F jumps to search. (egui itself moves between tiles with Tab and the arrow keys, and Enter clicks.)
    fn keys(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F)) {
            self.page = Page::Discover;
            self.sheet = false;
            self.detail = None;
            self.focus_search = true;
        }
    }
}

/// A soft top-to-bottom gradient behind the page.
fn backdrop(ui: &egui::Ui) {
    let r = ui.clip_rect();
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(r.left_top(), BG_TOP);
    mesh.colored_vertex(r.right_top(), BG_TOP);
    mesh.colored_vertex(r.right_bottom(), BG_BOTTOM);
    mesh.colored_vertex(r.left_bottom(), BG_BOTTOM);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    ui.painter().add(mesh);
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok((path, img)) = self.got_thumb.try_recv() {
            let state = match img {
                Some(img) => Thumb::Ready(ctx.load_texture(&path, img, Default::default())),
                None => Thumb::Missing,
            };
            self.thumbs.insert(path, state);
        }
        while let Ok((path, a)) = self.got_aspect.try_recv() {
            self.aspects.insert(path, a);
        }
        self.finish_adding();
        if self.setting_changed.is_some_and(|t| Instant::now() >= t) {
            self.setting_changed = None;
            self.commit();
        }
        if self.setting_changed.is_some() {
            ctx.request_repaint_after(Duration::from_millis(20));
        }
        // Once a second: the background process's changes (like a finished Workshop download), what it's doing,
        // and where the tray menu asked to go. Not while a setting waits to save.
        if self.setting_changed.is_none() && self.checked.elapsed().as_secs() >= 1 {
            self.checked = Instant::now();
            let disk = Config::read_disk().unwrap_or_else(|| self.seen.clone());
            if disk != self.seen {
                self.cfg = Config::parse(&disk);
                self.cfg.fit_to(self.screens.len());
                self.seen = disk;
            }
            self.read_status();
            self.go_to_asked_page();
        }
        if self.pending_delete.as_ref().is_some_and(|(_, t, _)| t.elapsed().as_secs() >= 7) {
            let (path, _, _) = self.pending_delete.take().unwrap();
            self.delete_now(&path);
        }
        self.poll_installed(ctx);
        self.keys(ctx);
        ctx.request_repaint_after(Duration::from_secs(1));

        egui::SidePanel::left("nav")
            .exact_width(224.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(SIDEBAR).inner_margin(egui::Margin::symmetric(12, 0)))
            .show(ctx, |ui| self.sidebar(ui));
        egui::CentralPanel::default().frame(egui::Frame::new().inner_margin(egui::Margin { left: 30, right: 30, top: 26, bottom: 10 })).show(ctx, |ui| {
            backdrop(ui);
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| match self.page {
                Page::Home => self.home(ui),
                Page::Discover => self.discover(ui),
                Page::Library => self.library(ui),
                Page::Settings => self.settings(ui),
            });
        });
        self.sheet(ctx);
        self.detail(ctx);
        self.trial_bar(ctx);
        self.toast(ctx);
        self.drop_files(ctx);
        self.close_note(ctx);
        self.we_note(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // A delete still waiting for Undo happens now.
        if let Some((path, _, screens)) = self.pending_delete.take() {
            // Just taken off a screen: give the background a moment to let go of the file.
            if !screens.is_empty() {
                std::thread::sleep(Duration::from_millis(1500));
            }
            self.delete_now(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_made_absolute() {
        let page = "https://motionbgs.com/arlecchino";
        assert_eq!(absolute("/dl/hd/1", page), "https://motionbgs.com/dl/hd/1");
        assert_eq!(absolute("//cdn.x.com/a.mp4", page), "https://cdn.x.com/a.mp4");
        assert_eq!(absolute("b.mp4", "https://x.com/dir/page"), "https://x.com/dir/b.mp4");
        assert_eq!(absolute("https://y.com/a.mp4?x=1&amp;y=2", page), "https://y.com/a.mp4?x=1&y=2");
    }

    #[test]
    fn reads_search_results() {
        // Snippets shaped like the real pages (November 2026).
        let steam = r#"<a href="https://steamcommunity.com/sharedfiles/filedetails/?id=3018519288" class="x"><img src="https://images.steamusercontent.com/ugc/1/A/?ima=fit&amp;imw=322" alt="Red | Satoru Gojo"></a><a href="https://steamcommunity.com/sharedfiles/filedetails/?id=3018519288">again</a>"#;
        let h = steam_hits(steam);
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].title.as_str(), h[0].thumb.as_str()), ("Red | Satoru Gojo", "https://images.steamusercontent.com/ugc/1/A/?ima=fit&imw=322"));
        assert_eq!(h[0].link, "https://steamcommunity.com/sharedfiles/filedetails/?id=3018519288");
        let moe = r#"<article class="entry"><a title="Gojo Hollow Purple Jujutsu Kaisen Live Wallpaper" href="https://moewalls.com/anime/gojo-3/"><img src="https://moewalls.com/wp-content/uploads/g-thumb.jpg" alt=""></a></article>"#;
        let h = moewalls_hits(moe);
        assert_eq!((h[0].link.as_str(), h[0].thumb.as_str()), ("https://moewalls.com/anime/gojo-3/", "https://moewalls.com/wp-content/uploads/g-thumb.jpg"));
        let mb = r#"<a title="Gojo With Blue Rose (Jujutsu Kaisen) live wallpaper" href=/gojo-with-blue-rose> <figure><picture><source srcset=/i/c/546x308/media/420/g.jpg.webp type=image/webp><source srcset=/i/c/364x205/media/420/g.jpg type=image/jpeg></picture></figure></a><a title="Tags" href=/tag:x/>"#;
        let h = motionbgs_hits(mb);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].title, "Gojo With Blue Rose (Jujutsu Kaisen)");
        assert_eq!((h[0].link.as_str(), h[0].thumb.as_str()), ("https://motionbgs.com/gojo-with-blue-rose", "https://motionbgs.com/i/c/364x205/media/420/g.jpg"));
    }

    #[test]
    fn cuts_text_safely() {
        // "漢" is 3 bytes (2..5 here): a cut at 3 or 4 steps back to 2 instead of crashing.
        assert_eq!(head("ab漢字", 3), "ab");
        assert_eq!(head("ab漢字", 5), "ab漢");
        assert_eq!(head("ab", 800), "ab");
        // A Steam result whose title runs past the old 800-byte cut, mid-character, still parses.
        let page = format!(r#"<a href="https://steamcommunity.com/sharedfiles/filedetails/?id=42"><img src="https://img/1" alt="x{}">"#, "漢".repeat(400));
        let hits = steam_hits(&page);
        assert_eq!((hits.len(), hits[0].thumb.as_str()), (1, "https://img/1"));
    }

    #[test]
    fn builds_search_addresses() {
        // Wallpaper Engine: trending without words, best match with them; tags from the picks; never Web wallpapers,
        // and no Mature ones unless picked. Filters: Type, Genre, Size, Age, Sort.
        assert_eq!(search_url(0, "", &[0; 6], false, 1), "https://steamcommunity.com/workshop/browse/?appid=431960&browsesort=trend&searchtext=&excludedtags[]=Web&excludedtags[]=Mature");
        let u = search_url(0, "gojo blue", &[1, 1, 3, 0, 3, 0], false, 1);
        assert!(u.contains("browsesort=mostrecent") && u.contains("searchtext=gojo%20blue"));
        assert!(u.contains("requiredtags[]=Scene") && u.contains("requiredtags[]=Anime") && u.contains("requiredtags[]=3840%20x%202160"));
        assert!(search_url(0, "gojo", &[0; 6], false, 1).contains("browsesort=textsearch"));
        // The No anime switch excludes the tag; Any age drops the Mature filter; Mature asks for it.
        let u = search_url(0, "", &[0, 0, 0, 4, 0, 0], true, 1);
        assert!(u.contains("excludedtags[]=Anime") && !u.contains("Mature"));
        assert!(search_url(0, "", &[0, 0, 0, 3, 0, 0], false, 1).contains("requiredtags[]=Mature"));
        // Live Wallpaper: sort only, no tags.
        assert!(search_url(1, "", &[2, 0, 0, 0, 0, 0], false, 1).contains("appid=4056970&browsesort=toprated") && !search_url(1, "", &[2, 0, 0, 0, 0, 0], false, 1).contains("tags"));
        // Sites: a category page without words, the search page with them (categories rest then).
        assert_eq!(search_url(2, "", &[1, 0, 0, 0, 0, 0], false, 1), "https://moewalls.com/category/anime/");
        assert_eq!(search_url(2, "gojo", &[1, 0, 0, 0, 0, 0], false, 1), "https://moewalls.com/?s=gojo");
        assert_eq!(search_url(3, "", &[2, 0, 0, 0, 0, 0], false, 1), "https://motionbgs.com/tag:games/");
        // Page 2 and on, each site its own way.
        assert!(search_url(0, "", &[0; 6], false, 3).ends_with("&p=3"));
        assert_eq!(search_url(2, "", &[0; 6], false, 2), "https://moewalls.com/page/2/");
        assert_eq!(search_url(2, "", &[1, 0, 0, 0, 0, 0], false, 2), "https://moewalls.com/category/anime/page/2/");
        assert_eq!(search_url(2, "gojo", &[0; 6], false, 2), "https://moewalls.com/page/2/?s=gojo");
        assert_eq!(search_url(3, "", &[0; 6], false, 2), "https://motionbgs.com/2/");
        assert_eq!(search_url(3, "", &[2, 0, 0, 0, 0, 0], false, 2), "https://motionbgs.com/tag:games/2/");
        assert_eq!(search_url(3, "gojo", &[0; 6], false, 2), "https://motionbgs.com/search?q=gojo&page=2");
    }

    /// Searches every source for real, and every filter choice. Needs the internet: run with --ignored.
    #[test]
    #[ignore]
    fn searches_every_source() {
        for (k, name) in SEARCH.iter().enumerate() {
            let found = search(k, "gojo", &[0; 6], false, 1).unwrap();
            let front = search(k, "", &[0; 6], false, 1).unwrap();
            // Page 2 has results, and not just page 1's again.
            let next = search(k, "", &[0; 6], false, 2).unwrap();
            if k == 0 {
                assert!(!search(k, "", &[0; 6], true, 1).unwrap().is_empty(), "No anime finds nothing");
            }
            assert!(next.iter().any(|h| !front.iter().any(|f| f.link == h.link)), "{name}: page 2 is empty or repeats page 1");
            println!("{name}: {} results for gojo, {} on the front page", found.len(), front.len());
            assert!(!found.is_empty() && !front.is_empty(), "{name} found nothing");
            assert!(found.iter().all(|h| h.thumb.starts_with("https://") && h.link.starts_with("https://") && !h.title.is_empty()), "{name} has a broken result");
            // The first result's picture downloads and decodes.
            assert!(make_thumb(&found[0].thumb).is_some(), "{name}: no picture for {}", found[0].thumb);
            // Each filter choice finds something on its own.
            for (i, (filter, choices)) in filters(k).iter().enumerate() {
                for c in 1..choices.len() {
                    let mut picks = [0; 6];
                    picks[i] = c;
                    let n = search(k, "", &picks, false, 1).unwrap().len();
                    assert!(n > 0, "{name} {filter} {}: nothing", choices[c].0);
                }
            }
        }
    }

    #[test]
    fn finds_page_videos() {
        // Trimmed from real MotionBGs and MoeWalls pages: full download links win over player previews.
        let motionbgs = "<video autoplay><source src=/media/10351/arle.960x540.mp4 type=video/mp4></video>\
                         <a href=/dl/4k/10351>4K</a><a href=/dl/hd/10351>HD</a><link href=/static/main.css>";
        let found = page_videos(motionbgs, "https://motionbgs.com/arle");
        assert_eq!(found[0], ("https://motionbgs.com/dl/4k/10351".to_string(), false));
        assert_eq!(found[1], ("https://motionbgs.com/dl/hd/10351".to_string(), false));
        assert_eq!(found[2], ("https://motionbgs.com/media/10351/arle.960x540.mp4".to_string(), true));
        let moewalls = r##"<img src="/wp-content/plugins/x/loading.gif"><video id="p" poster="/t.jpg">
                          <source src="/wp-content/uploads/preview/2026/rose-preview.webm" type="video/mp4" /></video>
                          <a id="moe-download" data-url="hYLv" href="#">Download</a>"##;
        let found = page_videos(moewalls, "https://moewalls.com/anime/rose/");
        assert_eq!(found, vec![("https://moewalls.com/wp-content/uploads/preview/2026/rose-preview.webm".to_string(), true)]);
    }

    /// Downloads from the real sites, so it needs the internet. Run with: cargo test -- --ignored
    #[test]
    #[ignore]
    fn real_pages() {
        for (url, preview) in [
            ("https://moewalls.com/anime/white-rose-anime-girl-live-wallpaper/", true),
            ("https://motionbgs.com/arlecchino-girl-behind-the-curtain", false),
        ] {
            let (file, note) = download(url.to_string()).unwrap_or_else(|e| panic!("{url}: {e}"));
            let (w, h) = video_size(&file).unwrap();
            println!("{url} -> {file} {w}x{h} preview={}", note.is_some());
            assert_eq!(note.is_some(), preview);
            let _ = fs::remove_file(file);
        }
    }
}
