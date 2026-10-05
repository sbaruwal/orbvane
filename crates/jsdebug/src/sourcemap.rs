//! Source maps (version 3): which original line a generated position comes from, and where an
//! original line's code went. Lines and columns are 0-based.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Mapping {
    gen_line: u32,
    gen_col: u32,
    src: u32,
    line: u32,
    col: u32,
}

#[derive(Debug)]
pub struct SourceMap {
    /// The original files, as absolute paths.
    pub sources: Vec<PathBuf>,
    /// Sorted by generated position.
    mappings: Vec<Mapping>,
}

/// `a/b/../c` → `a/c`, without touching the disk.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// A file URL or path in a map as a path.
fn source_path(source: &str, root: &str, dir: &Path) -> PathBuf {
    let joined = if root.is_empty() { source.to_string() } else { format!("{}/{source}", root.trim_end_matches('/')) };
    if let Some(p) = joined.strip_prefix("file://") {
        return normalize(Path::new(&crate::percent_decode(p)));
    }
    // `webpack:///./src/x.ts` and the like: the part after the scheme, under the map's folder.
    let joined = match joined.find("://") {
        Some(i) => joined[i + 3..].trim_start_matches('/').to_string(),
        None => joined,
    };
    normalize(&dir.join(joined))
}

fn vlq(s: &[u8], i: &mut usize) -> Option<i64> {
    let mut value = 0i64;
    let mut shift = 0;
    loop {
        let c = *s.get(*i)?;
        *i += 1;
        let digit = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as i64;
        value |= (digit & 31) << shift;
        shift += 5;
        if digit & 32 == 0 {
            break;
        }
    }
    Some(if value & 1 == 1 { -(value >> 1) } else { value >> 1 })
}

impl SourceMap {
    /// Parses a map whose file is in `dir` (relative sources are resolved from there).
    pub fn parse(text: &str, dir: &Path) -> Option<SourceMap> {
        let v: Value = serde_json::from_str(text).ok()?;
        let root = v["sourceRoot"].as_str().unwrap_or("");
        let sources = v["sources"].as_array()?.iter().map(|s| source_path(s.as_str().unwrap_or(""), root, dir)).collect();
        let s = v["mappings"].as_str()?.as_bytes();
        let mut mappings = Vec::new();
        let (mut gen_line, mut gen_col, mut src, mut line, mut col, mut name) = (0i64, 0i64, 0i64, 0i64, 0i64, 0i64);
        let mut i = 0;
        while i < s.len() {
            match s[i] {
                b';' => {
                    gen_line += 1;
                    gen_col = 0;
                    i += 1;
                }
                b',' => i += 1,
                _ => {
                    gen_col += vlq(s, &mut i)?;
                    let more = |i: usize| i < s.len() && s[i] != b',' && s[i] != b';';
                    if more(i) {
                        src += vlq(s, &mut i)?;
                        line += vlq(s, &mut i)?;
                        col += vlq(s, &mut i)?;
                        if more(i) {
                            name += vlq(s, &mut i)?;
                        }
                        mappings.push(Mapping { gen_line: gen_line as u32, gen_col: gen_col as u32, src: src as u32, line: line as u32, col: col as u32 });
                    }
                }
            }
        }
        let _ = name;
        mappings.sort_by_key(|m| (m.gen_line, m.gen_col));
        Some(SourceMap { sources, mappings })
    }

    pub fn has_source(&self, path: &Path) -> bool {
        self.sources.iter().any(|s| s == path)
    }

    /// Where generated (`line`, `col`) comes from: the closest mapping at or before it on the
    /// line, else the line's first.
    pub fn original(&self, line: u32, col: u32) -> Option<(&Path, u32, u32)> {
        let end = self.mappings.partition_point(|m| (m.gen_line, m.gen_col) <= (line, col));
        let m = match end.checked_sub(1).map(|i| self.mappings[i]).filter(|m| m.gen_line == line) {
            Some(m) => m,
            None => *self.mappings.get(end).filter(|m| m.gen_line == line)?,
        };
        Some((self.sources.get(m.src as usize)?, m.line, m.col))
    }

    /// Where original `line` of `source` went: the first generated position of that line, or
    /// of the next line that has code.
    pub fn generated(&self, source: &Path, line: u32) -> Option<(u32, u32)> {
        let src = self.sources.iter().position(|s| s == source)? as u32;
        self.mappings
            .iter()
            .filter(|m| m.src == src && m.line >= line)
            .min_by_key(|m| (m.line, m.gen_line, m.gen_col))
            .map(|m| (m.gen_line, m.gen_col))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_and_maps_both_ways() {
        let mut i = 0;
        assert_eq!(vlq(b"gB", &mut i), Some(16));
        // tsc output for: let a = 1;\nconsole.log(a);  (two lines, one to one)
        let map = r#"{"version":3,"sources":["../src/a.ts"],"mappings":"AAAA,IAAI,CAAC,GAAG,CAAC,CAAC;AACV,OAAO,CAAC,GAAG,CAAC,CAAC,CAAC,CAAC"}"#;
        let m = SourceMap::parse(map, Path::new("/p/out")).unwrap();
        assert_eq!(m.sources, [PathBuf::from("/p/src/a.ts")]);
        assert_eq!(m.original(1, 8), Some((Path::new("/p/src/a.ts"), 1, 8)));
        assert_eq!(m.generated(Path::new("/p/src/a.ts"), 1), Some((1, 0)));
        assert_eq!(m.generated(Path::new("/p/src/b.ts"), 1), None);
        assert_eq!(normalize(Path::new("/a/b/../c/./d")), PathBuf::from("/a/c/d"));
        assert_eq!(source_path("webpack:///./src/x.ts", "", Path::new("/p/dist")), PathBuf::from("/p/dist/src/x.ts"));
    }
}
