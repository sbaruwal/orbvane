//! JSON language support of our own: a JSONC parser that keeps positions, JSON Schema
//! validation, and completion, hovers, outline, folding and formatting driven by schemas.
//! `server::serve` offers them as a language server running in-process.

pub mod features;
pub mod parse;
pub mod schema;
pub mod server;

pub use server::serve;
