//! Tree-sitter symbol tools (roadmap item 18): `symbols_overview` and
//! `replace_symbol` for Rust, Python, and TypeScript/TSX.
//!
//! One `symbols_overview` call replaces the search→read wandering a
//! model otherwise spends finding what lives in a file; one
//! `replace_symbol` call replaces the read→edit chain when the change
//! is "rewrite this function". Both address symbols by **name-path**
//! (`Class/method`), which stays stable across edits within a session —
//! unlike line numbers, which every write invalidates.
//!
//! The design follows Serena's tool semantics (the roadmap's named
//! reference for this item), adapted to tree-sitter single-file scope:
//! name-paths match by suffix by default, a leading `/` anchors them at
//! the top level, and an ambiguous or missing match is an error that
//! lists candidates — never a silent pick. Replacement splices the new
//! body in a **single atomic write** (Serena's delete+insert version
//! corrupted files on mid-failure writes; that lesson is baked in), and
//! the candidate buffer passes the same syntax guard `edit_file` uses
//! before anything reaches the disk.
//!
//! Registration is gated (`EngineConfig::symbols`, default off) so the
//! frozen SWE-bench prompt bytes are unchanged until a frontend opts in.

use crate::confinement::{resolve_within, workspace_relative};
use loom_kernel::tool::ToolError;
use std::collections::HashMap;
use std::path::Path;
use tree_sitter::{Node, Parser, Tree};

mod overview;
mod replace;
#[cfg(test)]
mod tests;

pub use overview::SymbolsOverviewTool;
pub use replace::ReplaceSymbolTool;

/// Head-bounded excerpt on a char boundary, for the result echo.
fn head_bytes(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_owned();
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}[truncated]", &text[..end])
}

/// Files above this size are refused rather than walked (matches the
/// edit path's tolerance: a file this large is not a symbol-edit target).
const SYMBOLS_MAX_FILE_BYTES: u64 = 1 << 20;

/// Hard cap on collected symbols per call: a file that declares more is
/// machine-generated, and the tool should say so rather than grind.
const SYMBOLS_MAX_COUNT: usize = 10_000;

/// The languages the symbol tools understand, selected by extension.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SymbolLanguage {
    Rust,
    Python,
    TypeScript,
    Tsx,
}

impl SymbolLanguage {
    /// Maps a file extension to a language; `None` means the symbol
    /// tools do not serve this file (the error path lists the supported
    /// extensions).
    fn for_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            _ => None,
        }
    }

    fn tree_sitter_language(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        }
    }
}

/// One named declaration found in the file, with the byte range of its
/// whole definition (for Python, the *decorated* definition, so
/// replacement cannot orphan decorators).
#[derive(Debug)]
struct Symbol {
    /// Coarse, language-neutral bucket: function, method, class, struct,
    /// enum, trait, interface, type, module, const, impl, macro.
    kind: &'static str,
    /// File-relative name-path, `Outer/inner` (root symbols have no
    /// separator).
    path: String,
    start_byte: usize,
    end_byte: usize,
    /// 1-based, end-inclusive display lines.
    start_line: usize,
    end_line: usize,
    /// Symbols declared inside this one.
    children: Vec<Symbol>,
}

impl Symbol {
    /// `Class/method` style display used in listings.
    fn display(&self) -> String {
        format!("{} (L{}-L{})", self.path, self.start_line, self.end_line)
    }
}

/// A symbol plus its nesting depth, produced by the flattened walk.
struct FlatSymbol {
    path: String,
    kind: &'static str,
    depth: usize,
    start_byte: usize,
    end_byte: usize,
    start_line: usize,
    end_line: usize,
    child_count: usize,
}

impl FlatSymbol {
    /// `Class/method` style display used in listings.
    fn display(&self) -> String {
        format!("{} (L{}-L{})", self.path, self.start_line, self.end_line)
    }
}

/// Parses `source` and flattens every declared symbol with its
/// name-path. Error-tolerant: tree-sitter returns a best-effort tree,
/// and symbols in the intact regions are still addressable.
fn parse_symbols(lang: SymbolLanguage, source: &str) -> Result<(Tree, Vec<FlatSymbol>), String> {
    let mut parser = Parser::new();
    let language = lang.tree_sitter_language();
    parser
        .set_language(&language)
        .map_err(|error| format!("grammar failed to load: {error}"))?;
    let Some(tree) = parser.parse(source, None) else {
        return Err("parser returned no tree".to_owned());
    };
    let mut count = 0usize;
    let roots = collect_children(lang, tree.root_node(), source, "", 0, &mut count)?;
    let mut flat = Vec::with_capacity(count);
    flatten(&roots, 0, &mut flat);
    Ok((tree, flat))
}

/// Enclosing-symbol cues for search hits (roadmap item 31): maps each
/// queried 1-based line to the name-path of the deepest symbol whose
/// span contains it, so a match reads as `path:42: …  [in Class/method]`
/// instead of a bare line number. `None` means the language is
/// unsupported or the file produced no tree; a line missing from the
/// map (imports, whitespace between declarations) carries no cue —
/// honest absence, not a guessed parent.
pub fn enclosing_symbol_cues(
    path: &Path,
    source: &str,
    lines: &[usize],
) -> Option<HashMap<usize, String>> {
    let lang = SymbolLanguage::for_path(path)?;
    let (_, symbols) = parse_symbols(lang, source).ok()?;
    let mut cues = HashMap::new();
    for &line in lines {
        let deepest = symbols
            .iter()
            .filter(|s| s.start_line <= line && line <= s.end_line)
            .max_by_key(|s| s.depth);
        if let Some(deepest) = deepest {
            cues.insert(line, deepest.path.clone());
        }
    }
    Some(cues)
}

/// Walks `node`'s children: classified declarations become symbols
/// (recursing into them for nested declarations); everything else is
/// descended through, so a declaration inside a block or expression is
/// still found — Python nested defs, Rust fns in `mod` blocks.
fn collect_children(
    lang: SymbolLanguage,
    node: Node<'_>,
    source: &str,
    prefix: &str,
    depth: usize,
    count: &mut usize,
) -> Result<Vec<Symbol>, String> {
    if depth > 64 {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return Ok(out);
    }
    loop {
        let child = cursor.node();
        if let Some((kind, name)) = classify(lang, child, source) {
            *count += 1;
            if *count > SYMBOLS_MAX_COUNT {
                return Err(format!(
                    "file declares more than {SYMBOLS_MAX_COUNT} symbols; \
                     it is not a symbol-edit target"
                ));
            }
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            // A Python decorated definition replaces with its decorators:
            // span the wrapper so an edit cannot orphan them.
            let span = python_decorated_span(child);
            let start_pos = span.start_position();
            let end_pos = span.end_position();
            let children = collect_children(lang, child, source, &path, depth + 1, count)?;
            out.push(Symbol {
                kind,
                path,
                start_byte: span.start_byte(),
                end_byte: span.end_byte(),
                start_line: start_pos.row + 1,
                end_line: end_pos.row + 1,
                children,
            });
        } else {
            let nested = collect_children(lang, child, source, prefix, depth + 1, count)?;
            out.extend(nested);
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    Ok(out)
}

/// For a classified Python definition, widens the span to the enclosing
/// `decorated_definition` when present; other languages (and undecorated
/// defs) span the node itself.
fn python_decorated_span(node: Node<'_>) -> Node<'_> {
    if matches!(node.kind(), "function_definition" | "class_definition")
        && node
            .parent()
            .is_some_and(|parent| parent.kind() == "decorated_definition")
    {
        let parent = node.parent();
        if let Some(decorated) = parent {
            return decorated;
        }
    }
    node
}

/// Maps a declaration node to `(kind, name)` for the walk. The name is
/// read from the grammar's `name` field (Rust, Python, TS declarations)
/// or synthesized (`impl` blocks); declarators are classified only when
/// their initializer is a function, so plain variables do not flood the
/// overview.
fn classify(lang: SymbolLanguage, node: Node<'_>, source: &str) -> Option<(&'static str, String)> {
    let field_name = |field: &str| -> Option<String> {
        node.child_by_field_name(field)?
            .utf8_text(source.as_bytes())
            .ok()
            .map(str::to_owned)
    };
    match lang {
        SymbolLanguage::Rust => match node.kind() {
            "function_item" => Some(("function", field_name("name")?)),
            "struct_item" => Some(("struct", field_name("name")?)),
            "enum_item" => Some(("enum", field_name("name")?)),
            "union_item" => Some(("union", field_name("name")?)),
            "trait_item" => Some(("trait", field_name("name")?)),
            "type_item" => Some(("type", field_name("name")?)),
            "mod_item" => Some(("module", field_name("name")?)),
            "const_item" => Some(("const", field_name("name")?)),
            "static_item" => Some(("static", field_name("name")?)),
            "macro_definition" => Some(("macro", field_name("name")?)),
            "impl_item" => {
                // Named by what it implements, not `impl <ty>` — the
                // methods inside then address as `Config/new`, which is
                // what a model naturally types. A trait impl keeps the
                // `Trait for Type` spelling, so `Display for Config`
                // and a plain `Config` impl coexist.
                let self_ty = field_name("type")?;
                let name = match field_name("trait") {
                    Some(trait_name) => format!("{trait_name} for {self_ty}"),
                    None => self_ty,
                };
                Some(("impl", name))
            }
            _ => None,
        },
        SymbolLanguage::Python => match node.kind() {
            "function_definition" => Some(("function", field_name("name")?)),
            "class_definition" => Some(("class", field_name("name")?)),
            _ => None,
        },
        SymbolLanguage::TypeScript | SymbolLanguage::Tsx => match node.kind() {
            "function_declaration" | "generator_function_declaration" => {
                Some(("function", field_name("name")?))
            }
            "class_declaration" | "abstract_class_declaration" => {
                Some(("class", field_name("name")?))
            }
            "interface_declaration" => Some(("interface", field_name("name")?)),
            "type_alias_declaration" => Some(("type", field_name("name")?)),
            "enum_declaration" => Some(("enum", field_name("name")?)),
            "method_definition" => Some(("method", field_name("name")?)),
            "variable_declarator" => {
                let value = node.child_by_field_name("value")?;
                let vk = value.kind();
                if vk == "arrow_function"
                    || vk == "function_expression"
                    || vk == "generator_function_expression"
                {
                    Some(("function", field_name("name")?))
                } else {
                    None
                }
            }
            _ => None,
        },
    }
}

/// Flattens the symbol tree for search and rendering.
fn flatten(symbols: &[Symbol], depth: usize, out: &mut Vec<FlatSymbol>) {
    for symbol in symbols {
        out.push(FlatSymbol {
            path: symbol.path.clone(),
            kind: symbol.kind,
            depth,
            start_byte: symbol.start_byte,
            end_byte: symbol.end_byte,
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            child_count: symbol.children.len(),
        });
        flatten(&symbol.children, depth + 1, out);
    }
}

/// Whether a query name-path addresses `path`: equal, or a suffix on
/// segment boundaries (`Class/method` matches `a/b/Class/method`).
/// A leading `/` anchors the query at the top level.
fn name_path_matches(query: &str, path: &str) -> bool {
    fn split(s: &str) -> Vec<&str> {
        s.split('/').filter(|seg| !seg.is_empty()).collect()
    }
    if let Some(absolute) = query.strip_prefix('/') {
        return absolute == path;
    }
    let query = split(query);
    let path = split(path);
    query.len() <= path.len() && path[path.len() - query.len()..] == query[..]
}

/// Loads and parses the target file for a symbol call. Shared by both
/// tools: language dispatch, size cap, UTF-8 requirement, then the
/// symbol walk.
fn load_symbols(
    root: &Path,
    display_path: &str,
) -> Result<(String, Vec<FlatSymbol>, String), ToolError> {
    let path = resolve_within(root, display_path)?;
    let relative = workspace_relative(root, &path);
    let lang = SymbolLanguage::for_path(&path).ok_or_else(|| {
        ToolError::InvalidInput(format!(
            "symbol tools serve .rs, .py, and .ts/.tsx files; \
             \"{relative}\" is not one"
        ))
    })?;
    let source = read_symbol_source(&path, &relative)?;
    let (_, symbols) = parse_symbols(lang, &source).map_err(ToolError::Execution)?;
    Ok((source, symbols, relative))
}

/// Reads a symbol-tool target with a hard bound on the open file handle.
fn read_symbol_source(path: &Path, relative: &str) -> Result<String, ToolError> {
    // Review E3: bounded read on the handle, not stat-then-read — a
    // file growing between metadata and read used to load unbounded.
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(
            std::fs::File::open(path)
                .map_err(|_| ToolError::InvalidInput(format!("file does not exist: {relative}")))?,
            SYMBOLS_MAX_FILE_BYTES + 1,
        ),
        &mut bytes,
    )
    .map_err(|err| ToolError::Execution(format!("cannot read \"{relative}\": {err}")))?;
    if bytes.len() as u64 > SYMBOLS_MAX_FILE_BYTES {
        return Err(ToolError::InvalidInput(format!(
            "file is too large for symbol tools ({} bytes; \
             limit {SYMBOLS_MAX_FILE_BYTES})",
            bytes.len()
        )));
    }
    let source = String::from_utf8(bytes)
        .map_err(|_| ToolError::Execution(format!("\"{relative}\" is not valid UTF-8")))?;
    Ok(source)
}

/// Head-bounded excerpt of replaced text for the result echo.
fn replaced_head(text: &str, head: usize) -> String {
    head_bytes(text, head)
}
