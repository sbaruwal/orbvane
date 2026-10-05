//! Our own Node.js debugger: a Debug Adapter Protocol server that
//! runs inside the editor and drives `node --inspect-brk` over the inspector
//! protocol, through a WebSocket client of our own. Breakpoints (conditions, hit counts,
//! logpoints), stepping, call stacks, scopes and values, the console, exceptions, `skipFiles`
//! and source maps (TypeScript and other compiled code).

mod adapter;
pub mod format;
pub mod sourcemap;
pub mod ws;

pub use adapter::serve;

use std::path::PathBuf;

/// `%20` → ` `.
pub(crate) fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A path as it appears in a `file://` URL.
pub(crate) fn percent_encode(path: &str) -> String {
    path.bytes()
        .map(|c| match c {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => (c as char).to_string(),
            _ => format!("%{c:02X}"),
        })
        .collect()
}

/// The file a script URL names (`file:///x.js`, or a bare path in older Node).
pub(crate) fn path_of_url(url: &str) -> Option<PathBuf> {
    if let Some(p) = url.strip_prefix("file://") {
        return Some(PathBuf::from(percent_decode(p)));
    }
    url.starts_with('/').then(|| PathBuf::from(url))
}

pub(crate) fn base64_decode(s: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    };
    let digits: Vec<u8> = s.bytes().filter_map(value).collect();
    let mut out = Vec::with_capacity(digits.len() * 3 / 4);
    for chunk in digits.chunks(4) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &d)| n | (d as u32) << (18 - 6 * i));
        for i in 0..chunk.len().saturating_sub(1) {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_urls() {
        assert_eq!(path_of_url("file:///a%20b/c.js"), Some(PathBuf::from("/a b/c.js")));
        assert_eq!(path_of_url("/x/y.js"), Some(PathBuf::from("/x/y.js")));
        assert_eq!(path_of_url("node:internal/main"), None);
        assert_eq!(percent_encode("/a b/c.js"), "/a%20b/c.js");
        assert_eq!(base64_decode(&ws::base64(b"source map!")), b"source map!");
    }
}
