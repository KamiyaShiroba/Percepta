//! Finds newer releases on GitHub and installs them. Requests go through WinHTTP so
//! that they follow the system proxy, which GitHub often can't be reached without.

use crate::i18n::tr;
use std::ffi::c_void;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::{null, null_mut};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use eframe::egui;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use windows_sys::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpCrackUrl, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    URL_COMPONENTS, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE,
    WINHTTP_QUERY_CONTENT_LENGTH, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
};

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/KamiyaShiroba/Percepta/releases/latest";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const USER_AGENT: &str = concat!("Percepta/", env!("CARGO_PKG_VERSION"));
const HTTP_OK: u32 = 200;
const HTTP_NOT_FOUND: u32 = 404;
const READ_CHUNK: usize = 64 * 1024;
/// Setup's own window shows the progress, and it starts Percepta again when done.
const INSTALLER_ARGS: [&str; 4] = ["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/CLOSEAPPLICATIONS"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub page: String,
    installer_name: String,
    installer_url: String,
    sha256: String,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    html_url: String,
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

fn parse_release(json: &str) -> Result<Release, String> {
    let api: ApiRelease = serde_json::from_str(json).map_err(|e| tr!("update.error.release", e))?;
    let version = Version::parse(api.tag_name.trim_start_matches('v'))
        .map_err(|e| tr!("update.error.version", api.tag_name, e))?;
    let installer_name = format!("Percepta-Setup-{version}.exe");
    let asset = api
        .assets
        .into_iter()
        .find(|asset| asset.name == installer_name)
        .ok_or_else(|| tr!("update.error.no_installer", version))?;
    // Never run a download that can't be checked.
    let sha256 = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .ok_or_else(|| tr!("update.error.no_checksum", version))?
        .to_owned();
    Ok(Release { version, page: api.html_url, installer_name, installer_url: asset.browser_download_url, sha256 })
}

fn is_newer(release: &Version) -> bool {
    Version::parse(CURRENT_VERSION).is_ok_and(|current| *release > current)
}

/// `None` when the repository has no published release yet.
fn latest_release() -> Result<Option<Release>, String> {
    let mut body = Vec::new();
    let status = http_get(LATEST_RELEASE_URL, |chunk, _| {
        body.extend_from_slice(chunk);
        Ok(())
    })?;
    match status {
        HTTP_OK => parse_release(&String::from_utf8_lossy(&body)).map(Some),
        HTTP_NOT_FOUND => Ok(None),
        status => Err(tr!("update.error.github", status)),
    }
}

/// Saves the file to `path`, or to nowhere at all if it doesn't match `sha256`.
fn download(url: &str, sha256: &str, path: &Path, mut progress: impl FnMut(f32)) -> Result<(), String> {
    let mut file = File::create(path).map_err(|e| tr!("update.error.create_file", e))?;
    let mut hash = Sha256::new();
    let mut received = 0u64;
    let status = http_get(url, |chunk, total| {
        file.write_all(chunk).map_err(|e| tr!("update.error.write_file", e))?;
        hash.update(chunk);
        received += chunk.len() as u64;
        if let Some(total) = total.filter(|&total| total > 0) {
            progress(received as f32 / total as f32);
        }
        Ok(())
    });
    drop(file);
    let result = match status {
        Ok(HTTP_OK) if to_hex(&hash.finalize()).eq_ignore_ascii_case(sha256) => Ok(()),
        Ok(HTTP_OK) => Err(tr!("update.error.checksum").to_owned()),
        Ok(status) => Err(tr!("update.error.download", status)),
        Err(reason) => Err(reason),
    };
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn download_dir() -> PathBuf {
    std::env::temp_dir().join("Percepta-update")
}

fn download_and_run(release: &Release, progress: impl FnMut(f32)) -> Result<(), String> {
    let dir = download_dir();
    fs::create_dir_all(&dir).map_err(|e| tr!("update.error.folder", e))?;
    let path = dir.join(&release.installer_name);
    download(&release.installer_url, &release.sha256, &path, progress)?;
    Command::new(&path).args(INSTALLER_ARGS).spawn().map_err(|e| tr!("update.error.start_installer", e))?;
    Ok(())
}

pub enum State {
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Downloading(f32),
    /// The installer is running, and this app must get out of its way.
    Installing,
    Failed(String),
}

enum Message {
    Checked(Result<Option<Release>, String>),
    Progress(f32),
    Launched(Result<(), String>),
}

pub struct Updater {
    pub state: State,
    messages: Receiver<Message>,
    sender: Sender<Message>,
}

impl Updater {
    pub fn new() -> Self {
        // What the last update left behind. Setup can't delete itself.
        let _ = fs::remove_dir_all(download_dir());
        let (sender, messages) = mpsc::channel();
        Self { state: State::Idle, messages, sender }
    }

    pub fn check(&mut self, ctx: &egui::Context) {
        self.state = State::Checking;
        let sender = self.sender.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = sender.send(Message::Checked(latest_release()));
            ctx.request_repaint();
        });
    }

    pub fn install(&mut self, ctx: &egui::Context, release: Release) {
        self.state = State::Downloading(0.0);
        let sender = self.sender.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let result = download_and_run(&release, |fraction| {
                let _ = sender.send(Message::Progress(fraction));
                ctx.request_repaint();
            });
            let _ = sender.send(Message::Launched(result));
            ctx.request_repaint();
        });
    }

    pub fn poll(&mut self) {
        while let Ok(message) = self.messages.try_recv() {
            self.state = match message {
                Message::Checked(Ok(Some(release))) if is_newer(&release.version) => State::Available(release),
                Message::Checked(Ok(_)) => State::UpToDate,
                Message::Progress(fraction) => State::Downloading(fraction),
                Message::Launched(Ok(())) => State::Installing,
                Message::Checked(Err(reason)) | Message::Launched(Err(reason)) => State::Failed(reason),
            };
        }
    }
}

struct Handle(*mut c_void);

impl Handle {
    fn new(raw: *mut c_void, what: &str) -> Result<Self, String> {
        if raw.is_null() {
            Err(last_error(what))
        } else {
            Ok(Self(raw))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from WinHTTP and is closed only here.
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

fn last_error(what: &str) -> String {
    tr!("update.error.step", what, std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

/// Returns the final status code, following redirects. The body is only read after a
/// 200. `sink` gets each piece of it, along with the total size if the server gave one.
fn http_get(
    url: &str,
    mut sink: impl FnMut(&[u8], Option<u64>) -> Result<(), String>,
) -> Result<u32, String> {
    let url_wide = wide(url);
    // SAFETY: an all-zero URL_COMPONENTS is valid, and with these lengths WinHTTP only
    // points its fields into `url_wide`, which outlives them.
    let mut parts: URL_COMPONENTS = unsafe { std::mem::zeroed() };
    parts.dwStructSize = size_of::<URL_COMPONENTS>() as u32;
    parts.dwHostNameLength = u32::MAX;
    parts.dwUrlPathLength = u32::MAX;
    if unsafe { WinHttpCrackUrl(url_wide.as_ptr(), 0, 0, &mut parts) } == 0 {
        return Err(last_error(tr!("update.step.parse")));
    }
    // SAFETY: as above, the pointer and length describe part of `url_wide`.
    let host = unsafe { std::slice::from_raw_parts(parts.lpszHostName, parts.dwHostNameLength as usize) };
    let host: Vec<u16> = host.iter().copied().chain([0]).collect();
    // The path runs on through the query to the end of the URL, which is what's wanted.
    let path = parts.lpszUrlPath;
    let flags = if url.starts_with("https:") { WINHTTP_FLAG_SECURE } else { 0 };

    let agent = wide(USER_AGENT);
    let verb = wide("GET");
    // SAFETY: every string is NUL-terminated and outlives the call, the handles are valid
    // for as long as their `Handle` lives, and the out-pointers are valid locals.
    unsafe {
        let session = Handle::new(
            WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, null(), null(), 0),
            tr!("update.step.open"),
        )?;
        let connection = Handle::new(WinHttpConnect(session.0, host.as_ptr(), parts.nPort, 0), tr!("connect.connect"))?;
        let request = Handle::new(
            WinHttpOpenRequest(connection.0, verb.as_ptr(), path, null(), null(), null(), flags),
            tr!("update.step.request"),
        )?;
        if WinHttpSendRequest(request.0, null(), 0, null(), 0, 0, 0) == 0 {
            return Err(last_error(tr!("update.step.send")));
        }
        if WinHttpReceiveResponse(request.0, null_mut()) == 0 {
            return Err(last_error(tr!("update.step.receive")));
        }
        let status = query_number(request.0, WINHTTP_QUERY_STATUS_CODE).ok_or(tr!("update.error.no_status"))?;
        if status != HTTP_OK {
            return Ok(status);
        }
        let total = query_number(request.0, WINHTTP_QUERY_CONTENT_LENGTH).map(u64::from);
        let mut buffer = vec![0u8; READ_CHUNK];
        loop {
            let mut read = 0u32;
            if WinHttpReadData(request.0, buffer.as_mut_ptr().cast(), READ_CHUNK as u32, &mut read) == 0 {
                return Err(last_error(tr!("update.step.read")));
            }
            if read == 0 {
                return Ok(status);
            }
            sink(&buffer[..read as usize], total)?;
        }
    }
}

/// # Safety
/// `request` must be a request handle whose response has been received.
unsafe fn query_number(request: *mut c_void, header: u32) -> Option<u32> {
    let mut value = 0u32;
    let mut length = size_of::<u32>() as u32;
    let found = WinHttpQueryHeaders(
        request,
        header | WINHTTP_QUERY_FLAG_NUMBER,
        null(),
        (&raw mut value).cast(),
        &mut length,
        null_mut(),
    );
    (found != 0).then_some(value)
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::net::TcpListener;

    use super::*;

    const RELEASE_JSON: &str = r#"{
        "tag_name": "v1.2.3",
        "html_url": "https://example.com/page",
        "assets": [
            {"name": "other.zip", "browser_download_url": "https://example.com/other", "digest": "sha256:00"},
            {"name": "Percepta-Setup-1.2.3.exe", "browser_download_url": "https://example.com/setup", "digest": "sha256:abc"}
        ]
    }"#;

    /// Answers one request with `response`, and returns the URL to ask for.
    fn serve_once(response: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            stream.write_all(&response).unwrap();
        });
        format!("http://127.0.0.1:{port}/file?x=1")
    }

    fn ok_response(body: &[u8]) -> Vec<u8> {
        let mut response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
        response.extend_from_slice(body);
        response
    }

    fn scratch_file(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("percepta-test-{}-{name}", std::process::id()))
    }

    #[test]
    fn a_release_is_read_with_its_installer_and_checksum() {
        let release = parse_release(RELEASE_JSON).unwrap();
        assert_eq!(release.version, Version::new(1, 2, 3));
        assert_eq!(release.installer_url, "https://example.com/setup");
        assert_eq!(release.sha256, "abc");
    }

    #[test]
    fn a_release_without_a_checkable_installer_is_refused() {
        assert!(parse_release(&RELEASE_JSON.replace("sha256:abc", "md5:abc")).is_err());
        assert!(parse_release(&RELEASE_JSON.replace("Percepta-Setup-1.2.3.exe", "nope.exe")).is_err());
    }

    #[test]
    fn a_final_release_is_newer_than_its_own_alpha() {
        assert!(Version::parse("1.0.0").unwrap() > Version::parse("1.0.0-alpha").unwrap());
        assert!(!is_newer(&Version::parse("0.9.0").unwrap()));
    }

    #[test]
    fn a_download_is_kept_only_if_its_checksum_matches() {
        let body = b"installer bytes";
        let sha256 = to_hex(&Sha256::digest(body));

        let good = scratch_file("good");
        let mut fractions = Vec::new();
        download(&serve_once(ok_response(body)), &sha256, &good, |f| fractions.push(f)).unwrap();
        assert_eq!(fs::read(&good).unwrap(), body);
        assert_eq!(fractions.last(), Some(&1.0));
        fs::remove_file(&good).unwrap();

        let bad = scratch_file("bad");
        let wrong = "0".repeat(64);
        assert!(download(&serve_once(ok_response(body)), &wrong, &bad, |_| {}).is_err());
        assert!(!bad.exists());
    }

    #[test]
    fn other_statuses_are_reported_without_reading_a_body() {
        let url = serve_once(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec());
        let status = http_get(&url, |_, _| panic!("no body expected")).unwrap();
        assert_eq!(status, HTTP_NOT_FOUND);
    }

    #[test]
    #[ignore = "needs the network"]
    fn live_check_reaches_github() {
        let release = latest_release().unwrap();
        println!("{release:?}");
    }
}
