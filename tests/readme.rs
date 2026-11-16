//! The README quotes compiled code only: every ```rust block in `README.md` must appear line
//! for line (indentation aside) in one of the examples, or in the `async_api` module docs
//! (doc-comment prefix and hidden `# ` lines aside). An example edited without its README
//! excerpt fails here.

use std::path::Path;

fn read(path: &str) -> String {
    let full = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("read {}: {e}", full.display()))
}

/// The README's ```rust blocks, each as its lines, trimmed.
fn rust_blocks(readme: &str) -> Vec<Vec<String>> {
    let mut blocks = Vec::new();
    let mut current: Option<Vec<String>> = None;
    for line in readme.lines() {
        let fence = line.trim_start();
        match current.as_mut() {
            None if fence.starts_with("```rust") => current = Some(Vec::new()),
            None => {}
            Some(_) if fence.starts_with("```") => blocks.extend(current.take()),
            Some(block) => block.push(line.trim().to_string()),
        }
    }
    assert!(current.is_none(), "README.md: unclosed ```rust block");
    blocks
}

/// A source file's lines, trimmed.
fn code_lines(source: &str) -> Vec<String> {
    source.lines().map(|l| l.trim().to_string()).collect()
}

/// The module docs' lines (`//!` stripped), trimmed, without rustdoc's hidden `# ` lines.
fn doc_lines(source: &str) -> Vec<String> {
    source
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("//!"))
        .map(|l| l.strip_prefix(' ').unwrap_or(l))
        .filter(|l| *l != "#" && !l.starts_with("# "))
        .map(|l| l.trim().to_string())
        .collect()
}

#[test]
fn every_readme_code_block_is_quoted_from_compiled_code() {
    let sources = [
        (
            "examples/bare_shell.rs",
            code_lines(&read("examples/bare_shell.rs")),
        ),
        (
            "examples/agent_send.rs",
            code_lines(&read("examples/agent_send.rs")),
        ),
        (
            "examples/rpc_child.rs",
            code_lines(&read("examples/rpc_child.rs")),
        ),
        ("src/async_api.rs", doc_lines(&read("src/async_api.rs"))),
    ];
    let blocks = rust_blocks(&read("README.md"));
    assert!(!blocks.is_empty(), "README.md has no ```rust block");
    for block in &blocks {
        assert!(!block.is_empty(), "README.md has an empty ```rust block");
        let quoted = sources
            .iter()
            .any(|(_, lines)| lines.windows(block.len()).any(|w| w == block.as_slice()));
        let names: Vec<&str> = sources.iter().map(|(name, _)| *name).collect();
        assert!(
            quoted,
            "README.md block not found line for line in {names:?}:\n{}",
            block.join("\n")
        );
    }
    // Each example is quoted at least once.
    for (name, lines) in &sources[..3] {
        assert!(
            blocks
                .iter()
                .any(|b| lines.windows(b.len()).any(|w| w == b.as_slice())),
            "README.md quotes nothing from {name}"
        );
    }
}
