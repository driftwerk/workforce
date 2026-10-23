//! Definitions found by name in one source file, and replacing one with new
//! text: an edit that addresses code by what it is (`Config::load`,
//! `impl Display for Config`, `Widget.render`) rather than by an exact string.
//!
//! A definition has a core, the item itself from its visibility or `export`
//! keyword on, and a preamble, the doc comments, attributes and decorators
//! directly above it. A replacement that brings no preamble of its own keeps
//! the old one. The result must parse: a replacement that leaves a syntax
//! error at or around itself, or breaks a file that parsed cleanly, is
//! refused and nothing changes.

use std::ops::Range;
use std::path::Path;

use tree_sitter::{Node, Parser, Tree};

/// The grammars definitions are found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grammar {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
}

impl Grammar {
    /// The grammar a file is written in, by its extension.
    pub fn of_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "rs" => Some(Self::Rust),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            _ => None,
        }
    }

    fn language(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        }
    }

    fn separator(self) -> &'static str {
        match self {
            Self::Rust => "::",
            _ => ".",
        }
    }

    fn parse(self, source: &str) -> Result<Tree, DefinitionError> {
        let mut parser = Parser::new();
        parser
            .set_language(&self.language())
            .map_err(|_| DefinitionError::Parse)?;
        parser
            .parse(source.as_bytes(), None)
            .ok_or(DefinitionError::Parse)
    }

    fn is_preamble(self, node: Node, source: &str) -> bool {
        let text = &source[node.byte_range()];
        match (self, node.kind()) {
            (Self::Rust, "attribute_item") => true,
            (Self::Rust, "line_comment" | "block_comment") => {
                !text.starts_with("//!") && !text.starts_with("/*!")
            }
            (Self::Rust, _) => false,
            (_, "comment" | "decorator") => true,
            _ => false,
        }
    }
}

/// One definition in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    /// What it is: `fn`, `struct`, `impl`, `class`, `method`, ...
    pub kind: &'static str,
    /// How it is named in a query: `Config::load`, `impl Display for
    /// Config`, `Widget.render`.
    pub label: String,
    /// Line of its name, 1-based.
    pub line: usize,
    /// Byte offset of its name.
    pub name_offset: usize,
    name: String,
    containers: Vec<Vec<String>>,
    implements: Option<Implements>,
    full: Range<usize>,
    core: Range<usize>,
}

impl Definition {
    /// Lines it spans, preamble included, 1-based and inclusive.
    pub fn lines(&self, source: &str) -> Range<usize> {
        line_of(source, self.full.start)..line_of(source, self.full.end.max(1) - 1)
    }

    fn matches(&self, query: &Query) -> bool {
        match (query, &self.implements) {
            (Query::Impl { trait_name, ty }, Some(implements)) => {
                implements.ty == *ty && implements.trait_name == *trait_name
            }
            (Query::Impl { .. }, None) | (Query::Path(_), Some(_)) => false,
            (Query::Path(segments), None) => {
                let Some((name, outer)) = segments.split_last() else {
                    return false;
                };
                *name == self.name
                    && outer.len() <= self.containers.len()
                    && outer
                        .iter()
                        .rev()
                        .zip(self.containers.iter().rev())
                        .all(|(segment, names)| names.contains(segment))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Implements {
    trait_name: Option<String>,
    ty: String,
}

/// What a query names.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Query {
    /// `name`, `Outer::name` or `Outer.name`: a definition whose innermost
    /// containers match the leading segments.
    Path(Vec<String>),
    /// `impl Type` (inherent) or `impl Trait for Type`.
    Impl {
        trait_name: Option<String>,
        ty: String,
    },
}

impl Query {
    fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        match raw
            .strip_prefix("impl")
            .filter(|rest| rest.starts_with(|c: char| c.is_whitespace() || c == '<'))
        {
            Some(header) => {
                let header = strip_generics(header);
                let (trait_name, ty) = match header.split_once(" for ") {
                    Some((trait_name, ty)) => (Some(base_name(trait_name)), base_name(ty)),
                    None => (None, base_name(&header)),
                };
                (!ty.is_empty()).then_some(Self::Impl { trait_name, ty })
            }
            None => {
                let segments: Vec<String> = raw
                    .split("::")
                    .flat_map(|part| part.split('.'))
                    .map(|segment| segment.trim().to_string())
                    .filter(|segment| !segment.is_empty())
                    .collect();
                (!segments.is_empty()).then_some(Self::Path(segments))
            }
        }
    }
}

/// `text` with every `<...>` group removed and whitespace collapsed.
fn strip_generics(text: &str) -> String {
    let mut depth = 0usize;
    let kept: String = text
        .chars()
        .filter(|c| match c {
            '<' => {
                depth += 1;
                false
            }
            '>' if depth > 0 => {
                depth -= 1;
                false
            }
            _ => depth == 0,
        })
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The last path segment of a type, without references, `dyn` or generics.
fn base_name(text: &str) -> String {
    let text = strip_generics(text);
    let text = text
        .trim()
        .trim_start_matches('&')
        .trim_start_matches("mut ")
        .trim_start_matches("dyn ")
        .trim();
    text.rsplit("::").next().unwrap_or("").trim().to_string()
}

/// Why a definition could not be found or replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefinitionError {
    Parse,
    BadQuery(String),
    NotFound {
        query: String,
        available: Vec<String>,
    },
    Ambiguous {
        query: String,
        candidates: Vec<(String, usize)>,
    },
    EmptyReplacement,
    Unchanged,
    /// The definition holds a syntax error, so where it ends is a guess.
    Broken(String),
    /// The new text would move or rename definitions outside the one named.
    Restructures(String),
    Syntax {
        line: usize,
    },
}

/// Definitions listed in a not-found error at most.
const AVAILABLE_SHOWN: usize = 40;

impl std::fmt::Display for DefinitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse => f.write_str("the file could not be parsed"),
            Self::BadQuery(query) => write!(f, "`{query}` does not name a definition"),
            Self::NotFound { query, available } if available.is_empty() => {
                write!(f, "no definition named `{query}` here, and the file has none it can address")
            }
            Self::NotFound { query, available } => write!(
                f,
                "no definition named `{query}` here; definitions in the file: {}",
                available.join(", ")
            ),
            Self::Ambiguous { query, candidates } => write!(
                f,
                "`{query}` names {} definitions; qualify it or pass the line of the one you mean: {}",
                candidates.len(),
                candidates
                    .iter()
                    .map(|(label, line)| format!("{label} (line {line})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::EmptyReplacement => f.write_str("new_content is empty"),
            Self::Unchanged => f.write_str("new_content is identical to the current definition"),
            Self::Broken(label) => write!(
                f,
                "`{label}` has a syntax error in or right after it, so where it ends is uncertain; fix that with code.edit first"
            ),
            Self::Restructures(label) => write!(
                f,
                "the replacement would change definitions outside `{label}` (are its braces balanced?); nothing was changed"
            ),
            Self::Syntax { line } => write!(
                f,
                "the replacement leaves a syntax error at line {line}; nothing was changed"
            ),
        }
    }
}

impl std::error::Error for DefinitionError {}

/// Every definition in `source` that `query` names.
pub fn find(
    source: &str,
    grammar: Grammar,
    query: &str,
) -> Result<Vec<Definition>, DefinitionError> {
    let parsed = Query::parse(query).ok_or_else(|| DefinitionError::BadQuery(query.into()))?;
    let tree = grammar.parse(source)?;
    Ok(definitions(&tree, source, grammar)
        .into_iter()
        .filter(|definition| definition.matches(&parsed))
        .collect())
}

/// The one definition `query` names, `line` choosing among several: the one
/// whose lines hold it.
pub fn find_one(
    source: &str,
    grammar: Grammar,
    query: &str,
    line: Option<usize>,
) -> Result<Definition, DefinitionError> {
    let parsed = Query::parse(query).ok_or_else(|| DefinitionError::BadQuery(query.into()))?;
    let tree = grammar.parse(source)?;
    select(&tree, source, grammar, query, &parsed, line)
}

fn select(
    tree: &Tree,
    source: &str,
    grammar: Grammar,
    query: &str,
    parsed: &Query,
    line: Option<usize>,
) -> Result<Definition, DefinitionError> {
    let all = definitions(tree, source, grammar);
    let matching: Vec<&Definition> = all.iter().filter(|d| d.matches(parsed)).collect();
    let chosen: Vec<&Definition> = match line {
        Some(line) if matching.len() > 1 => {
            let on_line: Vec<&Definition> = matching
                .iter()
                .copied()
                .filter(|d| d.line == line)
                .collect();
            match on_line.is_empty() {
                false => on_line,
                true => matching
                    .iter()
                    .copied()
                    .filter(|d| d.lines(source).contains(&line) || d.lines(source).end == line)
                    .min_by_key(|d| d.full.len())
                    .into_iter()
                    .collect(),
            }
        }
        _ => matching.clone(),
    };
    match chosen.as_slice() {
        [one] => Ok((*one).clone()),
        [] if matching.is_empty() => Err(DefinitionError::NotFound {
            query: query.into(),
            available: all
                .iter()
                .map(|d| d.label.clone())
                .take(AVAILABLE_SHOWN)
                .collect(),
        }),
        _ => Err(DefinitionError::Ambiguous {
            query: query.into(),
            candidates: matching.iter().map(|d| (d.label.clone(), d.line)).collect(),
        }),
    }
}

/// A replaced definition: the file's new text and where the definition now
/// sits in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replaced {
    pub source: String,
    pub kind: &'static str,
    pub label: String,
    /// First and last line of the new text, 1-based and inclusive.
    pub start_line: usize,
    pub end_line: usize,
    pub lines_removed: usize,
}

/// Replace the definition `query` names with `new_content`.
///
/// `new_content` is the definition as it should read, from its visibility or
/// `export` keyword on; when it starts with a doc comment, attribute or
/// decorator it replaces the old ones too. It is shifted to the column the
/// old definition starts at, and written with the file's line endings.
pub fn replace(
    source: &str,
    grammar: Grammar,
    query: &str,
    line: Option<usize>,
    new_content: &str,
) -> Result<Replaced, DefinitionError> {
    let parsed = Query::parse(query).ok_or_else(|| DefinitionError::BadQuery(query.into()))?;
    let tree = grammar.parse(source)?;
    let definition = select(&tree, source, grammar, query, &parsed, line)?;
    let errors_before = error_nodes(tree.root_node());
    if errors_before
        .iter()
        .any(|node| touches(&node.byte_range(), &definition.full))
    {
        return Err(DefinitionError::Broken(definition.label));
    }

    let content = new_content.replace("\r\n", "\n");
    let content = content.trim_end();
    let content = content.trim_start_matches(['\n', '\r']);
    if content.trim().is_empty() {
        return Err(DefinitionError::EmptyReplacement);
    }
    let range = match brings_preamble(content.trim_start(), grammar) {
        true => definition.full.clone(),
        false => definition.core.clone(),
    };
    // A class field or interface member ends before its `;`, which the new
    // text usually brings again.
    let range = match content.ends_with(';') && source[range.end..].starts_with(';') {
        true => range.start..range.end + 1,
        false => range,
    };
    let indent = indentation_before(source, range.start);
    let text = reindent(content, indent);
    let text = match source.contains("\r\n") {
        true => text.replace('\n', "\r\n"),
        false => text,
    };
    if text == source[range.clone()] {
        return Err(DefinitionError::Unchanged);
    }

    let replaced = format!("{}{}{}", &source[..range.start], text, &source[range.end..]);
    let written = range.start..range.start + text.len();
    let new_tree = grammar.parse(&replaced)?;
    let errors_after = error_nodes(new_tree.root_node());
    let broken = errors_after
        .iter()
        .find(|node| touches(&node.byte_range(), &written))
        .or_else(|| {
            errors_before
                .is_empty()
                .then(|| errors_after.first())
                .flatten()
        });
    if let Some(node) = broken {
        return Err(DefinitionError::Syntax {
            line: line_of(&replaced, node.start_byte()),
        });
    }
    let start = definition.full.start;
    if others(&tree, source, grammar, &(start..range.end))
        != others(&new_tree, &replaced, grammar, &(start..written.end))
    {
        return Err(DefinitionError::Restructures(definition.label));
    }

    let start_line = line_of(&replaced, written.start);
    Ok(Replaced {
        kind: definition.kind,
        label: definition.label,
        start_line,
        end_line: start_line + text.matches('\n').count(),
        lines_removed: source[range].matches('\n').count() + 1,
        source: replaced,
    })
}

fn brings_preamble(content: &str, grammar: Grammar) -> bool {
    match grammar {
        Grammar::Rust => {
            (content.starts_with("//") && !content.starts_with("//!"))
                || (content.starts_with("/*") && !content.starts_with("/*!"))
                || content.starts_with("#[")
        }
        _ => content.starts_with("//") || content.starts_with("/*") || content.starts_with('@'),
    }
}

/// The whitespace `offset` is preceded by on its line, when only whitespace
/// precedes it there.
fn indentation_before(source: &str, offset: usize) -> &str {
    let line_start = source[..offset].rfind('\n').map_or(0, |at| at + 1);
    match starts_line(source, offset) {
        true => &source[line_start..offset],
        false => "",
    }
}

/// Whether only whitespace precedes `offset` on its line.
fn starts_line(source: &str, offset: usize) -> bool {
    let line_start = source[..offset].rfind('\n').map_or(0, |at| at + 1);
    source[line_start..offset]
        .chars()
        .all(|c| c == ' ' || c == '\t')
}

fn leading_whitespace(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

/// `content` moved to `indent`: its common indentation removed and every
/// line after the first indented by `indent`, the first sitting where the old
/// definition started. Content already indented for that spot (an unindented
/// first line, the rest at the definition's column) is kept as it is.
fn reindent(content: &str, indent: &str) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    let non_empty = |line: &&&str| !line.trim().is_empty();
    let first = leading_whitespace(lines[0]);
    let rest = lines[1..]
        .iter()
        .filter(non_empty)
        .map(|l| leading_whitespace(l))
        .min();
    let last = lines
        .iter()
        .rev()
        .find(non_empty)
        .map_or(0, |l| leading_whitespace(l));
    if first == 0 && rest.is_some() && !indent.is_empty() && last == indent.len() {
        return content.to_string();
    }
    let common = rest.map_or(first, |rest| rest.min(first));
    lines
        .iter()
        .enumerate()
        .map(|(at, line)| {
            let line = line
                .get(common.min(leading_whitespace(line))..)
                .unwrap_or("");
            match (at, line.is_empty()) {
                (0, _) | (_, true) => line.to_string(),
                _ => format!("{indent}{line}"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The definitions not wholly inside `within`, by kind and label: what a
/// replacement there must leave as it found it.
fn others(
    tree: &Tree,
    source: &str,
    grammar: Grammar,
    within: &Range<usize>,
) -> Vec<(&'static str, String)> {
    definitions(tree, source, grammar)
        .into_iter()
        .filter(|d| d.full.start < within.start || within.end < d.full.end)
        .map(|d| (d.kind, d.label))
        .collect()
}

/// Whether two ranges overlap or sit against each other.
fn touches(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start <= b.end && b.start <= a.end
}

fn error_nodes(root: Node) -> Vec<Node> {
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.is_error() || node.is_missing() {
            found.push(node);
            continue;
        }
        if node.has_error() {
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
    }
    found.sort_by_key(|node| node.start_byte());
    found
}

fn line_of(source: &str, offset: usize) -> usize {
    source[..offset.min(source.len())].matches('\n').count() + 1
}

/// Every addressable definition in the file, outermost first.
fn definitions(tree: &Tree, source: &str, grammar: Grammar) -> Vec<Definition> {
    let mut found = Vec::new();
    let mut walker = Walker {
        source,
        grammar,
        found: &mut found,
    };
    walker.items(tree.root_node(), &mut Vec::new(), &mut Vec::new());
    found
}

struct Walker<'a, 's> {
    source: &'s str,
    grammar: Grammar,
    found: &'a mut Vec<Definition>,
}

/// One container a definition sits in: the names a query may use for it,
/// and how its label shows it.
type Containers = Vec<Vec<String>>;

impl Walker<'_, '_> {
    fn text(&self, node: Node) -> &str {
        &self.source[node.byte_range()]
    }

    fn items(&mut self, parent: Node, containers: &mut Containers, labels: &mut Vec<String>) {
        let mut cursor = parent.walk();
        let children: Vec<Node> = parent.named_children(&mut cursor).collect();
        for child in children {
            match self.grammar {
                Grammar::Rust => self.rust_item(child, containers, labels),
                _ => self.script_item(child, child, containers, labels),
            }
        }
    }

    fn rust_item(&mut self, node: Node, containers: &mut Containers, labels: &mut Vec<String>) {
        let in_type = node
            .parent()
            .filter(|body| body.kind() == "declaration_list")
            .and_then(|body| body.parent())
            .is_some_and(|owner| matches!(owner.kind(), "impl_item" | "trait_item"));
        let kind = match node.kind() {
            "function_item" | "function_signature_item" if in_type => "method",
            "function_item" | "function_signature_item" => "fn",
            "struct_item" => "struct",
            "enum_item" => "enum",
            "union_item" => "union",
            "type_item" | "associated_type" => "type",
            "const_item" => "const",
            "static_item" => "static",
            "macro_definition" => "macro",
            "mod_item" => "mod",
            "trait_item" => "trait",
            "impl_item" => return self.rust_impl(node, containers, labels),
            _ => return,
        };
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        let name_text = self.text(name).to_string();
        self.push(
            kind, node, node, name, &name_text, containers, labels, None, None,
        );
        if let ("mod" | "trait", Some(body)) = (kind, node.child_by_field_name("body")) {
            containers.push(vec![name_text.clone()]);
            labels.push(name_text);
            self.items(body, containers, labels);
            containers.pop();
            labels.pop();
        }
    }

    fn rust_impl(&mut self, node: Node, containers: &mut Containers, labels: &mut Vec<String>) {
        let Some(ty) = node.child_by_field_name("type") else {
            return;
        };
        let ty_name = base_name(self.text(ty));
        let trait_name = node
            .child_by_field_name("trait")
            .map(|t| base_name(self.text(t)));
        let header = match &trait_name {
            Some(trait_name) => format!("impl {trait_name} for {ty_name}"),
            None => format!("impl {ty_name}"),
        };
        let implements = Implements {
            trait_name: trait_name.clone(),
            ty: ty_name.clone(),
        };
        self.push(
            "impl",
            node,
            node,
            node,
            &header,
            containers,
            labels,
            Some(implements),
            Some(header.clone()),
        );
        if let Some(body) = node.child_by_field_name("body") {
            containers.push(std::iter::once(ty_name.clone()).chain(trait_name).collect());
            labels.push(ty_name);
            self.items(body, containers, labels);
            containers.pop();
            labels.pop();
        }
    }

    /// A TypeScript or JavaScript statement or member. `outer` is what the
    /// definition replaces as a whole: an `export`, `declare` or statement
    /// wrapping exactly this declaration, or the declaration itself.
    fn script_item(
        &mut self,
        outer: Node,
        node: Node,
        containers: &mut Containers,
        labels: &mut Vec<String>,
    ) {
        match node.kind() {
            "export_statement" => {
                if let Some(declaration) = node.child_by_field_name("declaration") {
                    self.script_item(outer, declaration, containers, labels);
                }
                return;
            }
            "ambient_declaration" | "expression_statement" => {
                let mut cursor = node.walk();
                let inner: Vec<Node> = node
                    .named_children(&mut cursor)
                    .filter(|child| !matches!(child.kind(), "comment" | "decorator"))
                    .collect();
                if let [only] = inner.as_slice() {
                    if node.kind() == "ambient_declaration" || only.kind() == "internal_module" {
                        self.script_item(outer, *only, containers, labels);
                    }
                }
                return;
            }
            _ => {}
        }
        let in_class = node
            .parent()
            .is_some_and(|p| matches!(p.kind(), "class_body" | "interface_body" | "object_type"));
        let kind = match node.kind() {
            "function_declaration" | "generator_function_declaration" | "function_signature" => {
                "function"
            }
            "class_declaration" | "abstract_class_declaration" | "class" => "class",
            "interface_declaration" => "interface",
            "type_alias_declaration" => "type",
            "enum_declaration" => "enum",
            "internal_module" | "module" => "namespace",
            "method_definition" | "method_signature" | "abstract_method_signature" if in_class => {
                "method"
            }
            "public_field_definition" | "field_definition" | "property_signature" if in_class => {
                "field"
            }
            "lexical_declaration" | "variable_declaration" => {
                return self.script_variable(outer, node, containers, labels)
            }
            _ => return,
        };
        let Some(name) = node
            .child_by_field_name("name")
            .or_else(|| node.child_by_field_name("property"))
        else {
            return;
        };
        let name_text = self.text(name).trim_matches(['"', '\'']).to_string();
        self.push(
            kind, outer, node, name, &name_text, containers, labels, None, None,
        );
        if let ("class" | "interface" | "namespace", Some(body)) =
            (kind, node.child_by_field_name("body"))
        {
            let segments: Vec<String> = name_text.split('.').map(str::to_string).collect();
            for segment in &segments {
                containers.push(vec![segment.clone()]);
            }
            labels.push(name_text);
            self.items(body, containers, labels);
            for _ in &segments {
                containers.pop();
            }
            labels.pop();
        }
    }

    fn script_variable(
        &mut self,
        outer: Node,
        node: Node,
        containers: &Containers,
        labels: &[String],
    ) {
        let mut cursor = node.walk();
        let declarators: Vec<Node> = node
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "variable_declarator")
            .collect();
        let [declarator] = declarators.as_slice() else {
            return;
        };
        let Some(name) = declarator
            .child_by_field_name("name")
            .filter(|name| name.kind() == "identifier")
        else {
            return;
        };
        let kind = match node.child(0).map(|keyword| self.text(keyword)) {
            Some("let") => "let",
            Some("var") => "var",
            _ => "const",
        };
        let name_text = self.text(name).to_string();
        self.push(
            kind, outer, node, name, &name_text, containers, labels, None, None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        kind: &'static str,
        outer: Node,
        node: Node,
        name: Node,
        name_text: &str,
        containers: &Containers,
        labels: &[String],
        implements: Option<Implements>,
        label: Option<String>,
    ) {
        let core_start = self.core_start(outer, node);
        let full_start = self.preamble_start(outer).min(outer.start_byte());
        let separator = self.grammar.separator();
        let label = label.unwrap_or_else(|| {
            labels
                .iter()
                .map(String::as_str)
                .chain(std::iter::once(name_text))
                .collect::<Vec<_>>()
                .join(separator)
        });
        self.found.push(Definition {
            kind,
            label,
            line: name.start_position().row + 1,
            name_offset: name.start_byte(),
            name: name_text.to_string(),
            containers: containers.clone(),
            implements,
            full: full_start..outer.end_byte(),
            core: core_start..outer.end_byte(),
        });
    }

    /// Where the definition proper begins: past the decorators, comments and
    /// attributes `outer` and `node` open with.
    fn core_start(&self, outer: Node, node: Node) -> usize {
        let first_proper = |owner: Node| {
            let mut cursor = owner.walk();
            let first = owner
                .children(&mut cursor)
                .find(|child| !self.grammar.is_preamble(*child, self.source))
                .map_or(owner.start_byte(), |child| child.start_byte());
            first
        };
        match outer == node {
            true => first_proper(node),
            false => {
                let start = first_proper(outer);
                match start == node.start_byte() {
                    true => first_proper(node),
                    false => start,
                }
            }
        }
    }

    /// Where the doc comments, attributes and decorators directly above
    /// `outer` begin: siblings each on its own line with no blank line
    /// between them and the definition.
    fn preamble_start(&self, outer: Node) -> usize {
        let mut start = outer.start_byte();
        let mut sibling = outer.prev_sibling();
        while let Some(node) = sibling {
            let gap = &self.source[node.end_byte()..start];
            // A Rust doc comment's node takes its line's newline with it.
            let newlines = gap.matches('\n').count() + usize::from(self.text(node).ends_with('\n'));
            let attached = newlines <= 1 && gap.trim().is_empty();
            let own_line = starts_line(self.source, node.start_byte());
            if !(attached && own_line && self.grammar.is_preamble(node, self.source)) {
                break;
            }
            start = node.start_byte();
            sibling = node.prev_sibling();
        }
        start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST: &str = r#"//! Module docs.
#![allow(dead_code)]

use std::fmt;

/// A config.
#[derive(Debug, Clone)]
pub struct Config {
    name: String,
}

impl Config {
    /// Make one.
    pub fn new(name: &str) -> Self {
        Self { name: name.into() }
    }

    fn helper(&self) -> usize {
        json!({ "a": { "b": [1, 2] } });
        1
    }
}

impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

mod inner {
    pub fn new() {}
}

macro_rules! make {
    ($x:expr) => { $x };
}

trait Shape {
    fn area(&self) -> f64;
}
"#;

    fn labels(source: &str, grammar: Grammar) -> Vec<String> {
        let tree = grammar.parse(source).unwrap();
        definitions(&tree, source, grammar)
            .into_iter()
            .map(|d| d.label)
            .collect()
    }

    #[test]
    fn rust_definitions_are_addressed_by_their_qualified_names() {
        assert_eq!(
            labels(RUST, Grammar::Rust),
            [
                "Config",
                "impl Config",
                "Config::new",
                "Config::helper",
                "impl Display for Config",
                "Config::fmt",
                "inner",
                "inner::new",
                "make",
                "Shape",
                "Shape::area",
            ]
        );
    }

    #[test]
    fn a_method_is_replaced_in_place_keeping_its_doc_comment_and_indentation() {
        let replaced = replace(
            RUST,
            Grammar::Rust,
            "Config::new",
            None,
            "pub fn new(name: impl Into<String>) -> Self {\n    Self { name: name.into() }\n}",
        )
        .unwrap();
        assert!(replaced.source.contains(
            "    /// Make one.\n    pub fn new(name: impl Into<String>) -> Self {\n        Self { name: name.into() }\n    }\n"
        ), "{}", replaced.source);
        assert_eq!(replaced.kind, "method");
        assert_eq!(replaced.start_line, 14);
        assert_eq!(replaced.end_line, 16);
        assert_eq!(
            replaced
                .source
                .replace("name: impl Into<String>", "name: &str"),
            RUST
        );
    }

    #[test]
    fn content_already_indented_for_its_place_is_kept_as_written() {
        let content = "    fn helper(&self) -> usize {\n        2\n    }";
        let copied_from_the_file = replace(RUST, Grammar::Rust, "helper", None, content).unwrap();
        let first_line_trimmed =
            replace(RUST, Grammar::Rust, "helper", None, content.trim_start()).unwrap();
        for replaced in [copied_from_the_file, first_line_trimmed] {
            assert!(
                replaced
                    .source
                    .contains("\n    fn helper(&self) -> usize {\n        2\n    }\n}"),
                "{}",
                replaced.source
            );
        }
    }

    #[test]
    fn attributes_and_doc_comments_are_kept_unless_the_replacement_brings_its_own() {
        let kept = replace(
            RUST,
            Grammar::Rust,
            "Config",
            None,
            "pub struct Config {\n    name: String,\n    port: u16,\n}",
        )
        .unwrap();
        assert!(kept
            .source
            .contains("/// A config.\n#[derive(Debug, Clone)]\npub struct Config {\n    name: String,\n    port: u16,\n}"));

        let own = replace(
            RUST,
            Grammar::Rust,
            "Config",
            None,
            "/// The settings.\n#[derive(Debug)]\npub struct Config {\n    name: String,\n}",
        )
        .unwrap();
        assert!(!own.source.contains("A config."));
        assert!(!own.source.contains("Clone"));
        assert!(own
            .source
            .contains("\n\n/// The settings.\n#[derive(Debug)]\npub struct Config"));
        assert!(own
            .source
            .starts_with("//! Module docs.\n#![allow(dead_code)]\n"));
    }

    #[test]
    fn a_comment_a_blank_line_away_is_not_part_of_the_preamble() {
        let source =
            "// ===== helpers =====\n\n/// Adds.\nfn add() {}\n\n/// Stray.\n\nfn sub() {}\n";
        let replaced = replace(
            source,
            Grammar::Rust,
            "add",
            None,
            "/// Adds two.\nfn add() { 2; }",
        )
        .unwrap();
        assert!(replaced
            .source
            .starts_with("// ===== helpers =====\n\n/// Adds two.\nfn add() { 2; }\n"));
        let replaced = replace(
            source,
            Grammar::Rust,
            "sub",
            None,
            "/// Subtracts.\nfn sub() {}",
        )
        .unwrap();
        assert!(
            replaced
                .source
                .ends_with("/// Stray.\n\n/// Subtracts.\nfn sub() {}\n"),
            "{}",
            replaced.source
        );
    }

    #[test]
    fn a_function_holding_macro_braces_is_replaced_whole() {
        let replaced = replace(
            RUST,
            Grammar::Rust,
            "helper",
            None,
            "fn helper(&self) -> usize {\n    json!({ \"c\": {} });\n    3\n}",
        )
        .unwrap();
        assert!(!replaced.source.contains("[1, 2]"));
        assert!(replaced
            .source
            .contains("        json!({ \"c\": {} });\n        3\n    }\n}\n"));
    }

    #[test]
    fn macro_rules_and_impl_blocks_are_addressable() {
        let replaced = replace(
            RUST,
            Grammar::Rust,
            "make",
            None,
            "macro_rules! make {\n    () => { 0 };\n}",
        )
        .unwrap();
        assert!(replaced
            .source
            .contains("macro_rules! make {\n    () => { 0 };\n}"));

        let replaced = replace(
            RUST,
            Grammar::Rust,
            "impl<T> fmt::Display for Config<T>",
            None,
            "impl fmt::Display for Config {\n    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {\n        f.write_str(&self.name)\n    }\n}",
        )
        .unwrap();
        assert_eq!(replaced.kind, "impl");
        assert!(replaced.source.contains("f.write_str(&self.name)"));
        assert!(replaced.source.contains("Self { name: name.into() }"));
    }

    #[test]
    fn an_ambiguous_name_lists_its_candidates_and_a_line_or_qualifier_picks_one() {
        let err = find_one(RUST, Grammar::Rust, "new", None).unwrap_err();
        let DefinitionError::Ambiguous { candidates, .. } = &err else {
            panic!("{err}")
        };
        assert_eq!(
            candidates,
            &[
                ("Config::new".to_string(), 14),
                ("inner::new".to_string(), 31)
            ]
        );
        assert_eq!(
            find_one(RUST, Grammar::Rust, "new", Some(31))
                .unwrap()
                .label,
            "inner::new"
        );
        assert_eq!(
            find_one(RUST, Grammar::Rust, "inner::new", None)
                .unwrap()
                .line,
            31
        );
        assert_eq!(
            find_one(RUST, Grammar::Rust, "Display::fmt", None)
                .unwrap()
                .label,
            "Config::fmt"
        );
    }

    #[test]
    fn a_missing_name_lists_what_the_file_defines() {
        let err = find_one(RUST, Grammar::Rust, "Config::missing", None).unwrap_err();
        assert!(
            err.to_string().contains("Config::new, Config::helper"),
            "{err}"
        );
    }

    #[test]
    fn a_replacement_that_does_not_parse_is_refused() {
        let err = replace(
            RUST,
            Grammar::Rust,
            "helper",
            None,
            "fn helper(&self) -> usize {\n    if true {\n    1\n}",
        )
        .unwrap_err();
        assert!(matches!(err, DefinitionError::Syntax { .. }), "{err}");
        assert_eq!(
            replace(RUST, Grammar::Rust, "inner::new", None, "pub fn new() {}").unwrap_err(),
            DefinitionError::Unchanged
        );
        assert_eq!(
            replace(RUST, Grammar::Rust, "inner::new", None, "  \n").unwrap_err(),
            DefinitionError::EmptyReplacement
        );
    }

    #[test]
    fn crlf_files_keep_their_line_endings() {
        let source = "fn a() {\r\n    1\r\n}\r\n\r\nfn b() {}\r\n";
        let replaced = replace(source, Grammar::Rust, "a", None, "fn a() {\n    2\n}").unwrap();
        assert_eq!(
            replaced.source,
            "fn a() {\r\n    2\r\n}\r\n\r\nfn b() {}\r\n"
        );
    }

    const TS: &str = r#"import { x } from "./x";

/** Adds. */
export function add(a: number, b: number): number {
  return a + b;
}

@Component({ selector: "w" })
export class Widget {
  @Input() size = 1;

  /** Renders. */
  render(): string {
    return "<w>";
  }
}

export const double = (n: number) => n * 2;

namespace Shapes {
  export function area(): number { return 0; }
}

interface Props {
  size: number;
}
"#;

    #[test]
    fn typescript_definitions_are_addressed_by_their_dotted_names() {
        assert_eq!(
            labels(TS, Grammar::TypeScript),
            [
                "add",
                "Widget",
                "Widget.size",
                "Widget.render",
                "double",
                "Shapes",
                "Shapes.area",
                "Props",
                "Props.size",
            ]
        );
    }

    #[test]
    fn a_class_method_is_replaced_keeping_its_comment_and_column() {
        let replaced = replace(
            TS,
            Grammar::TypeScript,
            "Widget.render",
            None,
            "render(): string {\n  return `<w size=${this.size}>`;\n}",
        )
        .unwrap();
        assert!(
            replaced.source.contains(
                "  /** Renders. */\n  render(): string {\n    return `<w size=${this.size}>`;\n  }\n}"
            ),
            "{}",
            replaced.source
        );
    }

    #[test]
    fn an_exported_function_needs_its_export_and_keeps_its_doc() {
        let replaced = replace(
            TS,
            Grammar::TypeScript,
            "add",
            None,
            "export function add(a: number, b: number): number {\n  return b + a;\n}",
        )
        .unwrap();
        assert!(replaced.source.contains(
            "/** Adds. */\nexport function add(a: number, b: number): number {\n  return b + a;\n}"
        ));
    }

    #[test]
    fn decorators_are_kept_unless_the_replacement_brings_its_own() {
        let kept = replace(
            TS,
            Grammar::TypeScript,
            "Widget",
            None,
            "export class Widget {\n  render(): string {\n    return \"\";\n  }\n}",
        )
        .unwrap();
        assert!(kept
            .source
            .contains("@Component({ selector: \"w\" })\nexport class Widget {\n  render()"));
        let own = replace(
            TS,
            Grammar::TypeScript,
            "Widget",
            None,
            "@Component({ selector: \"v\" })\nexport class Widget {}",
        )
        .unwrap();
        assert!(own
            .source
            .contains("\n\n@Component({ selector: \"v\" })\nexport class Widget {}\n"));
    }

    #[test]
    fn tsx_parses_with_its_own_grammar() {
        let source = "export function View() {\n  return <div className=\"a\">{1}</div>;\n}\n";
        let replaced = replace(
            source,
            Grammar::Tsx,
            "View",
            None,
            "export function View() {\n  return <span>{2}</span>;\n}",
        )
        .unwrap();
        assert!(replaced.source.contains("<span>{2}</span>"));
    }

    #[test]
    fn javascript_classes_and_functions_are_addressable() {
        let source = "export class A {\n  x = 1;\n  m() { return 1; }\n}\n";
        assert_eq!(labels(source, Grammar::JavaScript), ["A", "A.x", "A.m"]);
        let replaced = replace(
            source,
            Grammar::JavaScript,
            "A.m",
            None,
            "m() { return 2; }",
        )
        .unwrap();
        assert!(replaced.source.contains("  m() { return 2; }\n}"));
    }

    #[test]
    fn grammars_follow_the_file_extension() {
        assert_eq!(Grammar::of_path(Path::new("a/b.rs")), Some(Grammar::Rust));
        assert_eq!(Grammar::of_path(Path::new("b.tsx")), Some(Grammar::Tsx));
        assert_eq!(
            Grammar::of_path(Path::new("b.mts")),
            Some(Grammar::TypeScript)
        );
        assert_eq!(
            Grammar::of_path(Path::new("b.cjs")),
            Some(Grammar::JavaScript)
        );
        assert_eq!(Grammar::of_path(Path::new("b.py")), None);
    }

    #[test]
    fn a_definition_with_a_syntax_error_in_it_is_not_replaced_by_guesswork() {
        let source = "fn a() {\n    let x = ;\n}\n\nfn b() {}\n";
        let err = replace(source, Grammar::Rust, "a", None, "fn a() {}").unwrap_err();
        assert!(matches!(err, DefinitionError::Broken(_)), "{err}");

        let elsewhere = "fn a() { 1 }\n\nfn b() { 2 }\n\nfn broken( {}\n";
        let replaced = replace(elsewhere, Grammar::Rust, "a", None, "fn a() { 3 }").unwrap();
        assert!(replaced.source.starts_with("fn a() { 3 }\n\nfn b() { 2 }"));
    }

    #[test]
    fn a_replacement_that_moves_other_definitions_is_refused() {
        let broken = "mod m {\n    fn a() {}\n\n    fn b() {}\n}\n\nfn broken( {}\n";
        let err = replace(broken, Grammar::Rust, "m::a", None, "fn a() {} }").unwrap_err();
        assert!(matches!(err, DefinitionError::Restructures(_)), "{err}");

        let clean = "mod m {\n    fn a() {}\n\n    fn b() {}\n}\n";
        let parses_but_moves_b = "fn a() {}\n}\n\nmod n {";
        let err = replace(clean, Grammar::Rust, "m::a", None, parses_but_moves_b).unwrap_err();
        assert!(matches!(err, DefinitionError::Restructures(_)), "{err}");

        let renamed_with_a_helper = replace(
            RUST,
            Grammar::Rust,
            "Config::new",
            None,
            "pub fn build(name: &str) -> Self {\n    Self { name: name.into() }\n}\n\nfn extra() {}",
        )
        .unwrap();
        assert!(renamed_with_a_helper
            .source
            .contains("    /// Make one.\n    pub fn build(name: &str)"));
    }

    #[test]
    fn a_member_ending_in_a_semicolon_does_not_gain_a_second_one() {
        let field = replace(TS, Grammar::TypeScript, "Widget.size", None, "size = 2;").unwrap();
        assert!(
            field.source.contains("  @Input() size = 2;\n\n"),
            "{}",
            field.source
        );
        let member = replace(TS, Grammar::TypeScript, "Props.size", None, "size: string;").unwrap();
        assert!(
            member.source.contains("  size: string;\n}"),
            "{}",
            member.source
        );
        assert_eq!(
            replace(TS, Grammar::TypeScript, "Props.size", None, "size: number;").unwrap_err(),
            DefinitionError::Unchanged
        );
    }

    #[test]
    fn overloads_and_duplicates_need_a_line_and_nested_items_stay_out_of_reach() {
        let source = "function f(a: string): void;\nfunction f(a: number): void;\nfunction f(a: unknown): void {\n  function inner() {}\n}\n";
        let err = replace(source, Grammar::TypeScript, "f", None, "function f() {}").unwrap_err();
        let DefinitionError::Ambiguous { candidates, .. } = &err else {
            panic!("{err}")
        };
        assert_eq!(candidates.len(), 3);
        let replaced = replace(
            source,
            Grammar::TypeScript,
            "f",
            Some(2),
            "function f(a: boolean): void;",
        )
        .unwrap();
        assert!(replaced.source.starts_with(
            "function f(a: string): void;\nfunction f(a: boolean): void;\nfunction f(a: unknown)"
        ));
        assert!(matches!(
            find_one(source, Grammar::TypeScript, "inner", None),
            Err(DefinitionError::NotFound { .. })
        ));
    }

    #[test]
    fn crlf_doc_comments_and_generic_impls_are_found() {
        let source = "/// Doc.\r\n#[inline]\r\nfn a() {}\r\n\r\nimpl<T: Clone> From<Vec<T>> for Wrapper<T> {\r\n    fn from(v: Vec<T>) -> Self { Wrapper(v) }\r\n}\r\n";
        let replaced = replace(source, Grammar::Rust, "a", None, "fn a() { 1; }").unwrap();
        assert!(
            replaced
                .source
                .starts_with("/// Doc.\r\n#[inline]\r\nfn a() { 1; }\r\n\r\nimpl"),
            "{:?}",
            replaced.source
        );
        let own = replace(source, Grammar::Rust, "a", None, "/// New.\nfn a() {}").unwrap();
        assert!(
            own.source.starts_with("/// New.\r\nfn a() {}\r\n\r\nimpl"),
            "{:?}",
            own.source
        );
        assert_eq!(
            find_one(source, Grammar::Rust, "impl From for Wrapper", None)
                .unwrap()
                .kind,
            "impl"
        );
        assert_eq!(
            find_one(source, Grammar::Rust, "Wrapper::from", None)
                .unwrap()
                .label,
            "Wrapper::from"
        );
    }
}
