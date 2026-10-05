//! HTML language support of our own: the browser
//! data, a tolerant scanner and parser, completion, hovers, linked editing, folding, the outline
//! auto-inserted end tags and quotes, and Format Document. `serve` offers them as a language server running
//! in-process; CSS in `<style>` and `style=""` goes to the css crate.

pub mod data;
pub mod features;
pub mod format;
pub mod parse;
pub mod server;

pub use server::serve;
