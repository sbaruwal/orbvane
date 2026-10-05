//! Lists the given color keys that a theme leaves unset (drawn transparent) or doesn't know.
//! Usage: cargo run -p theme --example unset < keys.txt
use std::io::Read;

fn main() {
    let mut keys = String::new();
    std::io::stdin().read_to_string(&mut keys).unwrap();
    for info in theme::builtin_themes() {
        let t = theme::Theme::load(&info).unwrap();
        let unset: Vec<&str> = keys.lines().filter(|k| t.color_opt(k).is_none()).collect();
        println!("{}: {:?}", info.name, unset);
    }
}
