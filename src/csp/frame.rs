//! Wire framing:
//! `<type>$tcp_remote_command_protocol_version=1.0 RS $command=.. RS $serial=.. RS $detail=.. RS NUL`

pub const TYPE_COMMAND: u8 = 0x01;
pub const TYPE_SUCCESS: u8 = 0x06;
pub const TYPE_ERROR: u8 = 0x15;

const RS: u8 = 0x1E;
const NUL: u8 = 0x00;
/// Separates the JSON detail from an optional binary tail, which we ignore.
const BINARY_TAIL: u8 = 0x0B;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: u8,
    pub command: String,
    pub serial: u64,
    /// Raw JSON text; empty when the frame has no detail.
    pub detail: String,
}

pub fn encode(kind: u8, command: &str, serial: u64, detail: &str) -> Vec<u8> {
    let mut out = vec![kind];
    out.extend_from_slice(
        format!(
            "$tcp_remote_command_protocol_version=1.0\x1e$command={command}\x1e$serial={serial}\x1e$detail={detail}\x1e"
        )
        .as_bytes(),
    );
    out.push(NUL);
    out
}

/// Removes the next complete frame from `buf`. Malformed frames are dropped,
/// so call this in a loop until it returns `None`.
pub fn next_frame(buf: &mut Vec<u8>) -> Option<Frame> {
    while let Some(end) = buf.iter().position(|&b| b == NUL) {
        let raw: Vec<u8> = buf.drain(..=end).collect();
        if let Some(frame) = parse(&raw[..end]) {
            return Some(frame);
        }
    }
    None
}

fn parse(raw: &[u8]) -> Option<Frame> {
    let (&kind, body) = raw.split_first()?;
    let mut frame = Frame {
        kind,
        command: String::new(),
        serial: 0,
        detail: String::new(),
    };
    for field in body.split(|&b| b == RS) {
        let field = field.strip_prefix(b"$").unwrap_or(field);
        let Some(eq) = field.iter().position(|&b| b == b'=') else {
            continue;
        };
        let (key, value) = (&field[..eq], &field[eq + 1..]);
        match key {
            b"command" => frame.command = String::from_utf8_lossy(value).into_owned(),
            b"serial" => frame.serial = std::str::from_utf8(value).ok()?.parse().ok()?,
            b"detail" => {
                let json = value.split(|&b| b == BINARY_TAIL).next().unwrap_or(value);
                frame.detail = String::from_utf8_lossy(json).into_owned();
            }
            _ => {}
        }
    }
    (!frame.command.is_empty()).then_some(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_the_documented_layout() {
        assert_eq!(
            encode(TYPE_COMMAND, "TellHeartbeat", 7, ""),
            b"\x01$tcp_remote_command_protocol_version=1.0\x1e$command=TellHeartbeat\x1e$serial=7\x1e$detail=\x1e\x00"
        );
    }

    #[test]
    fn splits_coalesced_frames_and_keeps_partial_ones() {
        let mut buf = encode(TYPE_SUCCESS, "A", 1, "{\"x\":\"a=b\"}");
        buf.extend(encode(TYPE_ERROR, "B", 2, ""));
        let partial = encode(TYPE_COMMAND, "C", 3, "");
        buf.extend(&partial[..10]);

        let a = next_frame(&mut buf).unwrap();
        assert_eq!((a.kind, a.serial, a.detail.as_str()), (TYPE_SUCCESS, 1, "{\"x\":\"a=b\"}"));
        assert_eq!(next_frame(&mut buf).unwrap().command, "B");
        assert_eq!(next_frame(&mut buf), None);
        assert_eq!(buf, &partial[..10]);
    }

    #[test]
    fn ignores_the_binary_tail() {
        let mut buf = encode(TYPE_SUCCESS, "Preview", 1, "{\"w\":1}\x0bQUJD");
        assert_eq!(next_frame(&mut buf).unwrap().detail, "{\"w\":1}");
    }

    #[test]
    fn drops_malformed_frames() {
        let mut buf = b"\x01garbage\x00".to_vec();
        buf.extend(encode(TYPE_SUCCESS, "A", 1, ""));
        assert_eq!(next_frame(&mut buf).unwrap().command, "A");
    }
}
