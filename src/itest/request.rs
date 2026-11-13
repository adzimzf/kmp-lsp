//! The request kinds a case can ask, and how each answer is written down.
//!
//! An answer is rendered as the facts a reader would check by eye — a
//! position and the source line there, a name — one per line, in an order
//! that does not depend on the server. JSON shapes, URIs of the temporary
//! workspace and details one server words differently from another are
//! left out, so the same expectation holds whichever server is asked.

use serde_json::{json, Value};

use super::fixture::Fixture;
use super::server::Session;

/// What an answer with nothing in it is rendered as.
pub(super) const NOTHING: &str = "(nothing)";

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Definition,
    TypeDefinition,
    References,
    DocumentHighlight,
    Completion,
    Hover,
    SignatureHelp,
    PrepareRename,
    DocumentSymbol,
    FoldingRange,
    /// The query is the identifier under the cursor.
    WorkspaceSymbol,
}

impl Kind {
    fn method(self) -> &'static str {
        match self {
            Self::Definition => "textDocument/definition",
            Self::TypeDefinition => "textDocument/typeDefinition",
            Self::References => "textDocument/references",
            Self::DocumentHighlight => "textDocument/documentHighlight",
            Self::Completion => "textDocument/completion",
            Self::Hover => "textDocument/hover",
            Self::SignatureHelp => "textDocument/signatureHelp",
            Self::PrepareRename => "textDocument/prepareRename",
            Self::DocumentSymbol => "textDocument/documentSymbol",
            Self::FoldingRange => "textDocument/foldingRange",
            Self::WorkspaceSymbol => "workspace/symbol",
        }
    }

    fn params(self, session: &Session, fixture: &Fixture) -> Value {
        let cursor = &fixture.cursor;
        let document = json!({"uri": session.uri(&cursor.path)});
        let position = json!({"line": cursor.line, "character": cursor.character});
        match self {
            Self::DocumentSymbol | Self::FoldingRange => json!({"textDocument": document}),
            Self::WorkspaceSymbol => json!({"query": fixture.word_at_cursor()}),
            Self::References => json!({
                "textDocument": document,
                "position": position,
                "context": {"includeDeclaration": true},
            }),
            _ => json!({"textDocument": document, "position": position}),
        }
    }

    /// Asks the request at the fixture's cursor and renders the answer.
    pub(super) async fn ask(
        self,
        session: &mut Session,
        fixture: &Fixture,
        settles: Settles,
    ) -> String {
        let params = self.params(session, fixture);
        let settled = |answer: &Value| match settles {
            Settles::Immediately => true,
            Settles::WhenAnswered => has_content(answer),
        };
        let answer = session.request_until(self.method(), params, settled).await;
        let view = View { session, fixture };
        let mut lines = match self {
            Self::Definition | Self::TypeDefinition | Self::References => view.locations(&answer),
            Self::DocumentHighlight => view.ranges(&fixture.cursor.path, &answer),
            Self::Completion => view.completion_names(&answer),
            Self::Hover => hover_signature(&answer, &fixture.word_at_cursor()),
            Self::SignatureHelp => signature(&answer),
            Self::PrepareRename => view.rename_target(&answer),
            Self::DocumentSymbol => document_symbols(&answer),
            Self::FoldingRange => folded_lines(&answer),
            Self::WorkspaceSymbol => view.workspace_types(&answer),
        };
        if lines.is_empty() {
            lines.push(NOTHING.to_owned());
        }
        lines.join("\n") + "\n"
    }
}

/// Whether an empty answer is the case's answer or the server still
/// reading what was just opened. A case that expects nothing says so, and
/// is asked once instead of for the whole patience.
#[derive(Clone, Copy)]
pub(super) enum Settles {
    Immediately,
    WhenAnswered,
}

/// `null`, `[]` and an empty completion list are what a server says while
/// it is still reading the workspace.
fn has_content(answer: &Value) -> bool {
    match answer {
        Value::Null => false,
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => fields
            .get("items")
            .and_then(Value::as_array)
            .is_none_or(|items| !items.is_empty()),
        _ => true,
    }
}

/// One answer, or a list of them, as a list.
fn as_list(answer: &Value) -> Vec<&Value> {
    match answer {
        Value::Null => Vec::new(),
        Value::Array(items) => items.iter().collect(),
        single => vec![single],
    }
}

/// What a rendering needs to turn the server's URIs and positions back into
/// the fixture's paths and source lines.
struct View<'a> {
    session: &'a Session,
    fixture: &'a Fixture,
}

impl View<'_> {
    /// `/path:line:column  source line`, one-based, in source order.
    fn locations(&self, answer: &Value) -> Vec<String> {
        let places = as_list(answer).into_iter().filter_map(|location| {
            let uri = location
                .get("uri")
                .or_else(|| location.get("targetUri"))?
                .as_str()?;
            let range = location
                .get("targetSelectionRange")
                .or_else(|| location.get("range"))?;
            Some(Place::new(self.session.display(uri), range))
        });
        self.in_source_order(places.collect())
    }

    /// The ranges of `answer`, all in the file at `path`.
    fn ranges(&self, path: &str, answer: &Value) -> Vec<String> {
        let places = as_list(answer)
            .into_iter()
            .filter_map(|item| Some(Place::new(format!("/{path}"), item.get("range")?)));
        self.in_source_order(places.collect())
    }

    fn in_source_order(&self, mut places: Vec<Place>) -> Vec<String> {
        places.sort();
        places.dedup();
        places.iter().map(|place| self.show(place)).collect()
    }

    fn show(&self, place: &Place) -> String {
        let source = self
            .fixture
            .line(&place.path, place.line)
            .unwrap_or_default();
        format!(
            "{}:{}:{}  {}",
            place.path,
            place.line + 1,
            place.column + 1,
            source.trim()
        )
    }

    /// The names offered, best first, as a client would sort them — only
    /// those the fixture itself spells: what a JDK adds (`hashCode`,
    /// `getClass`, …) depends on the JDK the server found.
    fn completion_names(&self, answer: &Value) -> Vec<String> {
        let items = answer.get("items").unwrap_or(answer);
        let mut offered: Vec<(String, String)> = as_list(items)
            .into_iter()
            .filter_map(|item| {
                let name = identifier_prefix(item["label"].as_str()?);
                let sort_key = item["sortText"].as_str().unwrap_or(name);
                Some((sort_key.to_owned(), name.to_owned()))
            })
            .filter(|(_, name)| self.fixture.spells(name))
            .collect();
        offered.sort();
        let mut names: Vec<String> = offered.into_iter().map(|(_, name)| name).collect();
        names.dedup();
        names
    }

    /// Where the identifier a rename would change starts.
    fn rename_target(&self, answer: &Value) -> Vec<String> {
        let range = answer.get("range").unwrap_or(answer);
        if range.get("start").is_none() {
            return Vec::new();
        }
        let place = Place::new(format!("/{}", self.fixture.cursor.path), range);
        vec![self.show(&place)]
    }

    /// `name  /path:line` of the types found in the fixture's files, sorted.
    /// Whether fields and methods are searchable too differs by server.
    fn workspace_types(&self, answer: &Value) -> Vec<String> {
        let mut lines: Vec<String> = as_list(answer)
            .into_iter()
            .filter(|symbol| matches!(symbol["kind"].as_u64(), Some(5 | 10 | 11)))
            .filter_map(|symbol| {
                let path = self.session.display(symbol["location"]["uri"].as_str()?);
                let line = symbol["location"]["range"]["start"]["line"].as_u64()?;
                self.fixture.line(&path, line as usize)?;
                let name = identifier_prefix(symbol["name"].as_str()?);
                Some(format!("{name}  {path}:{}", line + 1))
            })
            .collect();
        lines.sort();
        lines.dedup();
        lines
    }
}

/// A zero-based position in a file, ordered as the source reads.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Place {
    path: String,
    line: usize,
    column: usize,
}

impl Place {
    fn new(path: String, range: &Value) -> Self {
        let start = &range["start"];
        Self {
            path,
            line: start["line"].as_u64().unwrap_or(0) as usize,
            column: start["character"].as_u64().unwrap_or(0) as usize,
        }
    }
}

/// `scaled` of `scaled(double, int) : Circle`.
fn identifier_prefix(label: &str) -> &str {
    let end = label
        .find(|character: char| !character.is_alphanumeric() && character != '_')
        .unwrap_or(label.len());
    &label[..end]
}

/// The first line of code a hover shows, with `name` unqualified: one
/// server writes `void print(String)`, another `void Printer.print(String)`.
fn hover_signature(answer: &Value, name: &str) -> Vec<String> {
    let text = as_list(&answer["contents"])
        .into_iter()
        .filter_map(|part| part.as_str().or_else(|| part["value"].as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    let signature = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("```"));
    signature
        .map(|line| vec![unqualified(line, name)])
        .unwrap_or_default()
}

/// `line` without the dotted qualifier in front of its first `name`.
fn unqualified(line: &str, name: &str) -> String {
    let Some(at) = line.find(name) else {
        return line.to_owned();
    };
    let qualifier_start = line[..at]
        .rfind(|character: char| !character.is_alphanumeric() && !matches!(character, '_' | '.'))
        .map_or(0, |separator| separator + 1);
    format!("{}{}", &line[..qualifier_start], &line[at..])
}

/// `name(parameter, [active parameter])` of the active signature.
fn signature(answer: &Value) -> Vec<String> {
    let active_signature = answer["activeSignature"].as_u64().unwrap_or(0) as usize;
    let Some(signature) = answer["signatures"].get(active_signature) else {
        return Vec::new();
    };
    let label = signature["label"].as_str().unwrap_or_default();
    let active_parameter = signature["activeParameter"]
        .as_u64()
        .or_else(|| answer["activeParameter"].as_u64());
    let parameters: Vec<String> = as_list(&signature["parameters"])
        .into_iter()
        .enumerate()
        .map(|(index, parameter)| {
            let text = parameter["label"].as_str().unwrap_or("?");
            if active_parameter == Some(index as u64) {
                format!("[{text}]")
            } else {
                text.to_owned()
            }
        })
        .collect();
    vec![format!(
        "{}({})",
        identifier_prefix(label),
        parameters.join(", ")
    )]
}

const PACKAGE: u64 = 4;

/// The outline in source order: `kind name`. Nesting is not shown, nor the
/// package — one server answers a tree under the package and another a flat
/// list of the same declarations.
fn document_symbols(answer: &Value) -> Vec<String> {
    let mut symbols = Vec::new();
    flatten_symbols(answer, &mut symbols);
    symbols.sort_by_key(|symbol| {
        let start = symbol
            .get("selectionRange")
            .unwrap_or(&symbol["location"]["range"])["start"]
            .clone();
        (start["line"].as_u64(), start["character"].as_u64())
    });
    symbols
        .into_iter()
        .map(|symbol| {
            let name = identifier_prefix(symbol["name"].as_str().unwrap_or("?"));
            format!("{} {name}", symbol_kind(symbol["kind"].as_u64()))
        })
        .collect()
}

fn flatten_symbols<'a>(symbols: &'a Value, flat: &mut Vec<&'a Value>) {
    for symbol in as_list(symbols) {
        if symbol["kind"].as_u64() != Some(PACKAGE) {
            flat.push(symbol);
        }
        flatten_symbols(&symbol["children"], flat);
    }
}

fn symbol_kind(kind: Option<u64>) -> &'static str {
    match kind {
        Some(5) => "class",
        Some(6) => "method",
        Some(8) => "field",
        Some(9) => "constructor",
        Some(10) => "enum",
        Some(11) => "interface",
        Some(14) => "constant",
        Some(22) => "enum-member",
        _ => "symbol",
    }
}

/// `first..last` line of each folded region, one-based, sorted.
fn folded_lines(answer: &Value) -> Vec<String> {
    let mut regions: Vec<(u64, u64)> = as_list(answer)
        .into_iter()
        .filter_map(|range| Some((range["startLine"].as_u64()?, range["endLine"].as_u64()?)))
        .collect();
    regions.sort_unstable();
    regions
        .into_iter()
        .map(|(start, end)| format!("{}..{}", start + 1, end + 1))
        .collect()
}
