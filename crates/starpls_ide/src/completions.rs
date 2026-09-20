//! Partially replicates the "completions" API in the LSP specification.

use std::collections::HashSet;

use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::find_node::CoveringNode;
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::token::Tokens;
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::Expr;
use ruff_python_ast::ExprRef;
use ruff_python_ast::ModModule;
use ruff_text_size::Ranged;
use rustc_hash::FxHashMap;
use starpls_common::parsed_module;
use starpls_common::Db as _;
use starpls_common::File;
use starpls_common::LoadItemCandidateKind;
use starpls_hir::Db;
use starpls_hir::Semantics;
use starpls_syntax::source::expr_range;
use starpls_syntax::source::string_value;
use starpls_syntax::source::suite_range;
use starpls_syntax::TextRange;
use starpls_syntax::TextSize;
use ty_python_core::definition::DefinitionKind;
use ty_python_core::global_scope;
use ty_python_core::scope::FileScopeId;
use ty_python_core::scope::NodeWithScopeRef;
use ty_python_core::semantic_index;
use ty_python_semantic::types::ide_support::call_signature_details;
use ty_python_semantic::types::list_members::all_end_of_scope_members;
use ty_python_semantic::types::Type;
use ty_python_semantic::HasType;
use ty_python_semantic::ObjectMembers;
use ty_python_semantic::SemanticModel;

use crate::selection::Selection;
use crate::util::pick_source_token;
use crate::util::CursorToken;
use crate::Database;
use crate::FilePosition;

const COMPLETION_MARKER: &str = "__STARPLS_COMPLETION_MARKER";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CompletionItem {
    pub label: String,
    pub kind: CompletionItemKind,
    pub mode: Option<CompletionMode>,
    pub filter_text: Option<String>,
    relevance: CompletionRelevance,
}

impl CompletionItem {
    pub fn sort_text(&self) -> String {
        format!("{}-{}", self.relevance as u16, self.label)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Edit {
    TextEdit(TextEdit),
    InsertReplaceEdit(InsertReplaceEdit),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TextEdit {
    pub range: TextRange,
    pub new_text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InsertReplaceEdit {
    pub new_text: String,
    pub insert: TextRange,
    pub replace: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CompletionMode {
    InsertText(String),
    TextEdit(Edit),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CompletionItemKind {
    Function,
    Field,
    Variable,
    Module,
    Keyword,
    File,
    Folder,
    Constant,
}

#[repr(u16)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum CompletionRelevance {
    Parameter,
    VariableOrKeyword,
    Builtin,
}

enum CompletionAnalysis<'a> {
    Name(NameContext<'a>),
    NameRef(NameRefContext),
    String(StringContext),
}

enum NameContext<'a> {
    Def,
    Dot { receiver_ty: Type<'a> },
}

struct NameRefContext {
    names: Vec<CompletionItem>,
    params: Vec<String>,
    is_in_def: bool,
    is_in_for: bool,
    is_lone_expr: bool,
    is_loop_variable: bool,
}

enum StringContext {
    // A recognized string context must not fall through to marker completion
    // when its module, dictionary keys, or decoded value are unavailable.
    Unavailable,
    LoadModule {
        file_id: File,
        text: Box<str>,
        body_start: TextSize,
    },
    LoadItem {
        loaded_file: File,
    },
    DictKey {
        keys: Vec<String>,
    },
    Label {
        file_id: File,
        text: Box<str>,
    },
}

struct CompletionContext<'a> {
    analysis: CompletionAnalysis<'a>,
}

pub(crate) fn completions(
    db: &Database,
    pos: FilePosition,
    trigger_character: Option<String>,
) -> Option<Vec<CompletionItem>> {
    let model = SemanticModel::new(db, db.starlark_program_file(pos.file_id));
    let ctx = CompletionContext::new(db, pos, trigger_character.clone())?;
    let mut items = Vec::new();

    match ctx.analysis {
        CompletionAnalysis::NameRef(NameRefContext {
            names,
            params,
            is_lone_expr,
            is_in_def,
            is_in_for,
            is_loop_variable,
        }) => {
            for name in params {
                items.push(CompletionItem {
                    label: format!("{name}="),
                    kind: CompletionItemKind::Variable,
                    mode: Some(CompletionMode::InsertText(format!("{name} = "))),
                    relevance: CompletionRelevance::Parameter,
                    filter_text: None,
                });
            }

            if !is_loop_variable {
                add_globals(&mut items);
                items.extend(names);

                if is_lone_expr {
                    add_keywords(&mut items, is_in_def, is_in_for);
                }
            }
        }
        CompletionAnalysis::Name(NameContext::Dot { receiver_ty }) => {
            for member in model.member_completions(receiver_ty, ObjectMembers::Exclude) {
                if member.is_type_check_only {
                    continue;
                }
                items.push(CompletionItem {
                    label: member.name.to_string(),
                    kind: if member.ty.is_some_and(|ty| {
                        crate::hover::is_function_type(ty)
                            || matches!(ty, Type::ClassLiteral(_) | Type::GenericAlias(_))
                    }) {
                        CompletionItemKind::Function
                    } else {
                        CompletionItemKind::Field
                    },
                    mode: None,
                    relevance: CompletionRelevance::VariableOrKeyword,
                    filter_text: None,
                })
            }
        }
        CompletionAnalysis::String(StringContext::LoadModule {
            file_id,
            text: value,
            body_start: token_start,
        }) => {
            for candidate in db.list_load_candidates(&value, file_id).ok()?? {
                let start = TextSize::from(
                    value
                        .rfind(['/', ':', '@'])
                        .map(|start| {
                            if candidate.replace_trailing_slash {
                                start
                            } else {
                                start + 1
                            }
                        })
                        .unwrap_or(0) as u32,
                );
                let end = TextSize::from(value.len() as u32);
                let (edit, filter_text) = if candidate.replace_trailing_slash {
                    (
                        Edit::InsertReplaceEdit(InsertReplaceEdit {
                            new_text: candidate.path.clone(),
                            insert: TextRange::new(token_start + start, token_start + end),
                            replace: TextRange::new(token_start + start, token_start + end),
                        }),
                        Some("/".to_string()),
                    )
                } else {
                    (
                        Edit::TextEdit(TextEdit {
                            range: TextRange::new(token_start + start, token_start + end),
                            new_text: candidate.path.clone(),
                        }),
                        None,
                    )
                };

                items.push(CompletionItem {
                    label: candidate.path,
                    kind: match candidate.kind {
                        LoadItemCandidateKind::Directory => CompletionItemKind::Folder,
                        LoadItemCandidateKind::File => CompletionItemKind::File,
                    },
                    mode: Some(CompletionMode::TextEdit(edit)),
                    relevance: CompletionRelevance::VariableOrKeyword,
                    filter_text,
                });
            }
        }
        CompletionAnalysis::String(StringContext::Unavailable) => return None,
        CompletionAnalysis::String(StringContext::LoadItem { loaded_file }) => {
            let file = db.starlark_program_file(loaded_file);
            let mut names = FxHashMap::default();
            for declaration in all_end_of_scope_members(db, global_scope(db, file)) {
                let member = declaration.member;
                if member.name.starts_with('_')
                    || member.is_type_check_only
                    || matches!(
                        declaration.first_reachable_definition.kind(db),
                        DefinitionKind::ProvidedBinding(_)
                    )
                {
                    continue;
                }
                names
                    .entry(member.name.to_string())
                    .or_insert_with(|| lexical_item(member.name.to_string(), Some(member.ty)));
            }
            items.extend(names.into_values());
        }
        CompletionAnalysis::String(StringContext::DictKey { keys }) => {
            for key in keys {
                items.push(CompletionItem {
                    label: key,
                    kind: CompletionItemKind::Constant,
                    mode: None,
                    relevance: CompletionRelevance::VariableOrKeyword,
                    filter_text: None,
                });
            }
        }

        CompletionAnalysis::String(StringContext::Label { file_id, text }) => {
            if matches!(trigger_character.as_deref(), Some("@")) {
                return None;
            }

            let package = db.resolve_build_file(file_id).unwrap_or_default();
            let is_relative = text.starts_with(':');
            let prefix = strip_last_package_or_target(&text);
            let has_target = text.contains(':');
            let mut seen_packages = HashSet::<&str>::new();

            for target in db.get_all_workspace_targets().iter() {
                let remaining = match if is_relative {
                    target
                        .strip_prefix("//")
                        .and_then(|res| res.strip_prefix(&package))
                        .and_then(|res| res.strip_prefix(prefix))
                } else {
                    target.strip_prefix(prefix)
                } {
                    Some(remaining) => remaining,
                    None => continue,
                };

                if has_target {
                    items.push(CompletionItem {
                        label: remaining.to_string(),
                        kind: CompletionItemKind::Field,
                        mode: None,
                        relevance: CompletionRelevance::VariableOrKeyword,
                        filter_text: None,
                    });
                } else if let Some(index) = remaining.find(['/', ':']) {
                    let package = &remaining[..index];
                    if !package.is_empty() && !seen_packages.contains(package) {
                        seen_packages.insert(package);
                        items.push(CompletionItem {
                            label: package.to_string(),
                            kind: CompletionItemKind::Folder,
                            mode: None,
                            relevance: CompletionRelevance::VariableOrKeyword,
                            filter_text: None,
                        });
                    }
                }
            }
        }
        _ => {}
    }

    Some(items)
}

pub(crate) fn add_globals(items: &mut Vec<CompletionItem>) {
    let add_global = &mut |global: &'static str| {
        items.push(CompletionItem {
            label: global.to_string(),
            kind: CompletionItemKind::Keyword,
            mode: None,
            relevance: CompletionRelevance::VariableOrKeyword,
            filter_text: None,
        })
    };
    add_global("True");
    add_global("False");
    add_global("None");
}

fn add_keywords(items: &mut Vec<CompletionItem>, is_in_def: bool, is_in_for: bool) {
    let add_keyword = &mut |keyword: &'static str| {
        items.push(CompletionItem {
            label: keyword.to_string(),
            kind: CompletionItemKind::Keyword,
            mode: None,
            relevance: CompletionRelevance::VariableOrKeyword,
            filter_text: None,
        })
    };
    add_keyword("def");
    add_keyword("if");
    add_keyword("for");
    add_keyword("load");
    add_keyword("pass");
    add_keyword("lambda");

    if is_in_def {
        add_keyword("return");
    }

    if is_in_for {
        add_keyword("break");
        add_keyword("continue");
    }
}

fn string_context(
    db: &Database,
    file: File,
    module: &ModModule,
    tokens: &Tokens,
    source: &str,
    pos: ruff_text_size::TextSize,
) -> Option<StringContext> {
    let sema = Semantics::new(db);
    let CursorToken::Token(token) =
        pick_source_token(tokens, pos, ruff_text_size::TextSize::of(source), |_| 0)?
    else {
        return None;
    };
    if token.kind() != TokenKind::String || token.unwrap_string_flags().is_byte_string() {
        return None;
    }
    let node = covering_node(module.into(), token.range());
    match crate::selection::classify(&node, token.range())? {
        Selection::LoadModule(call) => {
            if !sema.is_load_stmt(file, call) {
                return None;
            }
            Some(match string_value(&source[token.range()]) {
                Some((text, offset)) => StringContext::LoadModule {
                    file_id: file,
                    text,
                    body_start: TextSize::from(u32::from(token.start())) + TextSize::from(offset),
                },
                None => StringContext::Unavailable,
            })
        }
        Selection::LoadItem { call, item: _ } => Some(match sema.resolve_load_stmt(file, call) {
            Some(loaded_file) => StringContext::LoadItem { loaded_file },
            None => StringContext::Unavailable,
        }),
        Selection::String(expr) => {
            if !sema.contains_expr(file, expr.into()) {
                return None;
            }
            let parent = node
                .ancestors()
                .skip_while(|node| !matches!(node, AnyNodeRef::ExprStringLiteral(_)))
                .nth(1);
            if let Some(AnyNodeRef::ExprSubscript(index)) = parent {
                if index.slice.range() == expr.range()
                    && expr_range(&index.slice, index.into(), tokens) == expr.range()
                {
                    let model = SemanticModel::new(db, db.starlark_program_file(file));
                    let keys: Vec<_> = model
                        .expected_string_literal_completions(expr)
                        .into_iter()
                        .map(|candidate| candidate.value)
                        .collect();
                    return Some(if keys.is_empty() {
                        StringContext::Unavailable
                    } else {
                        StringContext::DictKey { keys }
                    });
                }
            }
            let (text, _) = string_value(&source[token.range()])?;
            if text.starts_with("//") || text.starts_with(':') {
                Some(StringContext::Label {
                    file_id: file,
                    text,
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Completion's marker is not lowered. Follow the source positions represented
/// by Starlark syntax, excluding Python-only children of otherwise valid nodes.
fn marker_is_starlark(node: &CoveringNode<'_>, tokens: &Tokens) -> bool {
    for (child, parent) in node.ancestors().zip(node.ancestors().skip(1)) {
        if let Some(expr) = child.as_expr_ref() {
            let wrapper = match expr {
                ExprRef::Starred(_) => matches!(parent, AnyNodeRef::Arguments(_)),
                ExprRef::Slice(_) => matches!(parent, AnyNodeRef::ExprSubscript(_)),
                _ => false,
            };
            if !wrapper && !starpls_syntax::supports_expr(expr, tokens) {
                return false;
            }
        }
        let represented = match parent {
            AnyNodeRef::StmtFunctionDef(def) => match child {
                AnyNodeRef::Identifier(_) => true,
                AnyNodeRef::Parameters(_) => true,
                _ => suite_range(tokens, &def.body, def.parameters.end(), def.end(), None)
                    .is_some_and(|range| range.contains_range(child.range())),
            },
            AnyNodeRef::StmtIf(stmt) => {
                child.range() == stmt.test.range()
                    || matches!(child, AnyNodeRef::ElifElseClause(_))
                    || suite_range(
                        tokens,
                        &stmt.body,
                        stmt.test.end(),
                        stmt.end(),
                        stmt.elif_else_clauses.first().map(Ranged::start),
                    )
                    .is_some_and(|range| range.contains_range(child.range()))
            }
            AnyNodeRef::ElifElseClause(clause) => {
                clause
                    .test
                    .as_ref()
                    .is_some_and(|test| test.range() == child.range())
                    || suite_range(
                        tokens,
                        &clause.body,
                        clause.test.as_ref().map_or(clause.start(), Ranged::end),
                        clause.end(),
                        None,
                    )
                    .is_some_and(|range| range.contains_range(child.range()))
            }
            AnyNodeRef::StmtFor(stmt) => {
                child.range() == stmt.target.range()
                    || child.range() == stmt.iter.range()
                    || (stmt.body.iter().any(|stmt| stmt.range() == child.range())
                        && suite_range(tokens, &stmt.body, stmt.iter.end(), stmt.end(), None)
                            .is_some())
            }
            AnyNodeRef::StmtAssign(_) => true,
            AnyNodeRef::StmtAugAssign(_) => true,
            AnyNodeRef::StmtReturn(_) => true,
            AnyNodeRef::StmtExpr(stmt) => {
                if let Expr::Call(call) = stmt.value.as_ref() {
                    if matches!(call.func.as_ref(), Expr::Name(name) if name.id == "load") {
                        let alias = call.arguments.keywords.iter().any(|keyword| {
                            keyword
                                .arg
                                .as_ref()
                                .is_some_and(|arg| arg.range() == node.node().range())
                        });
                        if !alias {
                            return false;
                        }
                    }
                }
                true
            }
            AnyNodeRef::Parameter(param) => !param
                .annotation
                .as_ref()
                .is_some_and(|annotation| annotation.range() == child.range()),
            AnyNodeRef::ExprDict(dict) => !dict
                .items
                .iter()
                .any(|item| item.key.is_none() && item.value.range() == child.range()),
            _ => !parent.is_statement(),
        };
        if !represented {
            return false;
        }
    }
    true
}

/// Reconnect an unaffected receiver or callee from the marker parse. Node
/// indices are local to each parse, so only the canonical result may be used
/// for semantic queries. Matching both range and kind rejects recovery changes.
fn original_expr<'a>(
    module: &'a ModModule,
    modified: &Expr,
    insertion: ruff_text_size::TextSize,
) -> Option<ExprRef<'a>> {
    if modified.end() > insertion {
        return None;
    }
    let node = covering_node(module.into(), modified.range());
    let original = node.ancestors().find(|node| {
        node.range() == modified.range() && node.kind() == AnyNodeRef::from(modified).kind()
    })?;
    original.as_expr_ref()
}

fn lexical_item(name: String, ty: Option<Type<'_>>) -> CompletionItem {
    CompletionItem {
        label: name,
        kind: if ty.is_some_and(|ty| {
            crate::hover::is_function_type(ty)
                || matches!(ty, Type::ClassLiteral(_) | Type::GenericAlias(_))
        }) {
            CompletionItemKind::Function
        } else {
            CompletionItemKind::Variable
        },
        mode: None,
        filter_text: None,
        relevance: CompletionRelevance::VariableOrKeyword,
    }
}

fn lexical_names(
    db: &Database,
    file: File,
    model: &SemanticModel<'_>,
    scope: FileScopeId,
) -> Vec<CompletionItem> {
    let mut names = FxHashMap::default();
    let mut add_lexical = |model: &SemanticModel<'_>, scope| {
        for completion in model.lexical_completions(scope) {
            if !completion.is_type_check_only {
                names
                    .entry(completion.name.to_string())
                    .or_insert_with(|| lexical_item(completion.name.to_string(), completion.ty));
            }
        }
    };
    add_lexical(model, scope);
    if let Some(prelude) = db.prelude_for_file(file) {
        add_lexical(
            &SemanticModel::new(db, db.starlark_program_file(prelude)),
            FileScopeId::global(),
        );
    }
    for (name, callable) in db.builtin_completion_names(file) {
        names.entry(name.clone()).or_insert(CompletionItem {
            label: name,
            kind: if callable {
                CompletionItemKind::Function
            } else {
                CompletionItemKind::Module
            },
            mode: None,
            filter_text: None,
            relevance: CompletionRelevance::Builtin,
        });
    }
    names.into_values().collect()
}

fn keyword_parameters(
    model: &SemanticModel<'_>,
    module: &ModModule,
    modified: &ruff_python_ast::ExprCall,
    insertion: ruff_text_size::TextSize,
) -> Vec<String> {
    let Some(callee) = original_expr(module, &modified.func, insertion) else {
        return Vec::new();
    };
    let node = covering_node(module.into(), callee.range());
    let Some(call) = node.ancestors().find_map(|node| match node {
        AnyNodeRef::ExprCall(call) => (call.func.range() == callee.range()).then_some(call),
        _ => None,
    }) else {
        return Vec::new();
    };
    let mut names = HashSet::new();
    for details in call_signature_details(model, call) {
        for parameter in details.parameters {
            if parameter.is_positional_only
                || parameter.is_variadic
                || parameter.is_keyword_variadic
                || modified.arguments.keywords.iter().any(|keyword| {
                    keyword
                        .arg
                        .as_ref()
                        .is_some_and(|name| name.as_str() == parameter.name)
                })
            {
                continue;
            }
            names.insert(parameter.name);
        }
    }
    names.into_iter().collect()
}

/// The marker parse recovers the cursor's syntactic owner, including trailing
/// blank suites. Reconnect that owner to this revision's canonical AST before
/// asking Ty for its scope; enumeration and binding remain in the semantic index.
fn completion_scope(
    db: &Database,
    file: File,
    module: &ModModule,
    marker: &CoveringNode<'_>,
    tokens: &Tokens,
) -> FileScopeId {
    let index = semantic_index(db, db.starlark_program_file(file));
    let marker_range = marker.node().range();
    // An existing partial name already has a canonical expression scope. The
    // recovered owner path is needed only when insertion creates the expression.
    let original_end = marker_range.end() - ruff_text_size::TextSize::of(COMPLETION_MARKER);
    let original_range = ruff_text_size::TextRange::new(marker_range.start(), original_end);
    if !original_range.is_empty() {
        let node = covering_node(module.into(), original_range);
        let expression = node.ancestors().find_map(|node| {
            (node.range() == original_range)
                .then(|| node.as_expr_ref())
                .flatten()
        });
        if let Some(expression) = expression {
            if let Some(scope) = index.try_expression_scope_id(&expression) {
                return scope;
            }
        }
    }
    for owner in marker.ancestors() {
        let body = match owner {
            AnyNodeRef::StmtFunctionDef(function) => suite_range(
                tokens,
                &function.body,
                function.parameters.end(),
                function.end(),
                None,
            )
            .is_some_and(|range| range.contains_range(marker_range)),
            AnyNodeRef::ExprLambda(lambda) => lambda.body.range().contains_range(marker_range),
            AnyNodeRef::ExprListComp(comp) => !comp
                .generators
                .first()
                .is_some_and(|generator| generator.iter.range().contains_range(marker_range)),
            AnyNodeRef::ExprDictComp(comp) => !comp
                .generators
                .first()
                .is_some_and(|generator| generator.iter.range().contains_range(marker_range)),
            _ => false,
        };
        if !body {
            continue;
        }
        let node = covering_node(
            module.into(),
            ruff_text_size::TextRange::empty(owner.start()),
        );
        let Some(original) = node
            .ancestors()
            .find(|original| original.start() == owner.start() && original.kind() == owner.kind())
        else {
            continue;
        };
        let scope_owner = match original {
            AnyNodeRef::StmtFunctionDef(function) => NodeWithScopeRef::Function(function),
            AnyNodeRef::ExprLambda(lambda) => NodeWithScopeRef::Lambda(lambda),
            AnyNodeRef::ExprListComp(comp) => NodeWithScopeRef::ListComprehension(comp),
            AnyNodeRef::ExprDictComp(comp) => NodeWithScopeRef::DictComprehension(comp),
            _ => continue,
        };
        if let Some(scope) = index.try_node_scope(scope_owner) {
            return scope;
        }
    }
    FileScopeId::global()
}

impl<'a> CompletionContext<'a> {
    fn new(
        db: &'a Database,
        FilePosition { file_id: file, pos }: FilePosition,
        trigger_character: Option<String>,
    ) -> Option<Self> {
        let parsed = parsed_module(db, file).load(db);
        let source = file.contents(db);
        let offset = u32::from(pos).into();
        if let Some(context) =
            string_context(db, file, parsed.syntax(), parsed.tokens(), &source, offset)
        {
            return Some(Self {
                analysis: CompletionAnalysis::String(context),
            });
        }
        if matches!(trigger_character.as_deref(), Some("/" | ":" | "@")) {
            return None;
        }
        let mut text = source.to_string();
        let insertion = usize::from(pos);
        if !text.is_char_boundary(insertion) {
            return None;
        }
        text.insert_str(insertion, COMPLETION_MARKER);
        let modified = ruff_python_parser::parse_unchecked_source(
            &text,
            ruff_python_ast::PySourceType::Python,
        );
        let CursorToken::Token(token) = pick_source_token(
            modified.tokens(),
            offset,
            ruff_text_size::TextSize::of(&text),
            |_| 0,
        )?
        else {
            return None;
        };
        let node = covering_node(modified.syntax().into(), token.range());
        if !marker_is_starlark(&node, modified.tokens()) {
            return None;
        }
        let analysis = match node.node() {
            AnyNodeRef::ExprName(name) => {
                let call = match node.parent() {
                    Some(AnyNodeRef::Arguments(args)) => {
                        let direct = args.args.iter().any(|arg| {
                            arg.range() == name.range()
                                && expr_range(arg, args.into(), modified.tokens()) == name.range()
                        });
                        if direct {
                            node.ancestors().find_map(|node| match node {
                                AnyNodeRef::ExprCall(call) => Some(call),
                                _ => None,
                            })
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                let model = SemanticModel::new(db, db.starlark_program_file(file));
                let params = call.map_or_else(Vec::new, |call| {
                    keyword_parameters(&model, parsed.syntax(), call, offset)
                });
                let mut is_in_def = false;
                let mut is_in_for = false;
                let mut is_loop_variable = false;
                for ancestor in node.ancestors() {
                    match ancestor {
                        AnyNodeRef::StmtFunctionDef(_) => is_in_def = true,
                        AnyNodeRef::StmtFor(stmt) => {
                            is_in_for |= !is_in_def;
                            is_loop_variable |= stmt.target.range().contains_range(name.range());
                        }
                        AnyNodeRef::Comprehension(comp) => {
                            is_loop_variable |= comp.target.range().contains_range(name.range());
                        }
                        _ => {}
                    }
                }
                let is_lone_expr = match node.parent() {
                    Some(AnyNodeRef::StmtExpr(stmt)) => {
                        expr_range(&stmt.value, stmt.into(), modified.tokens()) == name.range()
                    }
                    _ => false,
                };
                CompletionAnalysis::NameRef(NameRefContext {
                    names: lexical_names(
                        db,
                        file,
                        &model,
                        completion_scope(db, file, parsed.syntax(), &node, modified.tokens()),
                    ),
                    params,
                    is_in_def,
                    is_in_for,
                    is_lone_expr,
                    is_loop_variable,
                })
            }
            AnyNodeRef::Identifier(_) => {
                let context = match node.parent()? {
                    AnyNodeRef::ExprAttribute(expr) => {
                        let receiver = original_expr(parsed.syntax(), &expr.value, offset)?;
                        let model = SemanticModel::new(db, db.starlark_program_file(file));
                        NameContext::Dot {
                            receiver_ty: receiver.inferred_type(&model)?,
                        }
                    }
                    _ => NameContext::Def,
                };
                CompletionAnalysis::Name(context)
            }
            _ => return None,
        };
        Some(Self { analysis })
    }
}

fn strip_last_package_or_target(label: &str) -> &str {
    if let Some(index) = label.rfind([':', '/']) {
        &label[..index + 1]
    } else {
        label
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;

    use expect_test::expect;
    use expect_test::Expect;
    use starpls_hir::Db;
    use starpls_syntax::TextSize;

    use crate::completions::CompletionRelevance;
    use crate::Analysis;
    use crate::CompletionItemKind;
    use crate::FilePosition;

    #[test]
    fn marker_argument_ownership() {
        for (call, parameters) in [
            ("f($0)", vec!["x=", "y="]),
            ("(f)($0)", vec!["x=", "y="]),
            ("f(0, $0)", vec!["x=", "y="]),
            ("f($0, y=1)", vec!["x="]),
            ("f(($0))", vec![]),
            ("f(x=$0)", vec![]),
            ("f(*$0)", vec![]),
            ("f(**$0)", vec![]),
        ] {
            let source = format!("def f(x, y): pass\n{call}");
            let (analysis, fixture) = Analysis::from_single_file_fixture(&source);
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let items = analysis
                .snapshot()
                .completions(FilePosition { file_id, pos }, None)
                .unwrap()
                .unwrap();
            let mut actual = items
                .iter()
                .filter(|item| item.relevance == CompletionRelevance::Parameter)
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>();
            actual.sort_unstable();
            assert_eq!(actual, parameters, "{call}");
            assert!(items.iter().any(|item| item.label == "f"), "{call}");
        }
    }

    #[test]
    fn lexical_scope_reconnects_recovered_owners() {
        let cases: &[(&str, &[&str], &[&str])] = &[
            (
                "a = 0\nb, c = 1, 2\n[d, e] = [3, 4]\n$0",
                &["a", "b", "c", "d", "e"],
                &[],
            ),
            (
                "def f(x, *args, **kwargs):\n    $0",
                &["f", "x", "args", "kwargs"],
                &[],
            ),
            ("for x, y in [(1, 2)]:\n    $0", &["x", "y"], &[]),
            (
                "outer = [1]\nvalues = [item for item in outer if it$0em]",
                &["item", "outer"],
                &[],
            ),
            (
                "def outer(param):\n    local = 1\n    $0\n\ndef later(): pass\n",
                &["param", "local", "outer", "later"],
                &[],
            ),
            (
                "def outer(param):\n    local = 1\n    # comment\n\n    $0",
                &["param", "local"],
                &[],
            ),
            (
                "def outer(param):\n    local = 1\n\n# dedented comment\n$0",
                &["outer"],
                &["param", "local"],
            ),
            (
                "def outer(param):\n    def inner(nested):\n        inside = 1\n        $0",
                &["param", "nested", "inside", "inner"],
                &[],
            ),
            ("def empty(param):\n    $0", &["param"], &[]),
            (
                "global_value = 1\ndef f(param=$0):\n    local = 1\n",
                &["global_value"],
                &["param", "local"],
            ),
            (
                "global_value = 1\nf = lambda param=$0: param",
                &["global_value"],
                &["param"],
            ),
            ("f = lambda param: $0", &["param"], &[]),
            (
                "outer = [1]\nvalues = [item for item in $0]",
                &["outer"],
                &["item"],
            ),
            (
                "outer = [[1]]\nvalues = [later for item in outer for later in $0]",
                &["outer", "item", "later"],
                &[],
            ),
            (
                "outer = [1]\nvalues = {$0: item for item in outer}",
                &["outer", "item"],
                &[],
            ),
        ];
        let (mut analysis, fixture) = Analysis::from_single_file_fixture("");
        let file_id = fixture.main_file();
        for (source, present, absent) in cases {
            let pos = TextSize::try_from(source.find("$0").unwrap()).unwrap();
            analysis.update_file(file_id, source.replace("$0", ""));
            let items = analysis
                .snapshot()
                .completions(FilePosition { file_id, pos }, None)
                .unwrap()
                .unwrap();
            if source.contains("lambda param=$0") {
                assert!(starpls_hir::diagnostics_for_file(&analysis.db, file_id)
                    .any(|diagnostic| diagnostic.is_invalid_syntax()));
            }
            if source.starts_with("for x, y") {
                assert!(starpls_hir::diagnostics_for_file(&analysis.db, file_id)
                    .any(|diagnostic| diagnostic.headline_message()
                        == "Starlark does not allow top-level for statements"));
            }
            for name in *present {
                assert!(
                    items.iter().any(|item| item.label == *name),
                    "missing {name} in {source}: {items:?}"
                );
            }
            for name in *absent {
                assert!(
                    !items.iter().any(|item| item.label == *name),
                    "unexpected {name} in {source}: {items:?}"
                );
            }
        }
    }

    #[test]
    fn prelude_completions_preserve_local_shadowing() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        fixture.add_prelude_file(&mut analysis.db, "value = 1\ndef action(): pass\n");
        fixture.add_file_with_options(
            &mut analysis.db,
            "BUILD",
            "action = 1\nvalue$0",
            starpls_common::Dialect::Bazel,
            Some(starpls_common::FileInfo::Bazel {
                api_context: starpls_bazel::APIContext::Build,
                is_external: false,
            }),
        );
        loader.add_files_from_fixture(&fixture);
        let (file_id, pos) = fixture.cursor_pos.unwrap();
        for is_external in [false, true, false] {
            let file_id = starpls_common::File {
                info: Some(starpls_common::FileInfo::Bazel {
                    api_context: starpls_bazel::APIContext::Build,
                    is_external,
                }),
                ..file_id
            };
            let items = analysis
                .snapshot()
                .completions(FilePosition { file_id, pos }, None)
                .unwrap()
                .unwrap();
            let action = items
                .iter()
                .filter(|item| item.label == "action")
                .collect::<Vec<_>>();
            assert_eq!(action.len(), 1, "{items:?}");
            assert_eq!(action[0].kind, CompletionItemKind::Variable);
            assert_eq!(
                items.iter().any(|item| item.label == "value"),
                !is_external,
                "{items:?}"
            );
            let model = ty_python_semantic::SemanticModel::new(
                &analysis.db,
                analysis.db.starlark_program_file(file_id),
            );
            let parsed = starpls_common::parsed_module(&analysis.db, file_id).load(&analysis.db);
            let ruff_python_ast::Stmt::Expr(statement) = parsed.syntax().body.last().unwrap()
            else {
                panic!("expected value reference")
            };
            use ty_python_semantic::HasType;
            let ty = statement.value.inferred_type(&model).unwrap();
            assert_eq!(
                ty.display(&analysis.db, &model.program_environment())
                    .to_string(),
                if is_external { "Unknown" } else { "Literal[1]" }
            );
        }
    }

    #[test]
    fn loaded_keyword_and_name_completions_follow_edits() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        let definitions =
            fixture.add_file(&mut analysis.db, "defs.bzl", "def action(first): pass\n");
        fixture.add_file(
            &mut analysis.db,
            "main.bzl",
            "load(\"defs.bzl\", loaded = \"action\")\nlen = 1\nloaded($0)",
        );
        loader.add_files_from_fixture(&fixture);
        let (file_id, pos) = fixture.cursor_pos.unwrap();
        for parameter in ["first", "second", "first"] {
            analysis.update_file(definitions, format!("def action({parameter}): pass\n"));
            let items = analysis
                .snapshot()
                .completions(FilePosition { file_id, pos }, None)
                .unwrap()
                .unwrap();
            assert_eq!(
                items
                    .iter()
                    .filter(|item| item.relevance == CompletionRelevance::Parameter)
                    .map(|item| item.label.as_str())
                    .collect::<Vec<_>>(),
                [format!("{parameter}=")]
            );
            for (name, kind) in [
                ("loaded", CompletionItemKind::Function),
                ("len", CompletionItemKind::Variable),
            ] {
                let matches = items
                    .iter()
                    .filter(|item| item.label == name)
                    .collect::<Vec<_>>();
                assert_eq!(matches.len(), 1, "{name}: {items:?}");
                assert_eq!(matches[0].kind, kind);
                assert_eq!(matches[0].relevance, CompletionRelevance::VariableOrKeyword);
            }
            assert!(!items.iter().any(|item| matches!(
                item.label.as_str(),
                "open" | "__file__" | "__build_class__"
            )));
        }
    }

    #[test]
    fn original_receiver_follows_edits() {
        let (mut analysis, fixture) = Analysis::from_single_file_fixture("");
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                starpls_bazel::Builtins::default(),
            )
            .unwrap();
        let file_id = fixture.main_file();
        for (prefix, field) in [("", "first"), ("other = [1, 2]\n", "second"), ("", "first")] {
            let source = format!("{prefix}obj = struct({field}=1)\n(obj).");
            let pos = TextSize::try_from(source.len()).unwrap();
            analysis.update_file(file_id, source);
            let items = analysis
                .snapshot()
                .completions(FilePosition { file_id, pos }, None)
                .unwrap()
                .unwrap();
            assert_eq!(
                items
                    .iter()
                    .map(|item| item.label.as_str())
                    .collect::<Vec<_>>(),
                [field]
            );
        }
    }

    #[test]
    fn dictionary_keys_do_not_require_valid_strings() {
        let (analysis, fixture) = Analysis::from_single_file_fixture(
            r#"d = {"known": 1}
d["\x$0"]"#,
        );
        let (file_id, pos) = fixture.cursor_pos.unwrap();
        let items = analysis
            .snapshot()
            .completions(FilePosition { file_id, pos }, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            ["known"]
        );
    }

    #[test]
    fn marker_excludes_ignored_source() {
        for source in [
            "class C:\n    $0",
            "class C:\n    load(\"m\", al$0ias=\"x\")",
            "load(\"m\", f(al$0ias=\"x\"))",
            "d=1\nd[$0\"foo\"]",
            "def f(x: $0): pass",
            "def f() -> $0: pass",
            "1 ** $0",
            "{**$0}",
            "def f()\n    $0",
            "for x in []:\n    pass\nelse:\n    $0",
            "# comment $0",
            "# type: $0",
            "load($0)",
        ] {
            let (analysis, fixture) = Analysis::from_single_file_fixture(source);
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let result = analysis
                .snapshot()
                .completions(FilePosition { file_id, pos }, None)
                .unwrap();
            assert!(result.is_none(), "{source}: {result:?}");
        }
    }

    #[test]
    fn marker_preserves_names_and_keyword_contexts() {
        for (source, names, statements, flow) in [
            ("$0", true, true, vec![]),
            ("($0)", true, false, vec![]),
            ("xs[$0:]", true, false, vec![]),
            ("for $0 in []: pass", false, false, vec![]),
            ("[x for $0 in []]", false, false, vec![]),
            ("def f($0): pass", false, false, vec![]),
            (
                "def f():\n    for x in []:\n        $0",
                true,
                true,
                vec!["break", "continue", "return"],
            ),
            (
                "for x in []:\n    def f():\n        $0",
                true,
                true,
                vec!["return"],
            ),
        ] {
            let source = format!("known = 1\n{source}");
            let (analysis, fixture) = Analysis::from_single_file_fixture(&source);
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let items = analysis
                .snapshot()
                .completions(FilePosition { file_id, pos }, None)
                .unwrap()
                .unwrap();
            let labels = items
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>();
            assert_eq!(labels.contains(&"known"), names, "{source}");
            assert_eq!(labels.contains(&"def"), statements, "{source}");
            for keyword in ["break", "continue", "return"] {
                assert_eq!(
                    labels.contains(&keyword),
                    flow.contains(&keyword),
                    "{source}: {keyword}"
                );
            }
        }
    }

    #[test]
    fn load_strings_complete_public_definitions() {
        for (input, expected) in [
            (
                "load(\"defs.bzl\", \"$0\")",
                Some(vec!["alias", "f", "rebound", "value"]),
            ),
            (
                "load(\"defs.bzl\", alias = \"$0\")",
                Some(vec!["alias", "f", "rebound", "value"]),
            ),
            (
                r#"load("defs.bzl", "\x$0")"#,
                Some(vec!["alias", "f", "rebound", "value"]),
            ),
            (
                r#"load("defs.bzl", f("$0"))"#,
                Some(vec!["alias", "f", "rebound", "value"]),
            ),
            ("load(\"defs.bzl\", al$0ias = \"value\")", Some(vec![])),
            ("load(\"defs.bzl\", \"value\"$0,)", None),
        ] {
            let (mut analysis, loader) = Analysis::new_for_test();
            let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
            fixture.add_file(&mut analysis.db, "other.bzl", "imported = 1\nrebound = 1\n");
            fixture.add_file(
                &mut analysis.db,
                "defs.bzl",
                "load(\"other.bzl\", \"imported\", \"rebound\")\nalias = imported\nrebound = 2\nvalue = 1\n_private = 1\ndef f(): pass\n",
            );
            fixture.add_file(&mut analysis.db, "main.bzl", input);
            loader.add_files_from_fixture(&fixture);
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let items = analysis
                .snapshot()
                .completions(FilePosition { file_id, pos }, None)
                .unwrap();
            let labels = items.as_ref().map(|items| {
                let mut labels = items
                    .iter()
                    .map(|item| item.label.as_str())
                    .collect::<Vec<_>>();
                labels.sort_unstable();
                labels
            });
            assert_eq!(labels, expected, "{input}");
        }
    }

    fn check_completions(fixture: &str, expect: Expect) {
        check_completions_with_options(fixture, false, expect);
    }

    fn check_completions_with_options(
        fixture: &str,
        include_builtins_and_keywords: bool,
        expect: Expect,
    ) {
        let (mut analysis, fixture) = Analysis::from_single_file_fixture(fixture);
        analysis.db.set_all_workspace_targets(
            [
                "//:foo",
                "//:bar",
                "//bar:bar",
                "//foo:foo",
                "//foo/bar:bar",
                "//foo/bar:baz",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
        );

        let completions = analysis
            .snapshot()
            .completions(
                fixture
                    .cursor_pos
                    .map(|(file_id, pos)| FilePosition { file_id, pos })
                    .unwrap(),
                Some("".to_string()),
            )
            .unwrap()
            .unwrap();

        let mut completions = completions
            .into_iter()
            .filter(|item| {
                include_builtins_and_keywords
                    || (item.relevance != CompletionRelevance::Builtin
                        && item.kind != CompletionItemKind::Keyword)
            })
            .collect::<Vec<_>>();
        completions.sort_by(|item1, item2| item1.label.cmp(&item2.label));

        let expected = completions
            .into_iter()
            .fold(String::new(), |mut acc, item| {
                writeln!(acc, "{:?}", item).unwrap();
                acc
            });

        expect.assert_eq(&expected);
    }

    #[test]
    fn test_empty() {
        check_completions_with_options(
            r#"
$0
"#,
            true,
            expect![[r#"
                CompletionItem { label: "False", kind: Keyword, mode: None, filter_text: None, relevance: VariableOrKeyword }
                CompletionItem { label: "None", kind: Keyword, mode: None, filter_text: None, relevance: VariableOrKeyword }
                CompletionItem { label: "True", kind: Keyword, mode: None, filter_text: None, relevance: VariableOrKeyword }
                CompletionItem { label: "abs", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "all", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "any", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "attr", kind: Module, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "bool", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "bytes", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "def", kind: Keyword, mode: None, filter_text: None, relevance: VariableOrKeyword }
                CompletionItem { label: "dict", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "dir", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "enumerate", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "fail", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "float", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "for", kind: Keyword, mode: None, filter_text: None, relevance: VariableOrKeyword }
                CompletionItem { label: "getattr", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "hasattr", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "hash", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "if", kind: Keyword, mode: None, filter_text: None, relevance: VariableOrKeyword }
                CompletionItem { label: "int", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "lambda", kind: Keyword, mode: None, filter_text: None, relevance: VariableOrKeyword }
                CompletionItem { label: "len", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "licenses", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "list", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "load", kind: Keyword, mode: None, filter_text: None, relevance: VariableOrKeyword }
                CompletionItem { label: "max", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "min", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "module_extension", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "package", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "pass", kind: Keyword, mode: None, filter_text: None, relevance: VariableOrKeyword }
                CompletionItem { label: "print", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "provider", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "range", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "repository_rule", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "repr", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "reversed", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "rule", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "sorted", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "str", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "struct", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "tag_class", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "tuple", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "type", kind: Function, mode: None, filter_text: None, relevance: Builtin }
                CompletionItem { label: "zip", kind: Function, mode: None, filter_text: None, relevance: Builtin }
            "#]],
        );
    }

    #[test]
    fn test_parameters() {
        check_completions(
            r#"
abc = 1
def foo(x, y):
    pass
    x + $0
"#,
            expect![[r#"
                CompletionItem { label: "abc", kind: Variable, mode: None, filter_text: None, relevance: VariableOrKeyword }
                CompletionItem { label: "foo", kind: Function, mode: None, filter_text: None, relevance: VariableOrKeyword }
                CompletionItem { label: "x", kind: Variable, mode: None, filter_text: None, relevance: VariableOrKeyword }
                CompletionItem { label: "y", kind: Variable, mode: None, filter_text: None, relevance: VariableOrKeyword }
            "#]],
        );
    }

    #[test]
    fn test_arguments() {
        check_completions(
            r#"
def foo(x, y):
    pass

foo(
    $0
)
"#,
            expect![[r#"
                CompletionItem { label: "foo", kind: Function, mode: None, filter_text: None, relevance: VariableOrKeyword }
                CompletionItem { label: "x=", kind: Variable, mode: Some(InsertText("x = ")), filter_text: None, relevance: Parameter }
                CompletionItem { label: "y=", kind: Variable, mode: Some(InsertText("y = ")), filter_text: None, relevance: Parameter }
            "#]],
        );
    }

    #[test]
    fn test_variables() {
        check_completions(
            r#"
x = 1
y = 2
$0
"#,
            expect![[r#"
                CompletionItem { label: "x", kind: Variable, mode: None, filter_text: None, relevance: VariableOrKeyword }
                CompletionItem { label: "y", kind: Variable, mode: None, filter_text: None, relevance: VariableOrKeyword }
            "#]],
        );
    }

    #[test]
    fn test_fields() {
        check_completions(
            r#"
foo = struct(x = 1, y = 2)
foo.$0
"#,
            expect![[r#"
                CompletionItem { label: "x", kind: Field, mode: None, filter_text: None, relevance: VariableOrKeyword }
                CompletionItem { label: "y", kind: Field, mode: None, filter_text: None, relevance: VariableOrKeyword }
            "#]],
        );
    }

    #[test]
    fn test_dict_keys() {
        check_completions(
            r#"
d = {"a": 1, "b": 2}
d["$0"]
"#,
            expect![[r#"
                CompletionItem { label: "a", kind: Constant, mode: None, filter_text: None, relevance: VariableOrKeyword }
                CompletionItem { label: "b", kind: Constant, mode: None, filter_text: None, relevance: VariableOrKeyword }
            "#]],
        );
    }

    #[test]
    fn dictionary_keys_follow_loaded_source_bindings() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        fixture.add_file(
            &mut analysis.db,
            "defs.bzl",
            "d = {'stale': 1}\nd = {'loaded': 2}\nexported = d\n",
        );
        fixture.add_file(
            &mut analysis.db,
            "main.bzl",
            "d = {'caller': 1}\nload('defs.bzl', imported='exported')\nalias = imported\nimported = {'replacement': 2}\nalias['$0']",
        );
        loader.add_files_from_fixture(&fixture);
        let (file_id, pos) = fixture.cursor_pos.unwrap();
        let items = analysis
            .snapshot()
            .completions(FilePosition { file_id, pos }, None)
            .unwrap()
            .unwrap();
        let actual: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(actual, ["loaded"]);
    }

    #[test]
    fn test_label_completions_1() {
        check_completions(
            r#"
label = "//$0"
"#,
            expect![[r#"
                CompletionItem { label: "bar", kind: Folder, mode: None, filter_text: None, relevance: VariableOrKeyword }
                CompletionItem { label: "foo", kind: Folder, mode: None, filter_text: None, relevance: VariableOrKeyword }
            "#]],
        );
    }

    #[test]
    fn test_label_completions_2() {
        check_completions(
            r#"
label = "//fo$0"
"#,
            expect![[r#"
                CompletionItem { label: "bar", kind: Folder, mode: None, filter_text: None, relevance: VariableOrKeyword }
                CompletionItem { label: "foo", kind: Folder, mode: None, filter_text: None, relevance: VariableOrKeyword }
            "#]],
        );
    }

    #[test]
    fn test_label_completions_3() {
        check_completions(
            r#"
label = "//:$0"
"#,
            expect![[r#"
                CompletionItem { label: "bar", kind: Field, mode: None, filter_text: None, relevance: VariableOrKeyword }
                CompletionItem { label: "foo", kind: Field, mode: None, filter_text: None, relevance: VariableOrKeyword }
            "#]],
        );
    }

    #[test]
    fn test_label_completions_4() {
        check_completions(
            r#"
label = "//:f$0"
"#,
            expect![[r#"
                CompletionItem { label: "bar", kind: Field, mode: None, filter_text: None, relevance: VariableOrKeyword }
                CompletionItem { label: "foo", kind: Field, mode: None, filter_text: None, relevance: VariableOrKeyword }
            "#]],
        );
    }

    #[test]
    fn test_label_completions_5() {
        check_completions(
            r#"
label = "//foo:$0"
"#,
            expect![[r#"
                CompletionItem { label: "foo", kind: Field, mode: None, filter_text: None, relevance: VariableOrKeyword }
            "#]],
        );
    }

    #[test]
    fn test_label_completions_6() {
        check_completions(
            r#"
label = "//foo/$0"
"#,
            expect![[r#"
                CompletionItem { label: "bar", kind: Folder, mode: None, filter_text: None, relevance: VariableOrKeyword }
            "#]],
        );
    }

    #[test]
    fn test_label_completions_7() {
        check_completions(
            r#"
label = "//foo/bar:b$0"
"#,
            expect![[r#"
                CompletionItem { label: "bar", kind: Field, mode: None, filter_text: None, relevance: VariableOrKeyword }
                CompletionItem { label: "baz", kind: Field, mode: None, filter_text: None, relevance: VariableOrKeyword }
            "#]],
        );
    }
}
