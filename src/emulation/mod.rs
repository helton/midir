//! Emulation layer for text-only backends (emulation/): tool calling, structured output, stop sequences and
//! max_tokens built on a backend that takes one text prompt and returns text.

pub mod engine;
pub mod followups;
pub mod jsonmode;
pub mod output;
pub mod parser;
pub mod prompt;
