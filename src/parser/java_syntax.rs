//! Java syntax errors reported where javac reports them.
//!
//! tree-sitter's error recovery is right about *that* a file is broken but
//! not always about *where*: an unclosed string swallows the rest of its
//! line and turns the whole class into one error node, `int x = ;` blames
//! the `=` instead of the missing expression, `int a int b` blames `int b`
//! instead of the missing `,`. And some code javac rejects parses cleanly:
//! `'a` scans as an error but `goto end;` and a method without a return
//! type do not.
//!
//! Three passes put that right:
//!
//! - [`Scanned`] reads the text as javac's scanner does and finds what no
//!   grammar rule sees (unclosed literals and comments, illegal characters
//!   and escapes). It reports each at its place and blanks it out, so the
//!   parser reads the rest of the file as the code it is.
//! - [`describe_node`] words the error and missing nodes whose shape it
//!   knows at javac's place; any other is described the generic way.
//! - [`declaration_errors`] finds what parses but is not Java.
//!
//! Every position is a line and a byte column, as the parser's own.

use tower_lsp::lsp_types::{Position, Range};
use tree_sitter::Node;

use super::ts_to_lsp;
use crate::types::SyntaxError;

/// How much of the code after a lexical problem is its fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// Only itself: the token is blanked and the code around it is sound.
    Itself,
    /// The rest of its line, which the broken token swallowed.
    RestOfLine,
    /// The rest of the file (an unclosed comment).
    RestOfFile,
}

/// Something javac's scanner rejects, with how the text is repaired.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LexicalProblem {
    position: Position,
    message: String,
    /// The bytes replaced, and what by: ASCII of the same length, so every
    /// other position stays where it was.
    replaced: std::ops::Range<usize>,
    replacement: Vec<u8>,
    fault: Fault,
}

/// The Java source as the parser should read it, and the lexical problems
/// that were repaired to make it so.
pub(super) struct Scanned {
    pub(super) text: String,
    problems: Vec<LexicalProblem>,
}

impl Scanned {
    pub(super) fn new(source: &str) -> Self {
        let problems = Scanner::new(source).problems();
        let mut bytes = source.as_bytes().to_vec();
        for problem in &problems {
            bytes[problem.replaced.clone()].copy_from_slice(&problem.replacement);
        }
        let text = String::from_utf8(bytes).unwrap_or_else(|_| source.to_owned());
        Self { text, problems }
    }

    /// Whether the parser has to read [`Self::text`] instead of the source.
    pub(super) fn is_repaired(&self) -> bool {
        !self.problems.is_empty()
    }

    /// `errors` (of the repaired text), without those a lexical problem
    /// caused, with the lexical problems added, in source order.
    pub(super) fn report(&self, errors: Vec<SyntaxError>) -> Vec<SyntaxError> {
        let caused_by_a_problem = |error: &SyntaxError| {
            self.problems
                .iter()
                .any(|problem| problem.caused(error.range, &error.message))
        };
        let mut reported: Vec<SyntaxError> = errors
            .into_iter()
            .filter(|error| !caused_by_a_problem(error))
            .collect();
        reported.extend(self.problems.iter().map(|problem| SyntaxError {
            range: Range::new(problem.position, problem.position),
            message: problem.message.clone(),
        }));
        reported.sort_by_key(|error| (error.range.start.line, error.range.start.character));
        reported
    }
}

impl LexicalProblem {
    /// Whether the error at `range` is this problem's doing: after it on
    /// the stretch it swallowed, or an error node around it.
    fn caused(&self, range: Range, message: &str) -> bool {
        let at = self.position;
        let around_it = range.start <= at && at < range.end;
        let after_it = match self.fault {
            Fault::Itself => false,
            Fault::RestOfLine => range.start.line == at.line && range.start >= at,
            // The comment ate the closing braces of what it is in too.
            Fault::RestOfFile => range.start >= at || closes_something(message),
        };
        around_it || after_it
    }
}

fn closes_something(message: &str) -> bool {
    matches!(message, "missing `}`" | "missing `)`" | "missing `]`")
}

/// Where the scanner is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lexical {
    Code,
    LineComment,
    BlockComment { start: usize },
    String { quote: usize },
    TextBlock,
}

/// javac's scanner, as far as finding what it rejects goes.
struct Scanner<'source> {
    bytes: &'source [u8],
    /// The byte each line starts at.
    line_starts: Vec<usize>,
    problems: Vec<LexicalProblem>,
}

impl<'source> Scanner<'source> {
    fn new(source: &'source str) -> Self {
        let bytes = source.as_bytes();
        let line_starts = std::iter::once(0)
            .chain(
                bytes
                    .iter()
                    .enumerate()
                    .filter(|(_, byte)| **byte == b'\n')
                    .map(|(index, _)| index + 1),
            )
            .collect();
        Self {
            bytes,
            line_starts,
            problems: Vec::new(),
        }
    }

    fn position(&self, byte: usize) -> Position {
        let line = self.line_starts.partition_point(|start| *start <= byte) - 1;
        Position::new(line as u32, (byte - self.line_starts[line]) as u32)
    }

    /// The end of the line `byte` is on, before its `\r\n` or `\n`.
    fn line_end(&self, byte: usize) -> usize {
        let newline = self.bytes[byte..]
            .iter()
            .position(|next| *next == b'\n')
            .map_or(self.bytes.len(), |offset| byte + offset);
        if newline > byte && self.bytes[newline - 1] == b'\r' {
            newline - 1
        } else {
            newline
        }
    }

    fn report(
        &mut self,
        at: usize,
        message: impl Into<String>,
        replaced: std::ops::Range<usize>,
        replacement: Vec<u8>,
        fault: Fault,
    ) {
        self.problems.push(LexicalProblem {
            position: self.position(at),
            message: message.into(),
            replaced,
            replacement,
            fault,
        });
    }

    fn problems(mut self) -> Vec<LexicalProblem> {
        let mut state = Lexical::Code;
        let mut index = 0;
        while index < self.bytes.len() {
            let (next_state, step) = self.step(state, index);
            state = next_state;
            index += step;
        }
        match state {
            Lexical::String { quote } => self.unclosed_string(quote),
            Lexical::BlockComment { start } => self.unclosed_comment(start),
            _ => {}
        }
        self.problems
    }

    /// The state after the byte at `index`, and how many bytes were read.
    fn step(&mut self, state: Lexical, index: usize) -> (Lexical, usize) {
        let byte = self.bytes[index];
        let rest = &self.bytes[index..];
        match state {
            Lexical::Code => self.step_in_code(index),
            Lexical::LineComment if byte == b'\n' => (Lexical::Code, 1),
            Lexical::BlockComment { .. } if rest.starts_with(b"*/") => (Lexical::Code, 2),
            Lexical::TextBlock if rest.starts_with(b"\"\"\"") => (Lexical::Code, 3),
            Lexical::TextBlock if byte == b'\\' => (state, 2),
            Lexical::String { quote } if byte == b'\n' => {
                self.unclosed_string(quote);
                (Lexical::Code, 1)
            }
            Lexical::String { .. } if byte == b'"' => (Lexical::Code, 1),
            Lexical::String { .. } if byte == b'\\' => (state, self.escape(index)),
            _ => (state, 1),
        }
    }

    fn step_in_code(&mut self, index: usize) -> (Lexical, usize) {
        let byte = self.bytes[index];
        let rest = &self.bytes[index..];
        if rest.starts_with(b"//") {
            return (Lexical::LineComment, 2);
        }
        if rest.starts_with(b"/*") {
            return (Lexical::BlockComment { start: index }, 2);
        }
        if rest.starts_with(b"\"\"\"") {
            return (Lexical::TextBlock, 3);
        }
        match byte {
            b'"' => (Lexical::String { quote: index }, 1),
            b'\'' => (Lexical::Code, self.character_literal(index)),
            b'\\' if rest.get(1) == Some(&b'u') => (Lexical::Code, 2),
            b'#' | b'`' | b'\\' => {
                let message = format!("illegal character: `{}`", byte as char);
                self.report(
                    index,
                    message,
                    index..index + 1,
                    b" ".to_vec(),
                    Fault::RestOfLine,
                );
                (Lexical::Code, 1)
            }
            _ => (Lexical::Code, 1),
        }
    }

    /// Reads the escape at `index` (a `\`) in a string; how many bytes it is.
    fn escape(&mut self, index: usize) -> usize {
        let Some(&escaped) = self.bytes.get(index + 1) else {
            return 1;
        };
        let is_valid = matches!(
            escaped,
            b'b' | b't' | b'n' | b'f' | b'r' | b's' | b'"' | b'\'' | b'\\' | b'u' | b'0'..=b'7'
        );
        if escaped == b'\n' {
            return 1;
        }
        if !is_valid {
            let width = utf8_width(escaped);
            self.report(
                index + 1,
                "illegal escape character",
                index..index + 1 + width,
                vec![b' '; 1 + width],
                Fault::Itself,
            );
        }
        1 + utf8_width(escaped)
    }

    /// Reads the character literal opened at `quote`; how many bytes it is.
    /// A broken one is read as `0`, a literal of the same type.
    fn character_literal(&mut self, quote: usize) -> usize {
        let after = &self.bytes[quote + 1..];
        if after.first() == Some(&b'\'') {
            self.report(
                quote,
                "empty character literal",
                quote..quote + 2,
                b"0 ".to_vec(),
                Fault::Itself,
            );
            return 2;
        }
        let content = match after.first() {
            Some(b'\\') => escape_length(after),
            Some(first) if *first != b'\n' && *first != b'\r' => utf8_width(*first),
            _ => 0,
        };
        let closing = quote + 1 + content;
        if content > 0 && self.bytes.get(closing) == Some(&b'\'') {
            return closing + 1 - quote;
        }
        let line_end = self.line_end(quote);
        let mut replacement = vec![b' '; line_end - quote];
        replacement[0] = b'0';
        self.report(
            quote,
            "unclosed character literal",
            quote..line_end,
            replacement,
            Fault::RestOfLine,
        );
        line_end - quote
    }

    /// A `"` string its line ends inside: read as `""`.
    fn unclosed_string(&mut self, quote: usize) {
        let line_end = self.line_end(quote);
        let mut replacement = vec![b' '; line_end - quote];
        if replacement.len() >= 2 {
            replacement[..2].copy_from_slice(b"\"\"");
        }
        self.report(
            quote,
            "unclosed string literal",
            quote..line_end,
            replacement,
            Fault::RestOfLine,
        );
    }

    /// A `/*` the file ends inside: the rest of the file is the comment.
    fn unclosed_comment(&mut self, start: usize) {
        let replacement = self.bytes[start..]
            .iter()
            .map(|byte| if *byte == b'\n' { b'\n' } else { b' ' })
            .collect();
        self.report(
            start,
            "unclosed comment",
            start..self.bytes.len(),
            replacement,
            Fault::RestOfFile,
        );
    }
}

/// How many bytes the escape at the start of `escape` (a `\\`) takes:
/// `\\u0141` (with any number of `u`s), `\\123`, or `\\n`.
fn escape_length(escape: &[u8]) -> usize {
    let Some(&escaped) = escape.get(1) else {
        return 1;
    };
    match escaped {
        b'u' => {
            let us = escape[1..].iter().take_while(|byte| **byte == b'u').count();
            let digits = escape[1 + us..]
                .iter()
                .take(4)
                .take_while(|byte| byte.is_ascii_hexdigit())
                .count();
            1 + us + digits
        }
        b'0'..=b'7' => {
            let longest = if escaped <= b'3' { 3 } else { 2 };
            let digits = escape[1..]
                .iter()
                .take(longest)
                .take_while(|byte| (b'0'..=b'7').contains(*byte))
                .count();
            1 + digits
        }
        _ => 1 + utf8_width(escaped),
    }
}

/// How many bytes the UTF-8 character starting with `first` takes.
fn utf8_width(first: u8) -> usize {
    match first {
        0xF0..=0xFF => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

/// The javac reading of the error or missing node `node`, when its shape is
/// one tree-sitter reports in the wrong place; `None` leaves it to the
/// generic description.
pub(super) fn describe_node(node: Node<'_>, bytes: &[u8]) -> Option<Vec<SyntaxError>> {
    if is_java_tree_sitter_lacks(node, bytes) {
        return Some(Vec::new());
    }
    if node.is_missing() {
        return describe_missing(node).map(|error| vec![error]);
    }
    let one = type_header_without_body(node, bytes)
        .or_else(|| operator_without_operand(node))
        .or_else(|| number_with_trailing_underscore(node, bytes))
        .or_else(|| statement_without_its_start(node, bytes))
        .or_else(|| name_without_separator(node))
        .or_else(|| clause_without_type(node))
        .or_else(|| modifiers_without_declaration(node))
        .or_else(|| creation_without_arguments(node))
        .or_else(|| do_without_while(node));
    one.map(|error| vec![error])
        .or_else(|| condition_without_parentheses(node))
}

/// An error node that is Java tree-sitter-java has no rule for: no error
/// (whether the project's release accepts it is
/// [`super::java_features`]'s to say).
///
/// - `import module java.base;` (Java 25);
/// - `super(…)` or `this(…)` after another statement of a constructor
///   (Java 25), which the grammar only parses as the first;
/// - `0_17`, an octal literal with underscores, which it reads as `0` and a
///   name `_17`.
fn is_java_tree_sitter_lacks(node: Node<'_>, bytes: &[u8]) -> bool {
    if !node.is_error() {
        return false;
    }
    let in_module_import = node
        .parent()
        .and_then(|parent| {
            std::iter::successors(Some(parent), |current| current.parent())
                .find(|current| current.kind() == "import_declaration")
        })
        .and_then(|import| super::java_features::module_import_word(import, bytes))
        .is_some();
    let constructor_call = node.parent().is_some_and(|parent| {
        parent.kind() == "constructor_body"
            && super::java_features::constructor_call_after_error(
                node,
                node.next_named_sibling().as_ref(),
                bytes,
            )
            .is_some()
    });
    in_module_import || constructor_call || is_octal_with_underscores(node, bytes)
}

/// `0_17`: the error is the `0`, the name `_17` right after it.
fn is_octal_with_underscores(node: Node<'_>, bytes: &[u8]) -> bool {
    let zero = is_lone(node, &["decimal_integer_literal"])
        .filter(|zero| zero.utf8_text(bytes).ok() == Some("0"));
    let Some(zero) = zero else {
        return false;
    };
    let Some(digits) = node
        .next_sibling()
        .filter(|next| next.kind() == "identifier")
    else {
        return false;
    };
    let touches = zero.end_byte() == digits.start_byte();
    let digits = digits.utf8_text(bytes).unwrap_or("");
    let is_octal_tail = digits.starts_with('_')
        && digits.ends_with(|char: char| ('0'..='7').contains(&char))
        && digits
            .chars()
            .all(|char| char == '_' || ('0'..='7').contains(&char));
    touches && is_octal_tail
}

fn error_at(position: Position, message: impl Into<String>) -> SyntaxError {
    SyntaxError {
        range: Range::new(position, position),
        message: message.into(),
    }
}

fn start_of(node: Node<'_>) -> Position {
    ts_to_lsp(node.range()).start
}

fn end_of(node: Node<'_>) -> Position {
    ts_to_lsp(node.range()).end
}

/// The children of `node`, tokens included.
fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).collect()
}

/// The first token after `node`.
fn next_token(node: Node<'_>) -> Option<Node<'_>> {
    let next = std::iter::successors(Some(node), |current| current.parent())
        .find_map(|current| current.next_sibling())?;
    std::iter::successors(Some(next), |current| current.child(0)).last()
}

/// `class Main` (or an interface, enum or record) with no `{` after it:
/// the body is missing at the end of the header. `class {` misses the name.
fn type_header_without_body(node: Node<'_>, bytes: &[u8]) -> Option<SyntaxError> {
    let text = node.utf8_text(bytes).ok()?;
    let mut words = text
        .split_whitespace()
        .skip_while(|word| is_declaration_modifier(word));
    let keyword = words.next()?;
    let declares_a_type = matches!(keyword, "class" | "interface" | "enum" | "record");
    if !declares_a_type || text.contains('{') {
        return None;
    }
    let message = if words.next().is_some() {
        "missing `{`"
    } else {
        "missing name"
    };
    Some(error_at(end_of(node), message))
}

fn is_declaration_modifier(word: &str) -> bool {
    matches!(
        word,
        "public"
            | "protected"
            | "private"
            | "abstract"
            | "static"
            | "final"
            | "sealed"
            | "non-sealed"
            | "strictfp"
    )
}

/// The node's only child is a token of kind one of `kinds`.
fn is_lone<'tree>(node: Node<'tree>, kinds: &[&str]) -> Option<Node<'tree>> {
    let only = (node.child_count() == 1).then(|| node.child(0)).flatten()?;
    kinds.contains(&only.kind()).then_some(only)
}

/// `1 + ;`, `int x = ;`, `f(1,, 2)`, `return }`: an operator (or a
/// `return`) with nothing after it. The expression is missing where the
/// next token starts.
fn operator_without_operand(node: Node<'_>) -> Option<SyntaxError> {
    const OPERATORS: &[&str] = &[
        "=", "+=", "-=", "*=", "/=", "+", "-", "*", "/", "%", "&&", "||", "&", "|", "^", "<<",
        ">>", ">>>", "<", ">", "<=", ">=", "==", "!=", ",", "return",
    ];
    is_lone(node, OPERATORS)?;
    let next = next_token(node)?;
    let ends_an_expression = matches!(next.kind(), ";" | ")" | "," | "]" | "}");
    ends_an_expression.then(|| error_at(start_of(next), "missing expression"))
}

/// `1_`: a number may not end in `_` (tree-sitter reads a name after it).
fn number_with_trailing_underscore(node: Node<'_>, bytes: &[u8]) -> Option<SyntaxError> {
    let underscore = node
        .child(0)
        .filter(|first| first.utf8_text(bytes).ok() == Some("_"))?;
    let previous = node.prev_sibling()?;
    let number = std::iter::successors(Some(previous), |current| {
        current.child((current.child_count() as u32).checked_sub(1)?)
    })
    .last()?;
    let touches = number.end_byte() == underscore.start_byte();
    let is_a_number = number.kind().ends_with("_literal") && number.kind().contains("integer");
    (touches && is_a_number).then(|| error_at(start_of(underscore), "illegal underscore"))
}

/// `try { .. }` with no `catch` or `finally`, `else` with no `if`: the
/// statement lacks what makes it one, said at its keyword.
fn statement_without_its_start(node: Node<'_>, bytes: &[u8]) -> Option<SyntaxError> {
    let keyword = node.child(0)?;
    let message = match keyword.kind() {
        "try"
            if !children(node)
                .iter()
                .any(|part| matches!(part.kind(), "catch_clause" | "finally_clause")) =>
        {
            "`try` without `catch` or `finally`"
        }
        "else" | "identifier" if keyword.utf8_text(bytes).ok() == Some("else") => {
            "`else` without `if`"
        }
        _ => return None,
    };
    Some(error_at(start_of(keyword), message))
}

/// A name right after a complete declarator, parameter or expression:
/// `int String name;`, `int a int b`, `A B` in an enum, a call with the
/// next statement on the line below. The separator is missing at the end
/// of what came before.
fn name_without_separator(node: Node<'_>) -> Option<SyntaxError> {
    let first = node.child(0)?;
    if !matches!(first.kind(), "identifier" | "type_identifier") {
        return None;
    }
    let previous = node.prev_sibling().filter(|previous| previous.is_named())?;
    let separator = match node.parent()?.kind() {
        "enum_body" | "formal_parameters" => ",",
        _ => ";",
    };
    Some(error_at(end_of(previous), format!("missing `{separator}`")))
}

/// `extends {`, `throws {`, `implements A, {`: the type is missing before
/// the body. javac places it after `throws` (a name is expected there) and
/// at the `{` otherwise (no type starts with it).
fn clause_without_type(node: Node<'_>) -> Option<SyntaxError> {
    let clause = is_lone(node, &["extends", "implements", "throws", "permits", ","])?;
    let next = next_token(node).filter(|next| next.kind() == "{")?;
    let at = if clause.kind() == "throws" {
        end_of(clause)
    } else {
        start_of(next)
    };
    Some(error_at(at, "missing type"))
}

/// `public }`: modifiers with no member after them.
fn modifiers_without_declaration(node: Node<'_>) -> Option<SyntaxError> {
    let all_modifiers = node.child_count() > 0
        && children(node)
            .iter()
            .all(|child| is_declaration_modifier(child.kind()) || child.kind() == "modifiers");
    if !all_modifiers {
        return None;
    }
    let next = next_token(node).filter(|next| next.kind() == "}")?;
    Some(error_at(start_of(next), "missing declaration"))
}

/// `new Object;`: the arguments are missing after the type.
fn creation_without_arguments(node: Node<'_>) -> Option<SyntaxError> {
    let parts = children(node);
    let after_new = parts.iter().position(|part| part.kind() == "new")? + 1;
    let created = parts.get(after_new..)?;
    let only_the_type = !created.is_empty()
        && created.iter().all(|part| {
            matches!(
                part.kind(),
                "identifier" | "type_identifier" | "scoped_type_identifier" | "generic_type"
            )
        });
    only_the_type.then(|| error_at(end_of(node), "missing `(`"))
}

/// `do { .. }` with no `while (..)` after its body.
fn do_without_while(node: Node<'_>) -> Option<SyntaxError> {
    let parts = children(node);
    if parts.first()?.kind() != "do" || parts.iter().any(|part| part.kind() == "while") {
        return None;
    }
    let body = parts.iter().find(|part| part.kind() == "block")?;
    Some(error_at(end_of(*body), "missing `while`"))
}

/// `if x > 0) {`, `while true {`: the condition's `(` is missing after the
/// keyword, and its `)` at its end when that is missing too.
fn condition_without_parentheses(node: Node<'_>) -> Option<Vec<SyntaxError>> {
    let parts = children(node);
    let keyword = *parts.first()?;
    let opens = parts.get(1).is_some_and(|part| part.kind() == "(");
    if !matches!(keyword.kind(), "if" | "while") || opens || parts.len() < 2 {
        return None;
    }
    let mut errors = vec![error_at(end_of(keyword), "missing `(`")];
    if !parts.iter().any(|part| part.kind() == ")") {
        errors.push(error_at(end_of(node), "missing `)`"));
    }
    Some(errors)
}

/// A missing node tree-sitter places where javac does not: the argument
/// after a comma (`f(1, )`, a "missing identifier" to tree-sitter), and the
/// `>` of a type argument list, which javac expects where the next token
/// is.
fn describe_missing(node: Node<'_>) -> Option<SyntaxError> {
    let in_arguments = node
        .parent()
        .is_some_and(|parent| parent.kind() == "argument_list");
    match node.kind() {
        "identifier" if in_arguments => Some(error_at(start_of(node), "missing expression")),
        "identifier" => Some(error_at(start_of(node), "missing name")),
        ">" => {
            let next = next_token(node)?;
            Some(error_at(start_of(next), "missing `>`"))
        }
        _ => None,
    }
}

/// An error in code that parses but is not Java, and the stretch of code
/// whose other errors it explains.
pub(super) struct DeclarationError {
    pub(super) error: SyntaxError,
    pub(super) explains: Range,
}

/// What parses but javac rejects: a constructor named unlike its class (a
/// method without a return type), and `goto` or `const` used as a name.
pub(super) fn declaration_errors(root: Node<'_>, bytes: &[u8]) -> Vec<DeclarationError> {
    let mut found = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        found.extend(method_without_return_type(node, bytes));
        found.extend(reserved_word_as_name(node, bytes));
        pending.extend(children(node).into_iter().filter(|child| child.is_named()));
    }
    found.sort_by_key(|found| {
        (
            found.error.range.start.line,
            found.error.range.start.character,
        )
    });
    found
}

/// `run() {}` in `class Main`: tree-sitter reads a constructor, javac a
/// method missing its return type.
fn method_without_return_type(node: Node<'_>, bytes: &[u8]) -> Option<DeclarationError> {
    if node.kind() != "constructor_declaration" {
        return None;
    }
    let name = node.child_by_field_name("name")?;
    let owner = node
        .parent()
        .filter(|body| body.kind() == "class_body" || body.kind() == "enum_body_declarations")
        .and_then(|body| {
            std::iter::successors(body.parent(), |current| current.parent())
                .find(|current| current.kind() != "enum_body")
        })?;
    let owner_name = owner
        .child_by_field_name("name")
        .and_then(|owner_name| owner_name.utf8_text(bytes).ok());
    if owner_name.is_some() && owner_name == name.utf8_text(bytes).ok() {
        return None;
    }
    Some(DeclarationError {
        error: error_at(start_of(name), "missing return type"),
        explains: ts_to_lsp(name.range()),
    })
}

/// `goto end;`: `goto` and `const` are reserved, not names. The statement
/// they are in is theirs to explain.
fn reserved_word_as_name(node: Node<'_>, bytes: &[u8]) -> Option<DeclarationError> {
    if !matches!(node.kind(), "identifier" | "type_identifier") {
        return None;
    }
    let word = node.utf8_text(bytes).ok()?;
    if !matches!(word, "goto" | "const") {
        return None;
    }
    let statement = std::iter::successors(Some(node), |current| current.parent())
        .find(|current| {
            matches!(
                current.kind(),
                "local_variable_declaration" | "expression_statement" | "field_declaration"
            )
        })
        .unwrap_or(node);
    Some(DeclarationError {
        error: error_at(start_of(node), format!("`{word}` is a reserved word")),
        explains: ts_to_lsp(statement.range()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(line, column, message)` of each lexical problem, 0-based.
    fn problems(source: &str) -> Vec<(u32, u32, String)> {
        Scanner::new(source)
            .problems()
            .into_iter()
            .map(|problem| {
                (
                    problem.position.line,
                    problem.position.character,
                    problem.message,
                )
            })
            .collect()
    }

    #[test]
    fn closed_literals_and_comments_are_no_problem() {
        let source = "String a = \"x\\\"y\";\nchar c = '\"';\nchar d = '\\n';\n/* \" ' */\n";
        assert!(problems(source).is_empty());
    }

    #[test]
    fn a_string_its_line_ends_inside_is_unclosed_at_its_quote() {
        assert_eq!(
            problems("int a;\n  return \"open; }\n}\n"),
            vec![(1, 9, "unclosed string literal".to_owned())]
        );
    }

    #[test]
    fn quotes_in_comments_and_text_blocks_open_nothing() {
        let source = "// a \" quote\n/* \" */\nString t = \"\"\"\n  \"one\n  \"\"\";\n";
        assert!(problems(source).is_empty());
    }

    #[test]
    fn character_literals_empty_or_unclosed() {
        assert_eq!(
            problems("char a = '';\nchar b = 'b;\n"),
            vec![
                (0, 9, "empty character literal".to_owned()),
                (1, 9, "unclosed character literal".to_owned()),
            ]
        );
    }

    #[test]
    fn an_unclosed_comment_and_illegal_characters_and_escapes() {
        assert_eq!(
            problems("int a = 1 # 2;\nString s = \"a\\q\";\n/* open\n"),
            vec![
                (0, 10, "illegal character: `#`".to_owned()),
                (1, 14, "illegal escape character".to_owned()),
                (2, 0, "unclosed comment".to_owned()),
            ]
        );
    }

    #[test]
    fn unicode_and_octal_escapes_close_their_character_literal() {
        assert!(problems(
            "char a = '\\u0141';\nchar b = '\\uuu00e9';\nchar c = '\\377';\nchar d = '\\7';\n"
        )
        .is_empty());
    }

    #[test]
    fn a_unicode_escape_in_code_is_no_illegal_character() {
        assert!(problems("int \\u0061 = 1;\n").is_empty());
    }

    #[test]
    fn the_repaired_text_keeps_every_other_byte_in_place() {
        let scanned = Scanned::new("a(\"open\n)\nchar c = 'x;\n");
        assert_eq!(scanned.text, "a(\"\"   \n)\nchar c = 0  \n");
        assert!(scanned.is_repaired());
    }
}
