//! Finds CSP's pairing QR on screen. Captures stay in memory and are never saved.

use xcap::Monitor;

use crate::csp::auth::{parse_qr_url, Pairing};

pub fn scan_screens() -> Result<Option<Pairing>, String> {
    let monitors = Monitor::all().map_err(|e| e.to_string())?;
    for monitor in monitors {
        let image = monitor.capture_image().map_err(|e| e.to_string())?;
        let (width, height) = (image.width() as usize, image.height() as usize);
        let pixels = image.as_raw();
        let mut prepared = rqrr::PreparedImage::prepare_from_greyscale(width, height, |x, y| {
            let px = &pixels[(y * width + x) * 4..][..3];
            ((px[0] as u16 * 77 + px[1] as u16 * 150 + px[2] as u16 * 29) >> 8) as u8
        });
        for grid in prepared.detect_grids() {
            if let Some(pairing) = grid.decode().ok().and_then(|(_, text)| parse_qr_url(&text)) {
                return Ok(Some(pairing));
            }
        }
    }
    Ok(None)
}
