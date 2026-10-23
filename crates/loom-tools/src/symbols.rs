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

use crate::budget::ToolBudgets;
use crate::confinement::{resolve_within, workspace_relative};
use futures::future::BoxFuture;
use loom_kernel::tool::{
    PathTouch, Tool, ToolContext, ToolEffects, ToolError, ToolOutput, ToolSpec,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use std::sync::OnceLock;
use tree_sitter::{Node, Parser, Tree};

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
    let size = std::fs::metadata(&path)
        .map_err(|_| ToolError::InvalidInput(format!("file does not exist: {relative}")))?
        .len();
    if size > SYMBOLS_MAX_FILE_BYTES {
        return Err(ToolError::InvalidInput(format!(
            "file is too large for symbol tools ({size} bytes; \
             limit {SYMBOLS_MAX_FILE_BYTES})"
        )));
    }
    let bytes = std::fs::read(&path)
        .map_err(|err| ToolError::Execution(format!("cannot read \"{relative}\": {err}")))?;
    let source = String::from_utf8(bytes)
        .map_err(|_| ToolError::Execution(format!("\"{relative}\" is not valid UTF-8")))?;
    let (_, symbols) = parse_symbols(lang, &source).map_err(ToolError::Execution)?;
    Ok((source, symbols, relative))
}

/// Head-bounded excerpt of replaced text for the result echo.
fn replaced_head(text: &str, head: usize) -> String {
    head_bytes(text, head)
}

// ---------------------------------------------------------------------------
// symbols_overview
// ---------------------------------------------------------------------------

/// Lists the symbols a file declares, with name-paths and line ranges —
/// the one-call replacement for the search→read wander.
/// Tree-sitter-powered symbol survey: a bounded, indexed outline of one
/// file's definitions (roadmap item 18).
#[derive(Debug, Default)]
pub struct SymbolsOverviewTool {
    /// Output budgets (item 20.2): the overview byte cap before the
    /// kind-count fallback. `Default` keeps the constants.
    budgets: ToolBudgets,
}

impl SymbolsOverviewTool {
    /// Builds the tool under explicit output budgets (the engine's
    /// entry point).
    pub fn with_budgets(mut self, budgets: ToolBudgets) -> Self {
        self.budgets = budgets;
        self
    }
}

#[derive(Deserialize, JsonSchema)]
struct OverviewInput {
    /// Workspace-relative path of the file to survey.
    path: String,
    /// How many nesting levels below each top-level symbol to list
    /// (0–3; default 0 — top-level only, children counted).
    #[serde(default)]
    depth: Option<u32>,
}

impl Tool for SymbolsOverviewTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "symbols_overview".to_owned(),
            description: "Lists the symbols (functions, classes, structs, \
                          impls, interfaces, …) a Rust, Python, or \
                          TypeScript file declares, as name-paths with line \
                          ranges. One call replaces the search→read chain of \
                          finding what lives in a file. Use the name-path \
                          (e.g. `Class/method`) with `replace_symbol`."
                .to_owned(),
            parameters: crate::schema::parameters::<OverviewInput>(),
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn touched_paths(&self, input: &serde_json::Value) -> Option<Vec<PathTouch>> {
        Some(
            match serde_json::from_value::<OverviewInput>(input.clone()) {
                Ok(parsed) => vec![PathTouch::read(parsed.path)],
                Err(_) => Vec::new(),
            },
        )
    }

    fn failure_hint(&self, failure: loom_kernel::transcript::ToolFailure) -> Option<String> {
        if matches!(failure, loom_kernel::transcript::ToolFailure::InvalidInput) {
            return Some(
                "symbols_overview serves .rs, .py, .ts/.mts/.cts, and .tsx \
                 files with a workspace-relative `path`; call it on one file \
                 at a time."
                    .to_owned(),
            );
        }
        None
    }

    fn call<'a>(
        &'a self,
        input: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: OverviewInput = serde_json::from_value(input)
                .map_err(|err| ToolError::InvalidInput(format!("invalid arguments: {err}")))?;
            let depth = input.depth.unwrap_or(0).min(3);
            let root = ctx.workspace_root.clone();
            let display = input.path.clone();
            let (source, symbols, relative) =
                tokio::task::spawn_blocking(move || load_symbols(&root, &display))
                    .await
                    .map_err(|err| {
                        ToolError::Execution(format!("overview task failed: {err}"))
                    })??;

            let visible: Vec<&FlatSymbol> = symbols
                .iter()
                .filter(|symbol| symbol.depth <= depth as usize)
                .collect();
            let content =
                match render_overview(&relative, &visible, depth, symbols.len(), &self.budgets) {
                    Some(rendered) => rendered,
                    None => {
                        // Over budget: fall back to kind counts (Serena's
                        // progressive shortening) instead of truncating the
                        // listing mid-symbol.
                        render_kind_counts(&relative, &symbols)
                    }
                };
            let _ = source;
            Ok(ToolOutput::new(content))
        })
    }
}

/// Renders the visible symbols; `None` when the rendering would exceed
/// the byte budget (caller falls back to kind counts).
fn render_overview(
    relative: &str,
    visible: &[&FlatSymbol],
    depth: u32,
    total: usize,
    budgets: &ToolBudgets,
) -> Option<String> {
    let mut out = format!("{relative} — {total} symbols (depth {depth})\n");
    for symbol in visible {
        let line = format!(
            "{indent}{} {} (L{}-L{}){}\n",
            symbol.kind,
            symbol.path,
            symbol.start_line,
            symbol.end_line,
            if symbol.child_count > 0 && symbol.depth == depth as usize {
                let plural = if symbol.child_count == 1 { "" } else { "ren" };
                format!(" [{} child{plural} — raise depth]", symbol.child_count)
            } else {
                String::new()
            },
            indent = "  ".repeat(symbol.depth + 1),
        );
        if out.len() + line.len() > budgets.symbols_overview_max_bytes {
            return None;
        }
        out.push_str(&line);
    }
    Some(out)
}

/// Kind-count fallback for files whose full listing overflows the
/// budget: names are dropped, buckets remain.
fn render_kind_counts(relative: &str, symbols: &[FlatSymbol]) -> String {
    let mut counts: std::collections::BTreeMap<&'static str, usize> = Default::default();
    for symbol in symbols {
        *counts.entry(symbol.kind).or_default() += 1;
    }
    let parts: Vec<String> = counts
        .into_iter()
        .map(|(kind, count)| format!("{kind}s: {count}"))
        .collect();
    format!(
        "{relative} — too many symbols to list; by kind: {}. \
         Narrow with `depth: 0` or edit by name-path with `replace_symbol`.",
        parts.join(", ")
    )
}

// ---------------------------------------------------------------------------
// replace_symbol
// ---------------------------------------------------------------------------

/// Rewrites one symbol's definition by name-path — the one-call
/// replacement for the read→edit chain when the change is "rewrite this
/// function".
#[derive(Debug, Default)]
pub struct ReplaceSymbolTool {
    /// Output budgets (item 20.2): the replaced-definition echo head.
    pub(crate) budgets: ToolBudgets,
}

impl ReplaceSymbolTool {
    /// Builds the tool under explicit output budgets (the engine's
    /// registration path; item 20.2).
    pub fn with_budgets(mut self, budgets: ToolBudgets) -> Self {
        self.budgets = budgets;
        self
    }
}

#[derive(Deserialize, JsonSchema)]
struct ReplaceInput {
    /// Workspace-relative path of the file holding the symbol.
    path: String,
    /// Name-path addressing the symbol (`Class/method`). Suffix match by
    /// default; a leading `/` anchors at the top level.
    name_path: String,
    /// The full replacement definition, including the signature and
    /// decorators where the language uses them. Must parse on its own.
    body: String,
}

impl Tool for ReplaceSymbolTool {
    fn spec(&self) -> &ToolSpec {
        static SPEC: OnceLock<ToolSpec> = OnceLock::new();
        SPEC.get_or_init(|| ToolSpec {
            name: "replace_symbol".to_owned(),
            description: "Replaces one symbol's whole definition in a Rust, \
                          Python, or TypeScript file, addressed by name-path \
                          (e.g. `Class/method`, suffix-matched; leading `/` \
                          anchors at top level). `body` is the complete new \
                          definition including its signature. One atomic \
                          write; the file must still parse. Find name-paths \
                          with `symbols_overview`."
                .to_owned(),
            parameters: crate::schema::parameters::<ReplaceInput>(),
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn touched_paths(&self, input: &serde_json::Value) -> Option<Vec<PathTouch>> {
        Some(
            match serde_json::from_value::<ReplaceInput>(input.clone()) {
                Ok(parsed) => vec![PathTouch::write(parsed.path)],
                Err(_) => Vec::new(),
            },
        )
    }

    fn failure_hint(&self, failure: loom_kernel::transcript::ToolFailure) -> Option<String> {
        if matches!(failure, loom_kernel::transcript::ToolFailure::InvalidInput) {
            return Some(
                "call `symbols_overview` on the file first; address the \
                 symbol by its listed name-path (`Class/method` — extend it \
                 if several symbols share the suffix), and pass the complete \
                 new definition including the signature as `body`."
                    .to_owned(),
            );
        }
        None
    }

    fn call<'a>(
        &'a self,
        input: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: ReplaceInput = serde_json::from_value(input)
                .map_err(|err| ToolError::InvalidInput(format!("invalid arguments: {err}")))?;
            let root = ctx.workspace_root.clone();
            let display = input.path.clone();
            let name_path = input.name_path.clone();
            let body = input.body;
            let (relative, new_text, replaced, matches) = tokio::task::spawn_blocking(move || {
                replace_symbol(&root, &display, &name_path, &body)
            })
            .await
            .map_err(|err| ToolError::Execution(format!("replace task failed: {err}")))??;

            let content = format!(
                "replaced {} in {relative} ({} byte body); previous \
                 definition started with:\n{}",
                matches.display(),
                new_text.len(),
                replaced_head(&replaced, self.budgets.replace_symbol_head_bytes),
            );
            Ok(ToolOutput::new(content)
                .with_effects(ToolEffects::none().with_file_written(relative)))
        })
    }
}

/// The whole replace pipeline, minus the write: resolves, parses,
/// matches, splices, and guards. Returns the workspace-relative path,
/// the candidate text, the replaced original text, and the matched
/// symbol's display.
fn replace_symbol(
    root: &Path,
    display_path: &str,
    name_path: &str,
    body: &str,
) -> Result<(String, String, String, Symbol), ToolError> {
    let path = resolve_within(root, display_path)?;
    let relative = workspace_relative(root, &path);
    let lang = SymbolLanguage::for_path(&path).ok_or_else(|| {
        ToolError::InvalidInput(format!(
            "symbol tools serve .rs, .py, and .ts/.tsx files; \
             \"{relative}\" is not one"
        ))
    })?;
    let size = std::fs::metadata(&path)
        .map_err(|_| ToolError::InvalidInput(format!("file does not exist: {relative}")))?
        .len();
    if size > SYMBOLS_MAX_FILE_BYTES {
        return Err(ToolError::InvalidInput(format!(
            "file is too large for symbol tools ({size} bytes; \
             limit {SYMBOLS_MAX_FILE_BYTES})"
        )));
    }
    let bytes = std::fs::read(&path)
        .map_err(|err| ToolError::Execution(format!("cannot read \"{relative}\": {err}")))?;
    let source = String::from_utf8(bytes)
        .map_err(|_| ToolError::Execution(format!("\"{relative}\" is not valid UTF-8")))?;
    let (_, flat) = parse_symbols(lang, &source).map_err(ToolError::Execution)?;

    let matches: Vec<&FlatSymbol> = flat
        .iter()
        .filter(|symbol| name_path_matches(name_path, &symbol.path))
        .collect();
    let matched = match matches.as_slice() {
        [] => {
            let available: Vec<String> = flat
                .iter()
                .filter(|s| s.depth == 0)
                .map(|s| s.path.clone())
                .collect();
            return Err(ToolError::InvalidInput(format!(
                "no symbol matches name_path \"{name_path}\" in {relative}; \
                 top-level symbols: {}",
                summarize_paths(&available)
            )));
        }
        [one] => *one,
        many => {
            let listed: Vec<String> = many.iter().map(|s| s.display()).collect();
            return Err(ToolError::InvalidInput(format!(
                "name_path \"{name_path}\" matches {} symbols: {}; \
                 extend the path to disambiguate",
                many.len(),
                listed.join(", ")
            )));
        }
    };

    let replaced = source[matched.start_byte..matched.end_byte].to_owned();
    let mut new_text = String::with_capacity(source.len() - replaced.len() + body.len());
    new_text.push_str(&source[..matched.start_byte]);
    new_text.push_str(body);
    new_text.push_str(&source[matched.end_byte..]);

    // Same gate as edit_file: refuse a candidate that introduces a
    // parse error the original did not have.
    crate::syntax::guard_against(&source, &new_text, &path).map_err(|err| {
        ToolError::InvalidInput(format!(
            "refusing to write \"{relative}\": the replacement body does \
             not parse — {err}"
        ))
    })?;

    // Single atomic write (Serena's two-write version corrupted files
    // when the process died between delete and insert).
    std::fs::write(&path, &new_text)
        .map_err(|err| ToolError::Execution(format!("cannot write \"{relative}\": {err}")))?;
    Ok((relative, new_text, replaced, symbol_from_flat(matched)))
}

/// Rebuilds a minimal `Symbol` for the result display.
fn symbol_from_flat(flat: &FlatSymbol) -> Symbol {
    Symbol {
        kind: flat.kind,
        path: flat.path.clone(),
        start_byte: flat.start_byte,
        end_byte: flat.end_byte,
        start_line: flat.start_line,
        end_line: flat.end_line,
        children: Vec::new(),
    }
}

/// Compact listing of available paths for a no-match error.
fn summarize_paths(paths: &[String]) -> String {
    const SHOWN: usize = 24;
    if paths.len() <= SHOWN {
        paths.join(", ")
    } else {
        format!("{} … ({} total)", paths[..SHOWN].join(", "), paths.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loom_kernel::tool::ToolContext;

    fn ctx_for(root: &std::path::Path) -> ToolContext {
        ToolContext::new(
            root.to_owned(),
            tokio_util::sync::CancellationToken::new(),
            std::time::Duration::from_secs(30),
        )
    }

    fn temp_root() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize");
        (dir, root)
    }

    async fn call_overview(root: &std::path::Path, input: serde_json::Value) -> String {
        SymbolsOverviewTool::default()
            .call(input, &ctx_for(root))
            .await
            .expect("overview succeeds")
            .content
    }

    async fn call_replace(root: &std::path::Path, input: serde_json::Value) -> String {
        ReplaceSymbolTool::default()
            .call(input, &ctx_for(root))
            .await
            .expect("replace succeeds")
            .content
    }

    fn err_of(root: &std::path::Path, input: serde_json::Value) -> String {
        futures::executor::block_on(ReplaceSymbolTool::default().call(input, &ctx_for(root)))
            .expect_err("replace must fail")
            .to_string()
    }

    // ---- name-path semantics ----

    #[test]
    fn name_path_matches_suffix_and_absolute() {
        assert!(name_path_matches("method", "Class/method"));
        assert!(name_path_matches("Class/method", "a/b/Class/method"));
        assert!(name_path_matches("Class/method", "Class/method"));
        assert!(!name_path_matches("Class", "Class/method"));
        assert!(!name_path_matches("method", "Class/other"));
        assert!(name_path_matches("/Class/method", "Class/method"));
        assert!(!name_path_matches("/Class/method", "a/Class/method"));
    }

    // ---- Rust ----

    const RUST_SRC: &str = r#"//! module docs
use std::fmt;

const LIMIT: usize = 10;

pub struct Config {
    pub name: String,
}

impl Config {
    pub fn new(name: String) -> Self {
        Self { name }
    }

    fn helper(&self) -> usize {
        LIMIT
    }
}

pub fn main() {
    let inner = || 1;
    let _ = inner();
}
"#;

    #[tokio::test]
    async fn rust_overview_lists_top_level_and_children() {
        let (dir, root) = temp_root();
        std::fs::write(root.join("lib.rs"), RUST_SRC).expect("seed");
        let out = call_overview(&root, serde_json::json!({"path": "lib.rs", "depth": 1})).await;
        assert!(out.contains("const LIMIT (L4-L4)"), "{out}");
        assert!(out.contains("struct Config (L6-L8)"), "{out}");
        assert!(out.contains("impl Config (L10-L18)"), "{out}");
        assert!(out.contains("function Config/new (L11-L13)"), "{out}");
        assert!(out.contains("function main (L20-L23)"), "{out}");
        drop(dir);
    }

    #[tokio::test]
    async fn rust_overview_depth_zero_counts_children() {
        let (dir, root) = temp_root();
        std::fs::write(root.join("lib.rs"), RUST_SRC).expect("seed");
        let out = call_overview(&root, serde_json::json!({"path": "lib.rs"})).await;
        assert!(out.contains("impl Config (L10-L18) [2 children"), "{out}");
        assert!(!out.contains("Config/new"), "{out}");
        drop(dir);
    }

    #[tokio::test]
    async fn rust_replace_function_and_impl_method() {
        let (dir, root) = temp_root();
        std::fs::write(root.join("lib.rs"), RUST_SRC).expect("seed");
        call_replace(
            &root,
            serde_json::json!({
                "path": "lib.rs",
                "name_path": "Config/helper",
                "body": "    fn helper(&self) -> usize {\n        LIMIT + 1\n    }"
            }),
        )
        .await;
        let text = std::fs::read_to_string(root.join("lib.rs")).expect("read");
        assert!(text.contains("LIMIT + 1"), "{text}");
        assert!(text.contains("pub fn new"), "sibling untouched");
        assert_eq!(
            text.matches("fn helper").count(),
            1,
            "one definition remains"
        );

        // Top-level by bare name.
        call_replace(
            &root,
            serde_json::json!({
                "path": "lib.rs",
                "name_path": "main",
                "body": "pub fn main() {\n    println!(\"replaced\");\n}"
            }),
        )
        .await;
        let text = std::fs::read_to_string(root.join("lib.rs")).expect("read");
        assert!(text.contains("replaced"), "{text}");
        drop(dir);
    }

    #[tokio::test]
    async fn rust_ambiguous_and_missing_match_errors() {
        let (dir, root) = temp_root();
        let src = "fn dup() {}\nmod a {\n    fn dup() {}\n}\n";
        std::fs::write(root.join("lib.rs"), src).expect("seed");
        let err = err_of(
            &root,
            serde_json::json!({"path": "lib.rs", "name_path": "dup", "body": "fn dup() {}"}),
        );
        assert!(err.contains("matches 2 symbols"), "{err}");
        assert!(err.contains("a/dup"), "{err}");

        let err = err_of(
            &root,
            serde_json::json!({"path": "lib.rs", "name_path": "nope", "body": "fn nope() {}"}),
        );
        assert!(err.contains("no symbol matches"), "{err}");
        assert!(err.contains("dup"), "{err}");
        assert!(err.contains("a"), "{err}");
        drop(dir);
    }

    #[tokio::test]
    async fn rust_absolute_path_anchors_top_level() {
        let (dir, root) = temp_root();
        std::fs::write(root.join("lib.rs"), RUST_SRC).expect("seed");
        // `new` alone resolves; `/new` must not (it lives under Config).
        let err = err_of(
            &root,
            serde_json::json!({"path": "lib.rs", "name_path": "/new", "body": "fn new() {}"}),
        );
        assert!(err.contains("no symbol matches"), "{err}");
        drop(dir);
    }

    // ---- Python ----

    const PYTHON_SRC: &str = r#"
import os


def top_level(x):
    return x


class Greeter:
    """A greeter."""

    def greet(self):
        def nested():
            return "hi"
        return nested()

    @property
    def name(self):
        return "g"


@decorator
def decorated():
    return 1
"#;

    #[tokio::test]
    async fn python_overview_and_nested_defs() {
        let (dir, root) = temp_root();
        std::fs::write(root.join("app.py"), PYTHON_SRC).expect("seed");
        let out = call_overview(&root, serde_json::json!({"path": "app.py", "depth": 2})).await;
        assert!(out.contains("function top_level (L5-L6)"), "{out}");
        assert!(out.contains("class Greeter (L9-L19)"), "{out}");
        assert!(out.contains("function Greeter/greet (L12-L15)"), "{out}");
        assert!(
            out.contains("function Greeter/greet/nested (L13-L14)"),
            "{out}"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn python_replace_includes_decorators_in_span() {
        let (dir, root) = temp_root();
        std::fs::write(root.join("app.py"), PYTHON_SRC).expect("seed");
        call_replace(
            &root,
            serde_json::json!({
                "path": "app.py",
                "name_path": "Greeter/name",
                "body": "    @property\n    def name(self):\n        return \"ga\""
            }),
        )
        .await;
        let text = std::fs::read_to_string(root.join("app.py")).expect("read");
        assert!(text.contains("\"ga\""), "{text}");
        assert_eq!(
            text.matches("@property").count(),
            1,
            "one decorator remains"
        );
        drop(dir);
    }

    #[tokio::test]
    async fn python_replacing_decorated_symbol_requires_body_with_decorator() {
        let (dir, root) = temp_root();
        std::fs::write(root.join("app.py"), PYTHON_SRC).expect("seed");
        // Body without the decorator: the old decorator (inside the
        // replaced span) disappears with it — the model's body is the
        // whole definition.
        call_replace(
            &root,
            serde_json::json!({
                "path": "app.py",
                "name_path": "decorated",
                "body": "def decorated():\n    return 2"
            }),
        )
        .await;
        let text = std::fs::read_to_string(root.join("app.py")).expect("read");
        assert!(text.contains("return 2"), "{text}");
        assert!(
            !text.contains("@decorator"),
            "decorator was part of the span"
        );
        drop(dir);
    }

    // ---- TypeScript / TSX ----

    const TS_SRC: &str = r#"
export interface Point {
    x: number;
    y: number;
}

export type Id = string;

export enum Color {
    Red,
    Green,
}

export class Canvas {
    draw(p: Point): void {
        const helper = () => 1;
        helper();
    }

    get size(): number {
        return 0;
    }
}

export function render(id: Id): string {
    return id;
}

const makePoint = (x: number, y: number): Point => ({ x, y });

let plain = 3;
"#;

    #[tokio::test]
    async fn typescript_overview_kinds() {
        let (dir, root) = temp_root();
        std::fs::write(root.join("canvas.ts"), TS_SRC).expect("seed");
        let out = call_overview(&root, serde_json::json!({"path": "canvas.ts", "depth": 1})).await;
        assert!(out.contains("interface Point (L2-L5)"), "{out}");
        assert!(out.contains("type Id (L7-L7)"), "{out}");
        assert!(out.contains("enum Color (L9-L12)"), "{out}");
        assert!(out.contains("class Canvas (L14-L23)"), "{out}");
        assert!(out.contains("method Canvas/draw (L15-L18)"), "{out}");
        assert!(out.contains("method Canvas/size (L20-L22)"), "{out}");
        assert!(out.contains("function render (L25-L27)"), "{out}");
        assert!(out.contains("function makePoint (L29-L29)"), "{out}");
        assert!(!out.contains("plain"), "plain variables are not symbols");
        drop(dir);
    }

    #[tokio::test]
    async fn typescript_replace_arrow_function_symbol() {
        let (dir, root) = temp_root();
        std::fs::write(root.join("canvas.ts"), TS_SRC).expect("seed");
        call_replace(
            &root,
            serde_json::json!({
                "path": "canvas.ts",
                "name_path": "makePoint",
                "body": "const makePoint = (x: number, y: number): Point => ({ x, y, z: 0 } as Point);"
            }),
        )
        .await;
        let text = std::fs::read_to_string(root.join("canvas.ts")).expect("read");
        assert!(text.contains("z: 0"), "{text}");
        drop(dir);
    }

    #[tokio::test]
    async fn tsx_file_dispatches_to_tsx_grammar() {
        let (dir, root) = temp_root();
        std::fs::write(
            root.join("widget.tsx"),
            "export function App() {\n    return <div>hi</div>;\n}\n",
        )
        .expect("seed");
        let out = call_overview(&root, serde_json::json!({"path": "widget.tsx"})).await;
        assert!(out.contains("function App (L1-L3)"), "{out}");
        drop(dir);
    }

    // ---- shared behavior ----

    #[tokio::test]
    async fn unsupported_extension_and_missing_file_error() {
        let (dir, root) = temp_root();
        std::fs::write(root.join("x.go"), "package x\n").expect("seed");
        let err = err_of(
            &root,
            serde_json::json!({"path": "x.go", "name_path": "x", "body": "y"}),
        );
        assert!(err.contains(".rs, .py"), "{err}");

        let err = err_of(
            &root,
            serde_json::json!({"path": "nope.rs", "name_path": "x", "body": "y"}),
        );
        assert!(err.contains("does not exist"), "{err}");
        drop(dir);
    }

    #[tokio::test]
    async fn replacement_that_breaks_parsing_is_refused() {
        let (dir, root) = temp_root();
        std::fs::write(root.join("lib.rs"), "fn ok() {}\n").expect("seed");
        let err = err_of(
            &root,
            serde_json::json!({"path": "lib.rs", "name_path": "ok", "body": "fn broken( {"}),
        );
        assert!(err.contains("refusing to write"), "{err}");
        // Nothing reached the disk.
        let text = std::fs::read_to_string(root.join("lib.rs")).expect("read");
        assert_eq!(text, "fn ok() {}\n");
        drop(dir);
    }

    #[tokio::test]
    async fn confinement_escape_is_refused() {
        let (dir, root) = temp_root();
        std::fs::write(root.join("inside.rs"), "fn a() {}\n").expect("seed");
        let err = err_of(
            &root,
            serde_json::json!({"path": "../outside.rs", "name_path": "a", "body": "fn a() {}"}),
        );
        assert!(err.to_lowercase().contains("workspace"), "{err}");
        drop(dir);
    }

    #[tokio::test]
    async fn overview_falls_back_to_kind_counts_on_huge_listings() {
        let (dir, root) = temp_root();
        // 6_000 functions: each line ~30 bytes → ~180 KB, far over the
        // 8 KB budget; the fallback keeps the response bounded.
        let mut src = String::new();
        for i in 0..6_000 {
            src.push_str(&format!("fn f{i}() {{}}\n"));
        }
        std::fs::write(root.join("big.rs"), src).expect("seed");
        let out = call_overview(&root, serde_json::json!({"path": "big.rs"})).await;
        assert!(out.contains("too many symbols to list"), "{out}");
        assert!(out.contains("functions: 6000"), "{out}");
        drop(dir);
    }

    #[tokio::test]
    async fn overview_reports_read_touch_replace_reports_write_touch() {
        let read_touch = SymbolsOverviewTool::default()
            .touched_paths(&serde_json::json!({"path": "lib.rs"}))
            .expect("touches");
        assert_eq!(read_touch.len(), 1);
        assert!(matches!(
            read_touch[0].access,
            loom_kernel::tool::PathAccess::Read
        ));
        let write_touch = ReplaceSymbolTool::default()
            .touched_paths(&serde_json::json!({"path": "lib.rs", "name_path": "a", "body": "b"}))
            .expect("touches");
        assert!(matches!(
            write_touch[0].access,
            loom_kernel::tool::PathAccess::Write
        ));
    }
}
