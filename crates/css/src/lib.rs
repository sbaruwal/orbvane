//! CSS, SCSS and Less language support of our own: a
//! tolerant parser, the lint rules, completion and hovers from the browser data,
//! the outline, folding, colors, references and rename for variables, classes and ids, and
//! formatting. `serve` offers them as a language server running in-process; the HTML server
//! uses the same pieces for `<style>` and `style=""`.

pub mod colors;
pub mod data;
pub mod features;
pub mod format;
pub mod lint;
pub mod parse;
pub mod server;

pub use server::serve;
