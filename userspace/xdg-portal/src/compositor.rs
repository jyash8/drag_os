//! Client-side support for the mywima compositor channel.
//! Protocol: 4-byte little-endian length prefix + UTF-8 JSON payload.

use std::os::unix::net::UnixStream;
use std::io::{Read, Write};

const COMPOSITOR_CHANNEL_PATH: &str = "/tmp/mywima-channel.sock";

/// Send a request to the compositor and get a response.
pub fn compositor_request(req: &str) -> Result<String, &'static str> {
    let mut stream = UnixStream::connect(COMPOSITOR_CHANNEL_PATH)
        .map_err(|_| "connect failed")?;
    stream.write_all(&(req.len() as u32).to_le_bytes()).ok();
    stream.write_all(req.as_bytes()).ok();
    stream.flush().ok();

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).map_err(|_| "read len failed")?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut resp = vec![0u8; len];
    stream.read_exact(&mut resp).map_err(|_| "read resp failed")?;
    String::from_utf8(resp).map_err(|_| "invalid utf8")
}

pub fn ping() -> bool {
    match compositor_request(r#"{"cmd":"ping"}"#) {
        Ok(r) => r.contains("\"ok\""),
        Err(_) => false,
    }
}