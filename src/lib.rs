//! Assembly (ASM) parser plugin — full-parse mode.
//!
//! Handles `.asm`, `.s`, `.S` files.
//! Uses the tree-sitter-asm Rust crate directly; no Python grammar package needed.
//!
//! Covers multiple ISAs (x86, ARM, RISC-V) as supported by the grammar.
//!
//! Key design decisions:
//! - `label` / `local_label` are the fundamental semantic unit (procedures in ASM are
//!   label-delimited); they map to method-like nodes.
//! - `section` and `meta` nodes are class-like (they group related instructions/data).
//! - `instruction` and `directive` nodes are emitted as leaf semantic nodes so that
//!   instruction-level renames and reorderings are tracked structurally.

use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentumdiff::plugin::parser::ExamplePair;
use crate::exports::intentumdiff::plugin::parser::Guest;
use crate::exports::intentumdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentumdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct AsmParser;

const TRIVIA: &[&str] = &["comment", "line_comment", "block_comment"];

const SEMANTIC_TYPES: &[&str] = &[
    // Root
    "source_file",
    "translation_unit",
    // Sections (class-like containers)
    "section",
    "meta",
    "segment",
    // Labels (method-like entry points)
    "label",
    "local_label",
    "label_definition",
    "global_label",
    // Instructions
    "instruction",
    "pseudo_instruction",
    // Directives
    "directive",
    "data_directive",
    "storage_directive",
    "string_directive",
    // Declarations
    "global_declaration",
    "extern_declaration",
    "constant",
    "equ",
];

fn is_trivia(kind: &str) -> bool {
    TRIVIA.contains(&kind)
}

fn is_section_like(kind: &str) -> bool {
    matches!(kind, "section" | "meta" | "segment")
}

fn is_label_like(kind: &str) -> bool {
    matches!(
        kind,
        "label" | "local_label" | "label_definition" | "global_label"
    )
}

fn ts_text<'a>(node: tree_sitter::Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

fn label_name_ts(node: tree_sitter::Node<'_>, source: &[u8]) -> String {
    for i in 0..node.child_count() {
        let child = node.child(i).unwrap();
        if is_trivia(child.kind()) {
            continue;
        }
        if matches!(child.kind(), "identifier" | "name" | "symbol") {
            return ts_text(child, source).to_string();
        }
        let t = ts_text(child, source)
            .trim_end_matches(':')
            .trim()
            .to_string();
        if !t.is_empty() {
            return t;
        }
    }
    let t = ts_text(node, source)
        .trim_end_matches(':')
        .trim()
        .to_string();
    if t.is_empty() {
        "(label)".to_string()
    } else {
        t
    }
}

fn section_name_ts(node: tree_sitter::Node<'_>, source: &[u8]) -> String {
    for i in 0..node.child_count() {
        let child = node.child(i).unwrap();
        if is_trivia(child.kind()) {
            continue;
        }
        if matches!(
            child.kind(),
            "identifier" | "name" | "section_name" | "string"
        ) {
            return ts_text(child, source).trim_matches('"').to_string();
        }
    }
    let raw = ts_text(node, source);
    if !raw.is_empty() {
        raw.split_whitespace()
            .nth(1)
            .unwrap_or("(section)")
            .to_string()
    } else {
        "(section)".to_string()
    }
}

fn label_for_ts(node: tree_sitter::Node<'_>, source: &[u8]) -> String {
    let kind = node.kind();
    if is_section_like(kind) {
        return section_name_ts(node, source);
    }
    if is_label_like(kind) {
        return label_name_ts(node, source);
    }
    // Instruction / directive — use mnemonic
    for i in 0..node.child_count() {
        let child = node.child(i).unwrap();
        if is_trivia(child.kind()) {
            continue;
        }
        if matches!(
            child.kind(),
            "mnemonic" | "opcode" | "directive_name" | "identifier" | "name"
        ) {
            return ts_text(child, source).to_string();
        }
        let t = ts_text(child, source);
        if !t.is_empty() {
            return t.split_whitespace().next().unwrap_or("").to_string();
        }
    }
    kind.to_string()
}

fn convert_ts(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    id_prefix: &str,
    current_section: Option<&str>,
) -> Option<SemanticNode> {
    let kind = node.kind();
    if is_trivia(kind) {
        return None;
    }

    let sl = node.start_position().row as u32;
    let sc = node.start_position().column as u32;
    let el = node.end_position().row as u32;
    let ec = node.end_position().column as u32;

    if is_section_like(kind) {
        let label = section_name_ts(node, source);
        let children: Vec<SemanticNode> = (0..node.child_count())
            .filter_map(|i| {
                convert_ts(
                    node.child(i)?,
                    source,
                    &format!("{}.{}", id_prefix, i),
                    Some(&label),
                )
            })
            .collect();
        return Some(
            SemanticNodeBuilder::new(id_prefix, kind, label, sl, sc, el, ec, "")
                .children(children)
                .build(),
        );
    }

    if is_label_like(kind) {
        let label = label_name_ts(node, source);
        let children: Vec<SemanticNode> = (0..node.child_count())
            .filter_map(|i| {
                convert_ts(
                    node.child(i)?,
                    source,
                    &format!("{}.{}", id_prefix, i),
                    current_section,
                )
            })
            .collect();
        let mut b =
            SemanticNodeBuilder::new(id_prefix, kind, label, sl, sc, el, ec, "").children(children);
        if let Some(sec) = current_section {
            b = b.parent_type(sec.to_string());
        }
        return Some(b.build());
    }

    if SEMANTIC_TYPES.contains(&kind) {
        let label = label_for_ts(node, source);
        let children: Vec<SemanticNode> = (0..node.child_count())
            .filter_map(|i| {
                convert_ts(
                    node.child(i)?,
                    source,
                    &format!("{}.{}", id_prefix, i),
                    current_section,
                )
            })
            .collect();
        let mut b =
            SemanticNodeBuilder::new(id_prefix, kind, label, sl, sc, el, ec, "").children(children);
        if let Some(sec) = current_section {
            b = b.parent_type(sec.to_string());
        }
        return Some(b.build());
    }

    // Not semantic — lift children
    let children: Vec<SemanticNode> = (0..node.child_count())
        .filter_map(|i| {
            convert_ts(
                node.child(i)?,
                source,
                &format!("{}.{}", id_prefix, i),
                current_section,
            )
        })
        .collect();
    if children.len() == 1 {
        return Some(children.into_iter().next().unwrap());
    }
    if !children.is_empty() {
        return Some(
            SemanticNodeBuilder::new(id_prefix, kind, "", sl, sc, el, ec, "")
                .children(children)
                .build(),
        );
    }
    None
}

fn process_impl(source: &str) -> String {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_asm::LANGUAGE.into())
        .is_err()
    {
        return r#"{"error":"Failed to load ASM grammar"}"#.to_string();
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return r#"{"error":"Parse failed"}"#.to_string(),
    };
    let root = tree.root_node();
    match convert_ts(root, source.as_bytes(), "0", None) {
        Some(n) => serde_json::to_string(&n).unwrap_or_else(|e| format!(r#"{{"error":"{}"}}"#, e)),
        None => r#"{"error":"Empty semantic tree"}"#.to_string(),
    }
}

impl Guest for AsmParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "asm".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        let lower = filename.to_lowercase();
        if lower.ends_with(".asm") || lower.ends_with(".s") {
            return "asm".to_string();
        }
        // .S (GNU preprocessed assembly) — check original case
        if filename.ends_with(".S") {
            return "asm".to_string();
        }
        String::new()
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "section .data\n    msg db \"Hello, World!\", 10\nsection .text\n    global _start\n_start:\n    mov rax, 1\n    mov rdi, 1\n    lea rsi, [msg]\n    mov rdx, 14\n    syscall\n    mov rax, 60\n    xor rdi, rdi\n    syscall\n".to_string(),
            new: "section .data\n    msg db \"Hello, World!\", 10\n    len equ $ - msg\nsection .text\n    global _start\nprint_msg:\n    mov rax, 1\n    mov rdi, 1\n    lea rsi, [msg]\n    mov rdx, len\n    syscall\n    ret\n_start:\n    call print_msg\n    mov rax, 60\n    xor rdi, rdi\n    syscall\n".to_string(),
        }
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        TRIVIA.iter().map(|s| s.to_string()).collect()
    }
    fn language_ids() -> Vec<String> {
        vec!["asm".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }
}

export!(AsmParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentumdiff::plugin::parser::Guest;
    use intentumdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!AsmParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        let gid = AsmParser::grammar_id();
        let ids = AsmParser::language_ids();
        assert!(
            ids.contains(&gid),
            "language_ids {:?} must contain {:?}",
            ids,
            gid
        );
    }

    #[test]
    fn detect_language_known_ext() {
        let r = AsmParser::detect_language("test.asm".to_string(), "".to_string());
        assert_eq!(r.as_str(), "asm");
    }

    #[test]
    fn detect_language_unknown_ext() {
        let r =
            AsmParser::detect_language("test.xyz_notareal_ext_9z8y".to_string(), "".to_string());
        assert_eq!(r.as_str(), "");
    }

    #[test]
    fn process_impl_empty_returns_valid_json() {
        let out = process_impl("");
        t::assert_valid_json(&out, "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        let out = process_impl("   \n  ");
        t::assert_valid_json(&out, "process(whitespace)");
    }
}
