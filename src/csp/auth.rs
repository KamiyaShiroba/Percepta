//! Pairing QR decoding and login tokens.

use rand::RngCore;
use serde::{Deserialize, Serialize};

const QR_KEY: [u8; 7] = [0x74, 0xB2, 0x92, 0x5B, 0x4A, 0x21, 0xDA];
const AUTH_KEY: [u8; 7] = [0xB6, 0xD5, 0x92, 0xC4, 0xA7, 0x83, 0xE1];
const RECONNECT_MARKER: &str = "{{(([[reconnection request marker]]))}}\r\n";

/// Where and how to log in. For a QR pairing `password` is the one-time QR
/// password; for a saved session it is the password we registered with CSP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pairing {
    pub ips: Vec<String>,
    pub port: u16,
    pub password: String,
    pub generation: String,
}

fn xor(data: &[u8], key: &[u8]) -> Vec<u8> {
    data.iter()
        .zip(key.iter().cycle())
        .map(|(b, k)| b ^ k)
        .collect()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

fn token(secret: &str) -> String {
    to_hex(&xor(secret.as_bytes(), &AUTH_KEY))
}

/// Parses `https://companion.clip-studio.com/rc/<lang>?s=<hex>`. Regional builds
/// may use another host, so the payload itself is what gets validated.
pub fn parse_qr_url(url: &str) -> Option<Pairing> {
    let (_, query) = url.trim().split_once('?')?;
    let payload = query.split('&').find_map(|pair| pair.strip_prefix("s="))?;
    let plain = String::from_utf8(xor(&from_hex(payload)?, &QR_KEY)).ok()?;

    let mut fields = plain.split('\t');
    let ips = fields.next()?.split(',').map(String::from).collect();
    let port = fields.next()?.parse().ok()?;
    let password = fields.next()?.to_string();
    let generation = fields.next()?.to_string();
    Some(Pairing { ips, port, password, generation })
}

pub fn new_password() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut seed = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut seed);
    seed.iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect()
}

/// Detail of the `Authenticate` command for a first login with the QR password.
pub fn first_login_detail(pairing: &Pairing, new_password: &str) -> String {
    serde_json::json!([pairing.generation, token(&pairing.password), token(new_password)])
        .to_string()
}

/// Detail of the `Authenticate` command for a login with a saved password.
pub fn reconnect_detail(pairing: &Pairing) -> String {
    serde_json::json!([pairing.generation, token(RECONNECT_MARKER), token(&pairing.password)])
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qr_url(plain: &str) -> String {
        format!(
            "https://companion.clip-studio.com/rc/zh-cn?s={}",
            to_hex(&xor(plain.as_bytes(), &QR_KEY))
        )
    }

    #[test]
    fn decodes_a_pairing_url() {
        let pairing = parse_qr_url(&qr_url("192.168.1.2,10.0.0.5\t32035\tsecret\tG#1:2022.12")).unwrap();
        assert_eq!(pairing.ips, ["192.168.1.2", "10.0.0.5"]);
        assert_eq!(pairing.port, 32035);
        assert_eq!(pairing.password, "secret");
        assert_eq!(pairing.generation, "G#1:2022.12");
    }

    #[test]
    fn rejects_unrelated_urls() {
        assert_eq!(parse_qr_url("https://example.com/"), None);
        assert_eq!(parse_qr_url("https://example.com/?s=zz"), None);
        assert_eq!(parse_qr_url(&qr_url("not a pairing payload")), None);
    }

    #[test]
    fn tokens_match_the_documented_scheme() {
        // "a" ^ 0xB6 = 0xD7, "b" ^ 0xD5 = 0xB7
        assert_eq!(token("ab"), "d7b7");
    }

    #[test]
    fn reconnect_reuses_the_saved_password() {
        let pairing = Pairing {
            ips: vec![],
            port: 1,
            password: "saved".into(),
            generation: "G".into(),
        };
        let detail: Vec<String> = serde_json::from_str(&reconnect_detail(&pairing)).unwrap();
        assert_eq!(detail, ["G".to_string(), token(RECONNECT_MARKER), token("saved")]);
    }
}
