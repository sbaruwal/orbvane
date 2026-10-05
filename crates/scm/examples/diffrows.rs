//! Prints side-by-side rows for two files: `cargo run -p scm --example diffrows -- old new`
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let old = std::fs::read_to_string(&args[0]).unwrap();
    let new = std::fs::read_to_string(&args[1]).unwrap();
    for row in scm::side_by_side(&old, &new) {
        println!("{row:?}");
    }
}
