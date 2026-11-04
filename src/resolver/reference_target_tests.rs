use super::*;

#[test]
fn word_columns_match_whole_identifiers_only() {
    assert_eq!(
        word_columns("normalize(normalized) + normalize", "normalize"),
        vec![0, 24]
    );
    assert_eq!(
        word_columns("renormalize _normalize normalize_", "normalize"),
        Vec::<u32>::new()
    );
    assert_eq!(
        word_columns("Normalizer.normalize(x)", "normalize"),
        vec![11]
    );
}

#[test]
fn word_columns_are_utf16() {
    // `é` is one UTF-16 unit but two UTF-8 bytes; `😀` is two units.
    assert_eq!(word_columns("é 😀 name", "name"), vec![5]);
}
