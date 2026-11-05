use std::sync::Arc;
use tower_lsp::lsp_types::Url;

use super::live_tree::{lang_for_path, parse_live, LiveDoc};
use super::Indexer;

/// How much memory the trees `live_doc_or_parse` remembers for unopened
/// files may take. Find-references reads the tree of every file a name
/// occurs in, so on a project that fits, a repeated or related search
/// parses nothing.
const UNOPENED_PARSE_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// Memory a parsed tree takes per byte of its source, source copy included.
/// Measured on a 588-file Kotlin/Java project: 5 MB of source held as
/// trees raised the resident set by about 50 MB.
const TREE_BYTES_PER_SOURCE_BYTE: usize = 11;

/// A tree parsed from one version of an indexed file: the version is its
/// source lines, which a `FileData` rebuild (source-set reclassification)
/// keeps and a re-index replaces.
pub(crate) struct UnopenedParse {
    uri: String,
    parsed_from: Arc<Vec<String>>,
    doc: Arc<LiveDoc>,
    /// Estimated memory the tree takes, counted against the budget.
    cost_bytes: usize,
}

impl UnopenedParse {
    fn new(uri: &Url, parsed_from: &Arc<Vec<String>>, doc: &Arc<LiveDoc>) -> Self {
        UnopenedParse {
            uri: uri.to_string(),
            parsed_from: Arc::clone(parsed_from),
            doc: Arc::clone(doc),
            cost_bytes: doc.bytes.len().max(1) * TREE_BYTES_PER_SOURCE_BYTE,
        }
    }
}

/// Remember `parse` as the most recently used, replacing any older parse of
/// the same file, and forget the least recently used ones until all fit in
/// `budget_bytes`. A tree bigger than the whole budget is not remembered.
fn remember_parse(parses: &mut Vec<UnopenedParse>, parse: UnopenedParse, budget_bytes: usize) {
    parses.retain(|remembered| remembered.uri != parse.uri);
    if parse.cost_bytes > budget_bytes {
        return;
    }
    let mut used: usize = parses.iter().map(|remembered| remembered.cost_bytes).sum();
    let mut evicted = 0;
    while used + parse.cost_bytes > budget_bytes && evicted < parses.len() {
        used -= parses[evicted].cost_bytes;
        evicted += 1;
    }
    parses.drain(..evicted);
    parses.push(parse);
}

impl Indexer {
    /// The tree of `uri`'s indexed `lines`, parsed at most once per version
    /// for the most recently used files that fit the memory budget. A poisoned lock only loses the
    /// memo, never the answer.
    fn parse_unopened(&self, uri: &Url, lines: &Arc<Vec<String>>) -> Option<Arc<LiveDoc>> {
        if let Some(doc) = self.remembered_unopened_parse(uri, lines) {
            return Some(doc);
        }
        // Parsed without the memo's lock held: find-references classifies
        // the files of one request on several threads at once.
        let lang = lang_for_path(uri.path())?;
        let doc = Arc::new(parse_live(&lines.join("\n"), lang)?);
        let mut parses = self
            .unopened_parses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        remember_parse(
            &mut parses,
            UnopenedParse::new(uri, lines, &doc),
            UNOPENED_PARSE_BUDGET_BYTES,
        );
        Some(doc)
    }

    /// The memoized tree of this version of `uri`'s lines, marked most
    /// recently used.
    fn remembered_unopened_parse(
        &self,
        uri: &Url,
        lines: &Arc<Vec<String>>,
    ) -> Option<Arc<LiveDoc>> {
        let mut parses = self
            .unopened_parses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let remembered_at = parses.iter().position(|parse| {
            Arc::ptr_eq(&parse.parsed_from, lines) && parse.uri == uri.as_str()
        })?;
        let parse = parses.remove(remembered_at);
        let doc = Arc::clone(&parse.doc);
        parses.push(parse);
        Some(doc)
    }

    /// Parse `content` and store the resulting `LiveDoc` for `uri`.
    ///
    /// If the file extension is unsupported or parsing fails, any previously
    /// stored tree for `uri` is removed so consumers never read a stale doc.
    pub(crate) fn store_live_tree(&self, uri: &Url, content: &str) {
        let path = uri.path();
        match lang_for_path(path).and_then(|lang| parse_live(content, lang)) {
            Some(doc) => {
                self.live_trees.insert(uri.to_string(), Arc::new(doc));
            }
            None => {
                self.live_trees.remove(uri.as_str());
            }
        }
        // The new content invalidates any memoized brace-repair candidates —
        // they're derived from the old bytes and would silently answer every
        // repair attempt against stale text otherwise.
        self.repair_candidates.remove(uri.as_str());
    }

    /// Return the `LiveDoc` for `uri`, or `None` if the file is not open.
    pub(crate) fn live_doc(&self, uri: &Url) -> Option<Arc<LiveDoc>> {
        self.live_trees.get(uri.as_str()).map(|r| Arc::clone(&*r))
    }

    /// Return the `LiveDoc` for `uri`, parsing on-demand if not already cached.
    ///
    /// Used by CST-based queries (e.g. `enclosing_class_at`) that are called
    /// before `textDocument/didOpen` has been fully processed — the actor
    /// receives `did_open` asynchronously so the live tree may not exist yet
    /// when a navigation request arrives.  Content is taken from `live_lines`
    /// (if present), the indexed file, or disk (cold-start fallback); the
    /// result is stored into `live_trees` **only when the file is currently open**
    /// (i.e. `live_lines` is present) so that `live_trees` does not accumulate
    /// parse trees for files that were never opened by the editor.
    pub(crate) fn live_doc_or_parse(&self, uri: &Url) -> Option<Arc<LiveDoc>> {
        if let Some(doc) = self.live_doc(uri) {
            return Some(doc);
        }

        let is_open = self.live_lines.contains_key(uri.as_str());
        if !is_open {
            let indexed_lines = self
                .files
                .get(uri.as_str())
                .map(|file| Arc::clone(&file.lines));
            if let Some(lines) = indexed_lines {
                return self.parse_unopened(uri, &lines);
            }
        }

        let content: String = if let Some(ll) = self.live_lines.get(uri.as_str()) {
            ll.join("\n")
        } else if let Ok(path) = uri.to_file_path() {
            std::fs::read_to_string(path).ok()?
        } else {
            return None;
        };

        let lang = lang_for_path(uri.path())?;
        let doc = parse_live(&content, lang)?;
        let doc = Arc::new(doc);
        if is_open {
            self.live_trees.insert(uri.to_string(), Arc::clone(&doc));
        }
        Some(doc)
    }

    /// Remove the live parse tree for `uri` (called on `textDocument/didClose`).
    pub(crate) fn remove_live_tree(&self, uri: &Url) {
        self.live_trees.remove(uri.as_str());
        self.repair_candidates.remove(uri.as_str());
    }
}

#[cfg(test)]
#[path = "live_tree_impl_tests.rs"]
mod tests;
