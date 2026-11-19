//! Fresh single-terminal transcript namespaces. The host controls every writer.
//! Matching cwd and recency cannot select a terminal in a shared configuration root.

mod locate;
mod parser;
mod source;
#[cfg(test)]
mod tests;

pub(in crate::agent) use source::{attach, capture, check_empty};
