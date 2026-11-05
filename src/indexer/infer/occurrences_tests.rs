use tower_lsp::lsp_types::{Location, Position, Range, Url};

use super::*;

/// Kotlin and Java files whose `save` hits cover every outcome: a member
/// call on a typed receiver, a local declaration, a bare call reaching the
/// enclosing class, a comment and a string.
fn workspace(files: usize) -> (Indexer, Vec<Location>) {
    let indexer = Indexer::new();
    let mut hits = Vec::new();
    let repo = "package app\nclass Repo {\n    fun save(s: String) {}\n}\n";
    let repo_uri = Url::parse("file:///work/Repo.kt").unwrap();
    indexer.index_content(&repo_uri, repo);
    for file in 0..files {
        let (uri, source) = if file % 2 == 0 {
            (
                Url::parse(&format!("file:///work/Caller{file}.kt")).unwrap(),
                "package app\nclass Caller {\n    fun save(s: String) {}\n    fun run(repo: Repo) {\n        repo.save(\"a\")\n        val save = 1\n        save(\"b\")\n        // save\n        val text = \"save\"\n    }\n}\n".to_owned(),
            )
        } else {
            (
                Url::parse(&format!("file:///work/Caller{file}.java")).unwrap(),
                "package app;\nclass Caller {\n    void save(String s) {}\n    void run(Repo repo) {\n        repo.save(\"a\");\n        int save = 1;\n        save(\"b\");\n        // save\n        String text = \"save\";\n    }\n}\n".to_owned(),
            )
        };
        indexer.index_content(&uri, &source);
        for (line_index, line) in source.lines().enumerate() {
            for (column, _) in line.match_indices("save") {
                let start = Position::new(line_index as u32, column as u32);
                let end = Position::new(line_index as u32, column as u32 + 4);
                hits.push(Location {
                    uri: uri.clone(),
                    range: Range { start, end },
                });
            }
        }
    }
    (indexer, hits)
}

fn describe(occurrence: &ClassifiedOccurrence) -> String {
    format!("{:?} / {:?}", occurrence.site, occurrence.scope)
}

#[test]
fn workers_classify_exactly_like_one_thread_in_the_same_order() {
    let (indexer, hits) = workspace(12);
    let locations: Vec<&Location> = hits.iter().collect();
    assert!(
        locations.len() >= MIN_HITS_FOR_WORKERS,
        "the fixture must take the worker path"
    );

    let in_parallel = classify_occurrences(&indexer, &locations, OccurrenceDetail::WithScope);
    let one_by_one: Vec<ClassifiedOccurrence> = locations
        .iter()
        .map(|location| classify_occurrence(&indexer, location, OccurrenceDetail::WithScope))
        .collect();

    assert_eq!(in_parallel.len(), locations.len());
    let parallel: Vec<String> = in_parallel.iter().map(describe).collect();
    let sequential: Vec<String> = one_by_one.iter().map(describe).collect();
    assert_eq!(parallel, sequential);
}

#[test]
fn scope_tells_locals_receivers_and_members_apart() {
    let (indexer, hits) = workspace(2);
    let locations: Vec<&Location> = hits.iter().collect();
    let classified = classify_occurrences(&indexer, &locations, OccurrenceDetail::WithScope);
    for (location, occurrence) in locations.iter().zip(&classified) {
        let line = location.range.start.line;
        let scope = &occurrence.scope;
        match line {
            // `val save = 1` / `int save = 1`
            5 => assert!(
                matches!(occurrence.site, OccurrenceSite::Symbol(_)),
                "{}: {:?}",
                location.uri,
                occurrence.site
            ),
            // bare `save("b")` reaches the enclosing class's own `save`.
            6 => assert!(
                matches!(scope, OccurrenceScope::ImplicitReceiver(receiver) if receiver.type_path() == "Caller"),
                "{}: {scope:?}",
                location.uri
            ),
            // comment and string text
            7 | 8 => assert!(
                matches!(occurrence.site, OccurrenceSite::NotCode(_)),
                "{}:{line}: {:?}",
                location.uri,
                occurrence.site
            ),
            _ => {}
        }
    }
}

#[test]
fn site_only_skips_the_scope() {
    let (indexer, hits) = workspace(2);
    let locations: Vec<&Location> = hits.iter().collect();
    let classified = classify_occurrences(&indexer, &locations, OccurrenceDetail::SiteOnly);
    assert!(classified
        .iter()
        .all(|occurrence| matches!(occurrence.scope, OccurrenceScope::Unchecked)));
}

#[test]
fn files_are_never_split_between_workers() {
    let (_indexer, hits) = workspace(6);
    let locations: Vec<&Location> = hits.iter().collect();
    let assignments = assign_files_to_workers(&locations, 4);
    let mut worker_of_file: std::collections::HashMap<&str, usize> = Default::default();
    let mut assigned = 0;
    for (worker, indices) in assignments.iter().enumerate() {
        for &index in indices {
            assigned += 1;
            let previous = worker_of_file.insert(locations[index].uri.as_str(), worker);
            assert!(previous.is_none_or(|previous| previous == worker));
        }
    }
    assert_eq!(assigned, locations.len());
}
