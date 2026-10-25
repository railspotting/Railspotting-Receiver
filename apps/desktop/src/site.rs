//! Where links go: the receiver's dashboard, the Live Map window, or the browser.
//!
//! The Live Map window shows the headend's page made for it, https://railspotting.com/embed/map
//! (contract: docs/embed-map.md in the headend repository). It loads that page and the video
//! frames the page embeds, nothing else; every other navigation and new window goes to the
//! browser, where the owner is signed in. The site signs in with Google, which doesn't allow
//! sign-in inside an app's web view.
//!
//! The page's own requests (/static/, /api/public/, map tiles) are not navigations and need no
//! rule here. The navigation hook also sees sub-frame loads, without saying which frame, so the
//! video frames are allowed by address.

use tauri::Url;

pub const LIVE_MAP: &str = "https://railspotting.com/embed/map";
/// The site's main page, for "Open RailSpotting.com in Browser".
pub const SITE: &str = "https://railspotting.com/";

/// Whether a page may have this Mac's position (the Live Map's Here button): only the site
/// itself, over HTTPS on the standard port. The page moves the map with it; it isn't sent.
pub fn shares_location(protocol: &str, host: &str, port: i64) -> bool {
    protocol == "https" && host == "railspotting.com" && (port == 0 || port == 443)
}

/// The views in the main window, one tab each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Receiver,
    Map,
}

impl Tab {
    pub fn name(self) -> &'static str {
        match self {
            Tab::Receiver => "receiver",
            Tab::Map => "map",
        }
    }
}

/// A click in the tab bar: its links are rsapp://tab/<name>, which the app catches.
pub fn tab_request(url: &Url) -> Option<Tab> {
    if url.scheme() != "rsapp" || url.host_str() != Some("tab") {
        return None;
    }
    match url.path() {
        "/receiver" => Some(Tab::Receiver),
        "/map" => Some(Tab::Map),
        _ => None,
    }
}

/// The tab bar's own page.
pub fn is_tab_bar(url: &Url) -> bool {
    url.scheme() == "tauri" && url.host_str() == Some("localhost") && url.path() == "/tabs.html"
}

/// Any other link in the tab bar (the pop-out to railspotting.com): the browser.
pub fn from_tab_bar(url: &Url) -> Route {
    if is_tab_bar(url) { Route::Stay } else { web(url) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The page itself, in the window it was asked for.
    Stay,
    /// The Live Map tab.
    LiveMap,
    /// The default browser.
    Browser,
    /// Nothing (a scheme that isn't http or https).
    Drop,
}

/// The Live Map page, for this receiver when its device ID is known. The page ignores an ID it
/// doesn't show publicly.
pub fn live_map_url(device_id: Option<&str>) -> Url {
    let mut u = Url::parse(LIVE_MAP).expect("LIVE_MAP is a valid URL");
    if let Some(id) = device_id.filter(|id| !id.is_empty()) {
        u.query_pairs_mut().append_pair("receiver", id);
    }
    u
}

fn is_site(url: &Url) -> bool {
    url.scheme() == "https" && url.port().is_none() && url.host_str() == Some("railspotting.com")
}

/// The Live Map page, with any query.
fn is_live_map(url: &Url) -> bool {
    is_site(url) && url.path() == "/embed/map"
}

/// Videos on train notes, embedded by the Live Map page.
fn is_video_frame(url: &Url) -> bool {
    url.scheme() == "https" && url.port().is_none() && url.host_str() == Some("www.youtube-nocookie.com")
        && url.path().starts_with("/embed/")
}

fn web(url: &Url) -> Route {
    if matches!(url.scheme(), "http" | "https") { Route::Browser } else { Route::Drop }
}

/// A link followed in the dashboard window, which shows only this receiver's own page. A link
/// to the site's main page or the Live Map opens the Live Map window.
pub fn from_dashboard(url: &Url, dashboard_port: Option<u16>) -> Route {
    let local = url.scheme() == "tauri"
        || (url.scheme() == "http" && url.host_str() == Some("127.0.0.1") && url.port().is_some() && url.port() == dashboard_port);
    if local {
        Route::Stay
    } else if is_live_map(url) || (is_site(url) && url.path() == "/") {
        Route::LiveMap
    } else {
        web(url)
    }
}

/// A navigation in the Live Map window (main frame or sub-frame).
pub fn from_live_map(url: &Url) -> Route {
    if is_live_map(url) || is_video_frame(url) || url.as_str() == "about:blank" { Route::Stay } else { web(url) }
}

/// A new window asked for by the Live Map page: always the browser.
pub fn new_window_from_live_map(url: &Url) -> Route {
    web(url)
}

/// Where a download from the dashboard goes, as a browser would put it: the Downloads folder,
/// under the page's suggested name (else the address's last part), with " (2)", " (3)" and so on
/// before the extension when the name is taken. None for a download from anywhere else.
pub fn download_path(url: &Url, dashboard_port: Option<u16>, suggested: &std::path::Path, downloads: &std::path::Path,
                     exists: &dyn Fn(&std::path::Path) -> bool) -> Option<std::path::PathBuf> {
    if from_dashboard(url, dashboard_port) != Route::Stay || url.scheme() != "http" {
        return None;
    }
    let clean = |s: &str| -> String {
        let s: String = s.chars().map(|c| if c == '/' || c == '\\' || c == ':' || c.is_control() { '_' } else { c }).collect();
        s.trim_start_matches('.').trim().to_string()
    };
    let mut name = suggested.file_name().map(|n| clean(&n.to_string_lossy())).unwrap_or_default();
    if name.is_empty() {
        name = clean(url.path_segments().and_then(|mut s| s.next_back()).unwrap_or(""));
    }
    if name.is_empty() {
        name = "download".into();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (name[..i].to_string(), name[i..].to_string()),
        _ => (name.clone(), String::new()),
    };
    let mut path = downloads.join(&name);
    let mut n = 2;
    while exists(&path) {
        path = downloads.join(format!("{stem} ({n}){ext}"));
        n += 1;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn tab_bar_links() {
        assert_eq!(tab_request(&u("rsapp://tab/receiver")), Some(Tab::Receiver));
        assert_eq!(tab_request(&u("rsapp://tab/map")), Some(Tab::Map));
        assert_eq!(tab_request(&u("rsapp://tab/other")), None);
        assert_eq!(tab_request(&u("rsapp://other/map")), None);
        assert_eq!(tab_request(&u("https://tab/map")), None);
        assert!(is_tab_bar(&u("tauri://localhost/tabs.html")));
        assert!(!is_tab_bar(&u("tauri://localhost/index.html")));
        assert_eq!(from_tab_bar(&u("tauri://localhost/tabs.html")), Route::Stay);
        assert_eq!(from_tab_bar(&u("https://railspotting.com/")), Route::Browser);
        assert_eq!(from_tab_bar(&u("tauri://localhost/index.html")), Route::Drop);
        assert_eq!(Tab::Map.name(), "map");
    }

    #[test]
    fn live_map_url_carries_the_device_id() {
        assert_eq!(live_map_url(None).as_str(), "https://railspotting.com/embed/map");
        assert_eq!(live_map_url(Some("")).as_str(), "https://railspotting.com/embed/map");
        assert_eq!(live_map_url(Some("rs-0a1b2c3d4e5f")).as_str(), "https://railspotting.com/embed/map?receiver=rs-0a1b2c3d4e5f");
        assert_eq!(live_map_url(Some("a&b=c")).query(), Some("receiver=a%26b%3Dc"));
    }

    #[test]
    fn dashboard_keeps_its_own_page_only() {
        assert_eq!(from_dashboard(&u("http://127.0.0.1:5000/"), Some(5000)), Route::Stay);
        assert_eq!(from_dashboard(&u("http://127.0.0.1:5001/"), Some(5000)), Route::Browser);
        assert_eq!(from_dashboard(&u("http://127.0.0.1:5000/"), None), Route::Browser);
        assert_eq!(from_dashboard(&u("tauri://localhost/index.html"), None), Route::Stay);
        assert_eq!(from_dashboard(&u("https://railspotting.com/"), Some(5000)), Route::LiveMap);
        assert_eq!(from_dashboard(&u("https://railspotting.com/embed/map"), Some(5000)), Route::LiveMap);
        // anything else on the site (such as the link code page) opens in the browser
        assert_eq!(from_dashboard(&u("https://railspotting.com/link"), Some(5000)), Route::Browser);
        assert_eq!(from_dashboard(&u("https://railspotting.com/login?next=%2Flink"), Some(5000)), Route::Browser);
        assert_eq!(from_dashboard(&u("https://example.com/"), Some(5000)), Route::Browser);
        assert_eq!(from_dashboard(&u("file:///etc/passwd"), Some(5000)), Route::Drop);
    }

    #[test]
    fn downloads_go_to_the_downloads_folder_from_the_dashboard_only() {
        use std::path::{Path, PathBuf};
        let dl = Path::new("/Users/x/Downloads");
        let none = |_: &Path| false;
        let rec = u("http://127.0.0.1:5000/recordings/2026-10-25/a_voice.wav?download=1");
        assert_eq!(download_path(&rec, Some(5000), Path::new("2026-10-25_a_voice.wav"), dl, &none),
                   Some(PathBuf::from("/Users/x/Downloads/2026-10-25_a_voice.wav")));
        // no suggested name: the address's last part
        assert_eq!(download_path(&rec, Some(5000), Path::new(""), dl, &none), Some(PathBuf::from("/Users/x/Downloads/a_voice.wav")));
        // taken names get a number, as in a browser
        let taken = |p: &Path| p.ends_with("a_voice.wav") || p.ends_with("a_voice (2).wav");
        assert_eq!(download_path(&rec, Some(5000), Path::new("a_voice.wav"), dl, &taken),
                   Some(PathBuf::from("/Users/x/Downloads/a_voice (3).wav")));
        // a suggested name can't climb out of the folder
        assert_eq!(download_path(&rec, Some(5000), Path::new("../../.ssh/x"), dl, &none), Some(PathBuf::from("/Users/x/Downloads/x")));
        assert_eq!(download_path(&rec, Some(5000), Path::new("..."), dl, &none), Some(PathBuf::from("/Users/x/Downloads/a_voice.wav")));
        // anywhere else: not saved
        assert_eq!(download_path(&rec, Some(5001), Path::new("a.wav"), dl, &none), None);
        assert_eq!(download_path(&u("https://example.com/a.wav"), Some(5000), Path::new("a.wav"), dl, &none), None);
    }

    #[test]
    fn live_map_window_loads_the_page_and_its_video_frames_only() {
        for p in ["https://railspotting.com/embed/map", "https://railspotting.com/embed/map?receiver=rs-1&t=2#x",
                  "https://www.youtube-nocookie.com/embed/abc123?start=4", "about:blank"] {
            assert_eq!(from_live_map(&u(p)), Route::Stay, "{p}");
        }
        for p in ["https://railspotting.com/", "https://railspotting.com/embed/mapx", "https://railspotting.com/embed/",
                  "https://www.railspotting.com/embed/map", "http://railspotting.com/embed/map",
                  "https://railspotting.com:8443/embed/map", "https://railspotting.com/trains",
                  "https://railspotting.com/login?next=%2F", "https://accounts.google.com/o/oauth2/auth",
                  "https://www.youtube.com/watch?v=abc123", "https://www.youtube-nocookie.com/watch?v=abc",
                  "https://railspotting.com.evil.example/embed/map", "https://tile.openstreetmap.org/1/0/0.png"] {
            assert_eq!(from_live_map(&u(p)), Route::Browser, "{p}");
        }
        assert_eq!(from_live_map(&u("mailto:x@example.com")), Route::Drop);
        // links on the page open new tabs: those always go to the browser
        assert_eq!(new_window_from_live_map(&u("https://railspotting.com/embed/map")), Route::Browser);
        assert_eq!(new_window_from_live_map(&u("https://railspotting.com/trains/123")), Route::Browser);
    }

    #[test]
    fn location_only_for_the_site() {
        assert!(shares_location("https", "railspotting.com", 0));
        assert!(shares_location("https", "railspotting.com", 443));
        assert!(!shares_location("http", "railspotting.com", 0));
        assert!(!shares_location("https", "railspotting.com", 8443));
        assert!(!shares_location("https", "www.railspotting.com", 0));
        assert!(!shares_location("https", "railspotting.com.evil.example", 0));
        assert!(!shares_location("http", "127.0.0.1", 5000));
    }
}
