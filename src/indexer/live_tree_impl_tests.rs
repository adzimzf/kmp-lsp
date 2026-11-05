use std::sync::Arc;

use tower_lsp::lsp_types::Url;

use super::*;

/// A remembered parse of `file` whose source is `source_bytes` long.
fn parse_of(file: &str, source_bytes: usize) -> UnopenedParse {
    let uri = Url::parse(&format!("file:///work/{file}.kt")).unwrap();
    let source = format!("class C{}", " ".repeat(source_bytes.saturating_sub(7)));
    let lines = Arc::new(vec![source.clone()]);
    let Some(doc) = parse_live(&source, tree_sitter_kotlin::LANGUAGE.into()) else {
        panic!("parse_live failed");
    };
    UnopenedParse::new(&uri, &lines, &Arc::new(doc))
}

fn remembered_files(parses: &[UnopenedParse]) -> Vec<String> {
    parses
        .iter()
        .map(|parse| {
            parse
                .uri
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .trim_end_matches(".kt")
                .to_owned()
        })
        .collect()
}

#[test]
fn cost_scales_with_the_source() {
    let small = parse_of("Small", 100);
    let large = parse_of("Large", 10_000);
    assert_eq!(small.cost_bytes, 100 * TREE_BYTES_PER_SOURCE_BYTE);
    assert_eq!(large.cost_bytes, 10_000 * TREE_BYTES_PER_SOURCE_BYTE);
}

#[test]
fn least_recently_used_trees_go_until_the_new_one_fits() {
    let one_file = parse_of("Probe", 1_000).cost_bytes;
    let budget = one_file * 3;
    let mut parses = Vec::new();
    for file in ["A", "B", "C"] {
        remember_parse(&mut parses, parse_of(file, 1_000), budget);
    }
    assert_eq!(remembered_files(&parses), ["A", "B", "C"]);

    // A file twice the size needs two slots: the two oldest go.
    remember_parse(&mut parses, parse_of("Big", 2_000), budget);
    assert_eq!(remembered_files(&parses), ["C", "Big"]);
    let used: usize = parses.iter().map(|parse| parse.cost_bytes).sum();
    assert!(used <= budget);
}

#[test]
fn many_small_files_share_the_budget_a_large_one_would_take() {
    // The old cache kept 32 files whatever their size; the budget keeps as
    // many as fit.
    let budget = parse_of("Probe", 10_000).cost_bytes;
    let mut parses = Vec::new();
    for number in 0..100 {
        remember_parse(
            &mut parses,
            parse_of(&format!("Small{number}"), 100),
            budget,
        );
    }
    assert_eq!(parses.len(), 100);
}

#[test]
fn a_tree_bigger_than_the_budget_is_not_remembered_and_evicts_nothing() {
    let budget = parse_of("Probe", 1_000).cost_bytes * 2;
    let mut parses = Vec::new();
    remember_parse(&mut parses, parse_of("A", 1_000), budget);
    remember_parse(&mut parses, parse_of("Huge", 100_000), budget);
    assert_eq!(remembered_files(&parses), ["A"]);
}

#[test]
fn a_new_version_of_a_file_replaces_its_old_tree() {
    let budget = parse_of("Probe", 1_000).cost_bytes * 2;
    let mut parses = Vec::new();
    remember_parse(&mut parses, parse_of("A", 1_000), budget);
    remember_parse(&mut parses, parse_of("B", 1_000), budget);
    // Re-parsing A must not count its old tree against the budget, so B stays.
    remember_parse(&mut parses, parse_of("A", 1_000), budget);
    assert_eq!(remembered_files(&parses), ["B", "A"]);
}
