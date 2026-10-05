//! Times a search: `cargo run --release -p search --example bench -- <dir> <pattern>`
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("dir");
    let pattern = args.next().expect("pattern");
    let query = search::Query { pattern, use_ignore_files: true, ..Default::default() };
    let t = Instant::now();
    let s = search::Search::start(std::path::Path::new(&dir), &query, Arc::new(|| {})).unwrap();
    let (mut files, mut matches) = (0, 0);
    while !s.is_done() {
        for f in s.poll() {
            files += 1;
            matches += f.matches.len();
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    for f in s.poll() {
        files += 1;
        matches += f.matches.len();
    }
    let searched = s.files_searched.load(std::sync::atomic::Ordering::Relaxed);
    println!("{matches} matches in {files} files ({searched} searched) in {:?}{}", t.elapsed(),
        if s.limit_hit.load(std::sync::atomic::Ordering::Relaxed) { " (limit hit)" } else { "" });
}
