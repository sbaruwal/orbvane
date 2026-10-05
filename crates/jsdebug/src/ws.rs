//! A small WebSocket client (RFC 6455), enough for the inspector protocol: the HTTP upgrade,
//! masked text frames out, text frames in (fragmented or not), pings answered.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

/// Sends frames; cheap to clone (the reader holds one to answer pings).
#[derive(Clone)]
pub struct Writer(Arc<Mutex<TcpStream>>);

pub struct Reader {
    stream: BufReader<TcpStream>,
    writer: Writer,
}

const TEXT: u8 = 0x1;
const CLOSE: u8 = 0x8;
const PING: u8 = 0x9;
const PONG: u8 = 0xA;

/// `ws://host:port/path` → ("host:port", "/path").
fn split_url(url: &str) -> io::Result<(&str, &str)> {
    let rest = url.strip_prefix("ws://").ok_or_else(|| io::Error::other(format!("not a ws:// URL: {url}")))?;
    Ok(match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    })
}

pub fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Bytes that are hard to predict (masking keys and the handshake key need not be secret).
fn noise(n: usize) -> Vec<u8> {
    use std::hash::{BuildHasher, Hasher};
    let mut out = Vec::new();
    while out.len() < n {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        out.extend(h.finish().to_le_bytes());
    }
    out.truncate(n);
    out
}

/// Opens `url` (`ws://...`).
pub fn connect(url: &str) -> io::Result<(Writer, Reader)> {
    let (host, path) = split_url(url)?;
    let mut stream = TcpStream::connect(host)?;
    stream.set_nodelay(true)?;
    let key = base64(&noise(16));
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut status = String::new();
    reader.read_line(&mut status)?;
    if !status.split_whitespace().nth(1).is_some_and(|c| c == "101") {
        return Err(io::Error::other(format!("the inspector refused the connection: {}", status.trim())));
    }
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
    }
    let writer = Writer(Arc::new(Mutex::new(stream)));
    Ok((writer.clone(), Reader { stream: reader, writer }))
}

/// A frame as a client sends it: masked.
fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0x80 | opcode];
    match payload.len() {
        n if n < 126 => out.push(0x80 | n as u8),
        n if n <= 0xFFFF => {
            out.push(0x80 | 126);
            out.extend((n as u16).to_be_bytes());
        }
        n => {
            out.push(0x80 | 127);
            out.extend((n as u64).to_be_bytes());
        }
    }
    let mask = noise(4);
    out.extend(&mask);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    out
}

impl Writer {
    pub fn send(&self, text: &str) -> io::Result<()> {
        self.send_frame(TEXT, text.as_bytes())
    }

    fn send_frame(&self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        s.write_all(&frame(opcode, payload))
    }

    /// Ends the connection (the inspector then lets the program exit).
    pub fn close(&self) {
        let _ = self.send_frame(CLOSE, &[]);
        let s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
}

impl Reader {
    /// The next text message; None when the connection closes.
    pub fn next(&mut self) -> Option<String> {
        let mut message = Vec::new();
        loop {
            let mut head = [0u8; 2];
            self.stream.read_exact(&mut head).ok()?;
            let (fin, opcode) = (head[0] & 0x80 != 0, head[0] & 0x0F);
            let masked = head[1] & 0x80 != 0;
            let len = match head[1] & 0x7F {
                126 => {
                    let mut b = [0u8; 2];
                    self.stream.read_exact(&mut b).ok()?;
                    u16::from_be_bytes(b) as usize
                }
                127 => {
                    let mut b = [0u8; 8];
                    self.stream.read_exact(&mut b).ok()?;
                    u64::from_be_bytes(b) as usize
                }
                n => n as usize,
            };
            let mut mask = [0u8; 4];
            if masked {
                self.stream.read_exact(&mut mask).ok()?;
            }
            let mut payload = vec![0u8; len];
            self.stream.read_exact(&mut payload).ok()?;
            if masked {
                payload.iter_mut().enumerate().for_each(|(i, b)| *b ^= mask[i % 4]);
            }
            match opcode {
                CLOSE => return None,
                PING => {
                    let _ = self.writer.send_frame(PONG, &payload);
                }
                PONG => {}
                _ => {
                    message.extend(payload);
                    if fin {
                        return Some(String::from_utf8_lossy(&message).into_owned());
                    }
                }
            }
        }
    }
}

/// `GET http://host:port/path` (the inspector's `/json/list`), returning the body.
pub fn http_get(host: &str, path: &str) -> io::Result<String> {
    let mut stream = TcpStream::connect(host)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    write!(stream, "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response.split_once("\r\n\r\n").map_or(String::new(), |(_, body)| body.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_base64_and_frames() {
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
        let f = frame(TEXT, b"hi");
        assert_eq!((f[0], f[1]), (0x81, 0x82));
        let mask = &f[2..6];
        assert_eq!([f[6] ^ mask[0], f[7] ^ mask[1]], *b"hi");
        assert_eq!(frame(TEXT, &[0; 300])[1], 0x80 | 126);
        assert_eq!(split_url("ws://127.0.0.1:9229/abc").unwrap(), ("127.0.0.1:9229", "/abc"));
    }
}
