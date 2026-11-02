//! Blocking request/reply connection to CSP.

use crate::i18n::tr;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::auth::{self, Pairing};
use super::frame::{self, Frame, TYPE_COMMAND, TYPE_ERROR, TYPE_SUCCESS};
use super::{Rgb, Slot};
use crate::color::{hls_to_rgb, hsv_to_rgb, rgb_to_hls, rgb_to_hsv, Channels};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Long enough to ride out CSP being busy, such as while saving a large file.
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);
const SERVER_UNREADY_RETRIES: u32 = 5;
/// Colour channels on the wire run from 0 to this, whatever they measure.
const CHANNEL_SCALE: f64 = u32::MAX as f64;

#[derive(Debug)]
pub enum Error {
    /// No address from the pairing accepted a connection.
    Unreachable,
    /// CSP rejected the password; only a fresh QR can fix this.
    PasswordMismatch,
    /// CSP speaks a protocol generation we did not offer.
    VersionMismatch,
    /// CSP is still starting up, or showing a dialog.
    ServerUnready,
    AuthRejected(String),
    Timeout,
    Closed,
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unreachable => f.write_str(tr!("error.unreachable")),
            Error::PasswordMismatch => f.write_str(tr!("error.password")),
            Error::VersionMismatch => f.write_str(tr!("error.version")),
            Error::ServerUnready => f.write_str(tr!("error.unready")),
            Error::AuthRejected(reason) => f.write_str(&tr!("error.auth", reason)),
            Error::Timeout => f.write_str(tr!("error.timeout")),
            Error::Closed => f.write_str(tr!("error.closed")),
            Error::Io(e) => f.write_str(&tr!("error.network", e)),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// Which channels CSP's colour wheel works in. It only reports and accepts slot
/// values in this form, so the fields of the other form are simply absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSpace {
    Hsv,
    Hls,
}

/// A slot's colour, with CSP's own numbers kept so that selecting the slot can write
/// them back exactly instead of a rounded version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotColor {
    pub rgb: Rgb,
    space: ColorSpace,
    raw: [u64; 3],
}

/// CSP's colour slots, as reported by `SyncColorCircleUIState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CspSlots {
    pub active: Slot,
    /// While transparent is active, the slot a colour edit should return to.
    pub previous: Slot,
    pub main: Option<SlotColor>,
    pub sub: Option<SlotColor>,
    pub space: ColorSpace,
    /// False mid-stroke or while a modal dialog is open; setters are refused then.
    pub can_set: bool,
}

impl CspSlots {
    /// CSP may leave out what has not changed, so fill the gaps from the last reply.
    pub fn merged_over(mut self, older: &CspSlots) -> Self {
        self.main = self.main.or(older.main);
        self.sub = self.sub.or(older.sub);
        self
    }

    fn color(&self, slot: Slot) -> Option<SlotColor> {
        match slot {
            Slot::Main => self.main,
            Slot::Sub => self.sub,
            Slot::Transparent => None,
        }
    }
}

pub struct Client {
    stream: TcpStream,
    buf: Vec<u8>,
    serial: u64,
    /// Protocol generation reported by CSP at login.
    pub server_generation: String,
}

impl Client {
    /// Connects and logs in. Returns the client and the pairing to save for
    /// later reconnects, which holds the address used and the current password.
    pub fn connect(pairing: &Pairing, reconnect: bool) -> Result<(Self, Pairing), Error> {
        let mut last_error = Error::Unreachable;
        for attempt in 0..=SERVER_UNREADY_RETRIES {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(1));
            }
            let (stream, ip) = open(pairing)?;
            let mut client = Client {
                stream,
                buf: Vec::new(),
                serial: 0,
                server_generation: String::new(),
            };
            match client.login(pairing, reconnect) {
                Ok(password) => {
                    let saved = Pairing {
                        ips: vec![ip],
                        port: pairing.port,
                        password,
                        generation: pairing.generation.clone(),
                    };
                    return Ok((client, saved));
                }
                Err(Error::ServerUnready) => last_error = Error::ServerUnready,
                Err(e) => return Err(e),
            }
        }
        Err(last_error)
    }

    fn login(&mut self, pairing: &Pairing, reconnect: bool) -> Result<String, Error> {
        let (detail, password) = if reconnect {
            (auth::reconnect_detail(pairing), pairing.password.clone())
        } else {
            let password = auth::new_password();
            (auth::first_login_detail(pairing, &password), password)
        };
        let reply = self.request("Authenticate", &detail)?;
        let body: Value = serde_json::from_str(&reply.detail).unwrap_or(Value::Null);

        // CSP's own client trusts this field, not the frame type.
        match body["AuthErrorReason"].as_str() {
            Some("") | Some("Unknown") => {}
            Some("PasswordMismatch") => return Err(Error::PasswordMismatch),
            Some("VersionMismatch") => return Err(Error::VersionMismatch),
            Some("ServerUnready") => return Err(Error::ServerUnready),
            Some(other) => return Err(Error::AuthRejected(other.to_string())),
            None if reply.kind == TYPE_SUCCESS => {}
            None => return Err(Error::AuthRejected(reply.detail)),
        }
        self.server_generation = body["RemoteCommandSpecVersionOfServer"]
            .as_str()
            .unwrap_or("?")
            .to_string();
        Ok(password)
    }

    /// Writes the active colour slot. Returns whether CSP accepted it.
    pub fn set_rgb(&mut self, rgb: Rgb) -> Result<bool, Error> {
        // CSP recognises the RGB form by the absence of ColorSpaceKind and ColorIndex.
        let detail = json!({
            "IsColorTransparent": false,
            "RGBColorR": rgb[0],
            "RGBColorG": rgb[1],
            "RGBColorB": rgb[2],
        });
        let reply = self.request("SetCurrentColor", &detail.to_string())?;
        Ok(reply.kind != TYPE_ERROR)
    }

    /// Writes a colour into one slot and makes that slot active. This is how a colour
    /// edit leaves the transparent selection for the slot of the user's choosing.
    pub fn set_rgb_in_slot(&mut self, space: ColorSpace, rgb: Rgb, slot: Slot) -> Result<bool, Error> {
        let Some(index) = slot_index(slot) else {
            return Ok(false);
        };
        self.write_slot(space, rgb_to_raw(space, rgb), index)
    }

    /// Makes a slot active without changing any colour. Returns false if CSP refused,
    /// or if the slot's current value is not known: the only way to select a slot is
    /// to write a value to it, so guessing would overwrite the user's colour.
    pub fn select_slot(&mut self, slots: &CspSlots, slot: Slot) -> Result<bool, Error> {
        let Some(index) = slot_index(slot) else {
            let detail = json!({
                "IsColorTransparent": true,
                "RGBColorR": 0,
                "RGBColorG": 0,
                "RGBColorB": 0,
            });
            let reply = self.request("SetCurrentColor", &detail.to_string())?;
            return Ok(reply.kind != TYPE_ERROR);
        };
        match slots.color(slot) {
            Some(color) => self.write_slot(color.space, color.raw, index),
            None => Ok(false),
        }
    }

    fn write_slot(&mut self, space: ColorSpace, raw: [u64; 3], index: u8) -> Result<bool, Error> {
        let detail = match space {
            ColorSpace::Hsv => json!({
                "ColorSpaceKind": "HSV",
                "IsColorTransparent": false,
                "HSVColorH": raw[0],
                "HSVColorS": raw[1],
                "HSVColorV": raw[2],
                "ColorIndex": index,
            }),
            ColorSpace::Hls => json!({
                "ColorSpaceKind": "HLS",
                "IsColorTransparent": false,
                "HLSColorH": raw[0],
                "HLSColorL": raw[1],
                "HLSColorS": raw[2],
                "ColorIndex": index,
            }),
        };
        let reply = self.request("SetCurrentColor", &detail.to_string())?;
        Ok(reply.kind != TYPE_ERROR)
    }

    /// `None` when the reply says nothing, which happens while CSP considers us idle.
    pub fn read_slots(&mut self) -> Result<Option<CspSlots>, Error> {
        let reply = self.request("SyncColorCircleUIState", "")?;
        Ok(parse_slots(&reply.detail))
    }

    pub fn heartbeat(&mut self) -> Result<(), Error> {
        self.request("TellHeartbeat", "").map(drop)
    }

    /// Clears the idle state in which some CSP versions answer polls with `{}`.
    /// This also resets the timer behind CSP's worktime tracker, so it is only
    /// sent when a poll actually came back empty.
    pub fn wake(&mut self) -> Result<(), Error> {
        self.request("TellHeartbeat", r#"{"IdleTimerResetRequested":true}"#)
            .map(drop)
    }

    fn request(&mut self, command: &str, detail: &str) -> Result<Frame, Error> {
        let serial = self.serial;
        self.serial += 1;
        self.stream
            .write_all(&frame::encode(TYPE_COMMAND, command, serial, detail))?;

        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            while let Some(frame) = frame::next_frame(&mut self.buf) {
                match frame.kind {
                    // Commands CSP starts must be acknowledged or it stalls.
                    TYPE_COMMAND => self.stream.write_all(&frame::encode(
                        TYPE_SUCCESS,
                        &frame.command,
                        frame.serial,
                        "",
                    ))?,
                    _ if frame.serial == serial => return Ok(frame),
                    _ => {}
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::Timeout);
            }
            self.stream.set_read_timeout(Some(remaining))?;
            let mut chunk = [0u8; 8192];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(Error::Closed),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// Tries loopback first: a QR address can be stale after a network change.
fn open(pairing: &Pairing) -> Result<(TcpStream, String), Error> {
    let loopback = "127.0.0.1".to_string();
    for ip in std::iter::once(&loopback).chain(&pairing.ips) {
        let Ok(ip_addr) = ip.parse() else { continue };
        let addr = SocketAddr::new(ip_addr, pairing.port);
        if let Ok(stream) = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            stream.set_nodelay(true)?;
            return Ok((stream, ip.clone()));
        }
    }
    Err(Error::Unreachable)
}

fn slot_index(slot: Slot) -> Option<u8> {
    match slot {
        Slot::Main => Some(0),
        Slot::Sub => Some(1),
        Slot::Transparent => None,
    }
}

fn rgb_to_raw(space: ColorSpace, rgb: Rgb) -> [u64; 3] {
    let rgb = rgb.map(|byte| byte as f64 / 255.0);
    let coords = match space {
        ColorSpace::Hsv => rgb_to_hsv(rgb),
        ColorSpace::Hls => rgb_to_hls(rgb),
    };
    let scale = |value: f64| (value * CHANNEL_SCALE).round() as u64;
    [scale(coords[0] / 360.0), scale(coords[1]), scale(coords[2])]
}

fn raw_to_rgb(space: ColorSpace, raw: [u64; 3]) -> Rgb {
    let channel = |value: u64| value as f64 / CHANNEL_SCALE;
    let coords: Channels = [channel(raw[0]) * 360.0, channel(raw[1]), channel(raw[2])];
    let rgb = match space {
        ColorSpace::Hsv => hsv_to_rgb(coords),
        ColorSpace::Hls => hls_to_rgb(coords),
    };
    rgb.map(|c| (c * 255.0).round().clamp(0.0, 255.0) as u8)
}

fn parse_slots(detail: &str) -> Option<CspSlots> {
    let body: Value = serde_json::from_str(detail).ok()?;
    let index = body["CurrentColorIndex"].as_i64()?;
    let space = match body["ColorSelectionModel"].as_str() {
        Some("HSV") => ColorSpace::Hsv,
        _ => ColorSpace::Hls,
    };
    let slot_color = |slot: &str| {
        let [prefix, first, second, third] = match space {
            ColorSpace::Hsv => ["HSV", "H", "S", "V"],
            ColorSpace::Hls => ["HLS", "H", "L", "S"],
        };
        let field = |channel: &str| body[format!("{prefix}Color{slot}{channel}")].as_u64();
        let raw = [field(first)?, field(second)?, field(third)?];
        Some(SlotColor { rgb: raw_to_rgb(space, raw), space, raw })
    };
    let transparent = body["IsCurrentColorTransparent"].as_bool().unwrap_or(false);
    Some(CspSlots {
        active: match index {
            _ if transparent => Slot::Transparent,
            0 => Slot::Main,
            1 => Slot::Sub,
            _ => Slot::Transparent,
        },
        previous: if body["PrevColorIndex"].as_i64() == Some(1) { Slot::Sub } else { Slot::Main },
        main: slot_color("Main"),
        sub: slot_color("Sub"),
        space,
        can_set: body["CanExecuteSetterCommand"].as_bool().unwrap_or(true),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    const MAIN: Rgb = [10, 20, 30];
    const SUB: Rgb = [200, 100, 50];

    /// Minimal stand-in for CSP in HLS mode: one connection, one login, its colour slots.
    fn fake_csp(auth_reason: &'static str) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut seen = Vec::new();
            let mut slots = [rgb_to_raw(ColorSpace::Hls, MAIN), rgb_to_raw(ColorSpace::Hls, SUB)];
            let (mut active, mut previous) = (0i64, 1i64);
            let mut chunk = [0u8; 4096];
            loop {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    return seen;
                }
                buf.extend_from_slice(&chunk[..n]);
                while let Some(f) = frame::next_frame(&mut buf) {
                    if f.kind != TYPE_COMMAND {
                        seen.push(format!("ack:{}", f.command));
                        continue;
                    }
                    seen.push(f.command.clone());
                    let reply = match f.command.as_str() {
                        "Authenticate" => {
                            // A server-initiated command, to check that it gets acknowledged.
                            stream
                                .write_all(&frame::encode(TYPE_COMMAND, "SyncColorMixUIState", 900, ""))
                                .unwrap();
                            json!({
                                "AuthErrorReason": auth_reason,
                                "RemoteCommandSpecVersionOfServer": "G#1:2022.12",
                            })
                            .to_string()
                        }
                        "SetCurrentColor" => {
                            let body: Value = serde_json::from_str(&f.detail).unwrap();
                            if body["IsColorTransparent"] == true {
                                if active >= 0 {
                                    previous = active;
                                }
                                active = -1;
                            } else if body.get("ColorSpaceKind").is_some() {
                                assert_eq!(body["ColorSpaceKind"], "HLS");
                                // CSP treats a missing channel as zero, destroying the colour.
                                let channel = |name: &str| body[name].as_u64().expect(name);
                                active = body["ColorIndex"].as_i64().unwrap();
                                slots[active as usize] =
                                    [channel("HLSColorH"), channel("HLSColorL"), channel("HLSColorS")];
                            } else {
                                assert!(active >= 0, "this test expects a slot to be named");
                                let byte = |name: &str| body[name].as_u64().unwrap() as u8;
                                let rgb = [byte("RGBColorR"), byte("RGBColorG"), byte("RGBColorB")];
                                slots[active as usize] = rgb_to_raw(ColorSpace::Hls, rgb);
                            }
                            String::new()
                        }
                        "SyncColorCircleUIState" => json!({
                            "ColorSelectionModel": "HLS",
                            "CurrentColorIndex": active,
                            "PrevColorIndex": previous,
                            "IsCurrentColorTransparent": active < 0,
                            "CanExecuteSetterCommand": true,
                            "HLSColorMainH": slots[0][0],
                            "HLSColorMainL": slots[0][1],
                            "HLSColorMainS": slots[0][2],
                            "HLSColorSubH": slots[1][0],
                            "HLSColorSubL": slots[1][1],
                            "HLSColorSubS": slots[1][2],
                        })
                        .to_string(),
                        _ => String::new(),
                    };
                    stream
                        .write_all(&frame::encode(TYPE_SUCCESS, &f.command, f.serial, &reply))
                        .unwrap();
                }
            }
        });
        (port, handle)
    }

    fn pairing(port: u16) -> Pairing {
        Pairing {
            ips: vec![],
            port,
            password: "qr-password".into(),
            generation: "G#1:2022.12".into(),
        }
    }

    #[test]
    fn logs_in_writes_and_reads_back() {
        let (port, host) = fake_csp("Unknown");
        let (mut client, saved) = Client::connect(&pairing(port), false).unwrap();
        assert_eq!(client.server_generation, "G#1:2022.12");
        assert_ne!(saved.password, "qr-password");
        assert_eq!(saved.ips, ["127.0.0.1"]);

        let slots = client.read_slots().unwrap().unwrap();
        assert_eq!(slots.main.unwrap().rgb, MAIN);
        assert!(client.set_rgb([18, 52, 86]).unwrap());
        let slots = client.read_slots().unwrap().unwrap();
        assert_eq!(slots.main.unwrap().rgb, [18, 52, 86]);
        assert_eq!(slots.sub.unwrap().rgb, SUB);
        client.heartbeat().unwrap();
        drop(client);

        let seen = host.join().unwrap();
        assert!(seen.iter().any(|c| c == "ack:SyncColorMixUIState"), "{seen:?}");
    }

    #[test]
    fn reports_a_stale_password() {
        let (port, _host) = fake_csp("PasswordMismatch");
        assert!(matches!(
            Client::connect(&pairing(port), true),
            Err(Error::PasswordMismatch)
        ));
    }

    #[test]
    fn selecting_a_slot_keeps_every_colour() {
        let (port, _host) = fake_csp("Unknown");
        let (mut client, _) = Client::connect(&pairing(port), false).unwrap();
        let slots = client.read_slots().unwrap().unwrap();

        assert!(client.select_slot(&slots, Slot::Sub).unwrap());
        let after = client.read_slots().unwrap().unwrap();
        assert_eq!(after.active, Slot::Sub);
        assert_eq!(after.main, slots.main);
        assert_eq!(after.sub, slots.sub);

        assert!(client.select_slot(&after, Slot::Transparent).unwrap());
        let after = client.read_slots().unwrap().unwrap();
        assert_eq!(after.active, Slot::Transparent);
        assert_eq!(after.previous, Slot::Sub);
        assert_eq!(after.main, slots.main);
        assert_eq!(after.sub, slots.sub);
    }

    #[test]
    fn a_colour_edit_can_leave_transparent_for_a_chosen_slot() {
        let (port, _host) = fake_csp("Unknown");
        let (mut client, _) = Client::connect(&pairing(port), false).unwrap();
        let slots = client.read_slots().unwrap().unwrap();
        client.select_slot(&slots, Slot::Transparent).unwrap();

        assert!(client.set_rgb_in_slot(ColorSpace::Hls, [1, 2, 3], Slot::Sub).unwrap());
        let after = client.read_slots().unwrap().unwrap();
        assert_eq!(after.active, Slot::Sub);
        assert_eq!(after.sub.unwrap().rgb, [1, 2, 3]);
        assert_eq!(after.main, slots.main);
    }

    #[test]
    fn selecting_a_slot_of_unknown_value_writes_nothing() {
        let (port, host) = fake_csp("Unknown");
        let (mut client, _) = Client::connect(&pairing(port), false).unwrap();
        let mut slots = client.read_slots().unwrap().unwrap();
        slots.sub = None;

        assert!(!client.select_slot(&slots, Slot::Sub).unwrap());
        drop(client);
        assert!(!host.join().unwrap().iter().any(|c| c == "SetCurrentColor"));
    }

    #[test]
    fn parses_hls_and_hsv_replies_and_ignores_the_absent_form() {
        // Recorded from a real CSP in HLS mode, with the foreground at H 24 L 62 S 66.
        let hls = r#"{"CanExecuteSetterCommand":true,"HLSColorSubH":0,"HLSColorMainH":288952640,"HLSColorSubL":0,"HLSColorMainL":2652279807,"HLSColorSubS":0,"ColorSelectionModel":"HLS","HLSColorMainS":2854193151,"CurrentColorIndex":-1,"IsCurrentColorTransparent":true,"PrevColorIndex":0}"#;
        let slots = parse_slots(hls).unwrap();
        assert_eq!(slots.space, ColorSpace::Hls);
        assert_eq!(slots.active, Slot::Transparent);
        assert_eq!(slots.previous, Slot::Main);
        assert_eq!(slots.main.unwrap().rgb, raw_to_rgb(ColorSpace::Hls, [288952640, 2652279807, 2854193151]));
        assert_eq!(slots.sub.unwrap().rgb, [0, 0, 0]);

        let hsv = r#"{"ColorSelectionModel":"HSV","CurrentColorIndex":1,"HSVColorMainH":0,"HSVColorMainS":4294967295,"HSVColorMainV":4294967295,"HSVColorSubH":0,"HSVColorSubS":0,"HSVColorSubV":4294967295}"#;
        let slots = parse_slots(hsv).unwrap();
        assert_eq!(slots.space, ColorSpace::Hsv);
        assert_eq!(slots.active, Slot::Sub);
        assert_eq!(slots.main.unwrap().rgb, [255, 0, 0]);
        assert_eq!(slots.sub.unwrap().rgb, [255, 255, 255]);
    }

    #[test]
    fn empty_and_partial_replies_are_handled() {
        assert_eq!(parse_slots("{}"), None);
        assert_eq!(parse_slots(""), None);

        let full = parse_slots(r#"{"ColorSelectionModel":"HSV","CurrentColorIndex":0,"HSVColorMainH":0,"HSVColorMainS":0,"HSVColorMainV":4294967295,"HSVColorSubH":0,"HSVColorSubS":0,"HSVColorSubV":0}"#).unwrap();
        let partial = parse_slots(r#"{"ColorSelectionModel":"HSV","CurrentColorIndex":1}"#).unwrap();
        assert_eq!(partial.main, None);
        let merged = partial.merged_over(&full);
        assert_eq!(merged.active, Slot::Sub);
        assert_eq!(merged.main, full.main);
        assert_eq!(merged.sub, full.sub);
    }

    #[test]
    fn every_byte_survives_the_wire_scale() {
        for space in [ColorSpace::Hsv, ColorSpace::Hls] {
            for value in (0..=255u8).step_by(5) {
                let rgb = [value, 255 - value, value / 2];
                assert_eq!(raw_to_rgb(space, rgb_to_raw(space, rgb)), rgb);
            }
        }
    }

    /// Read-only measurement against the real CSP, using the saved session:
    /// `cargo test live_read_timing -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a running CSP and a saved session"]
    fn live_read_timing() {
        let saved = crate::session::load().expect("no saved session");
        let (mut client, _) = Client::connect(&saved, true).unwrap();
        let mut times = Vec::new();
        let mut empty = 0;
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            let t = Instant::now();
            if client.read_slots().unwrap().is_none() {
                empty += 1;
            }
            times.push(t.elapsed());
        }
        times.sort();
        println!(
            "{} reads in 5 s, {empty} empty, median {:?}, p95 {:?}, max {:?}",
            times.len(),
            times[times.len() / 2],
            times[times.len() * 95 / 100],
            times[times.len() - 1],
        );
    }

    #[test]
    fn unreachable_when_nothing_listens() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(matches!(
            Client::connect(&pairing(port), false),
            Err(Error::Unreachable)
        ));
    }
}
