//! Partially replicates the "completions" API in the LSP specification.

use std::collections::HashSet;

use ruff_db::source::source_text;
use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::token::Tokens;
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::Expr;
use ruff_python_ast::ModModule;
use ruff_text_size::Ranged;
use rustc_hash::FxHashMap;
use starpls_common::parsed_module;
use starpls_common::Db as _;
use starpls_common::File;
use starpls_common::LoadItemCandidateKind;
use starpls_hir::Db;
use starpls_hir::Source;
use starpls_syntax::source::expr_range;
use starpls_syntax::source::string_value;
use starpls_syntax::source::suite_range;
use starpls_syntax::TextRange;
use starpls_syntax::TextSize;
use ty_ide::CompletionCursor;
use ty_ide::CompletionTarget;
use ty_python_core::definition::DefinitionKind;
use ty_python_core::global_scope;
use ty_python_core::scope::FileScopeId;
use ty_python_core::semantic_index;
use ty_python_semantic::provided::BuiltinUsage;
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
    is_annotation: bool,
}

enum StringContext {
    // A recognized string context must not fall through to name completion
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
    let file_id = pos.file_id;
    let model = SemanticModel::new(db, db.starlark_program_file(file_id));
    let ctx = CompletionContext::new(db, pos, trigger_character.clone())?;
    let mut items = Vec::new();

    match ctx.analysis {
        CompletionAnalysis::NameRef(NameRefContext {
            names,
            params,
            is_lone_expr,
            is_in_def,
            is_in_for,
            is_annotation,
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

            add_globals(&mut items, is_annotation);
            items.extend(names);

            if is_lone_expr {
                add_keywords(&mut items, is_in_def, is_in_for);
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
                        crate::hover::is_function_type(crate::hover::callable_display_type(
                            &model, ty,
                        )) || matches!(ty, Type::ClassLiteral(_) | Type::GenericAlias(_))
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
            let mut names = FxHashMap::default();
            let files = db
                .type_interface(file_id, loaded_file)
                .into_iter()
                .chain([loaded_file]);
            for declaration in files.flat_map(|file| {
                all_end_of_scope_members(db, global_scope(db, db.starlark_program_file(file)))
            }) {
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
                names.entry(member.name.to_string()).or_insert_with(|| {
                    lexical_item(&model, member.name.to_string(), Some(member.ty))
                });
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

fn add_globals(items: &mut Vec<CompletionItem>, is_annotation: bool) {
    let add_global = &mut |global: &'static str| {
        items.push(CompletionItem {
            label: global.to_string(),
            kind: CompletionItemKind::Keyword,
            mode: None,
            relevance: CompletionRelevance::VariableOrKeyword,
            filter_text: None,
        })
    };
    if !is_annotation {
        add_global("True");
        add_global("False");
    }
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
    let sema = Source::new(db);
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
            let model = SemanticModel::new(db, db.starlark_program_file(file));
            model.scope(expr.into())?;
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

fn lexical_item<'db>(
    model: &SemanticModel<'db>,
    name: String,
    ty: Option<Type<'db>>,
) -> CompletionItem {
    CompletionItem {
        label: name,
        kind: if ty.is_some_and(|ty| {
            crate::hover::is_function_type(crate::hover::callable_display_type(model, ty))
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
    usage: BuiltinUsage,
) -> Vec<CompletionItem> {
    let mut names = FxHashMap::default();
    let mut add_lexical = |model: &SemanticModel<'_>, scope| {
        for completion in model.lexical_completions(scope) {
            if !completion.is_type_check_only || matches!(usage, BuiltinUsage::Annotation) {
                names.entry(completion.name.to_string()).or_insert_with(|| {
                    lexical_item(model, completion.name.to_string(), completion.ty)
                });
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
    for (name, callable) in db.builtin_completion_names(file, usage) {
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

fn keyword_parameters(model: &SemanticModel<'_>, call: &ruff_python_ast::ExprCall) -> Vec<String> {
    let mut names = HashSet::new();
    for details in call_signature_details(model, call) {
        for parameter in details.parameters {
            if parameter.is_positional_only
                || parameter.is_variadic
                || parameter.is_keyword_variadic
                || call.arguments.keywords.iter().any(|keyword| {
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

impl<'a> CompletionContext<'a> {
    fn new(
        db: &'a Database,
        FilePosition { file_id: file, pos }: FilePosition,
        trigger_character: Option<String>,
    ) -> Option<Self> {
        let parsed = parsed_module(db, file).load(db);
        let program_file = db.starlark_program_file(file);
        let source = source_text(db, program_file.file(db));
        let offset = u32::from(pos).into();
        let index = semantic_index(db, program_file);
        if index.is_excluded(ruff_text_size::TextRange::empty(offset)) {
            return None;
        }
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
        let cursor = CompletionCursor::new(&parsed, &source, offset)?;
        if cursor.is_in_string()
            || cursor
                .ancestors()
                .any(|node| index.is_excluded(node.range()))
        {
            return None;
        }
        let target = cursor.target()?;
        for node in cursor.ancestors() {
            // Recovered owners can contain suites that the host grammar omits.
            let suite = match node {
                AnyNodeRef::StmtFunctionDef(function) => (!file.allows_native_annotations(db)
                    && offset > function.parameters.end())
                .then_some((function.body.as_slice(), function.parameters.end())),
                AnyNodeRef::StmtFor(statement) => (offset > statement.iter.end())
                    .then_some((statement.body.as_slice(), statement.iter.end())),
                _ => None,
            };
            if let Some((body, after)) = suite {
                if !suite_range(parsed.tokens(), body, after, node.end().max(offset), None)
                    .is_some_and(|range| range.contains_inclusive(offset))
                {
                    return None;
                }
            }
            // Load arguments declare opaque bindings. Only an alias name offers
            // the empty definition-name completion response.
            let AnyNodeRef::StmtExpr(statement) = node else {
                continue;
            };
            let Expr::Call(call) = statement.value.as_ref() else {
                continue;
            };
            if !matches!(call.func.as_ref(), Expr::Name(name) if name.id == "load") {
                continue;
            }
            let alias = call.arguments.keywords.iter().any(|keyword| {
                keyword
                    .arg
                    .as_ref()
                    .is_some_and(|name| name.range().contains_inclusive(offset))
            });
            return alias.then_some(Self {
                analysis: CompletionAnalysis::Name(NameContext::Def),
            });
        }
        let model = SemanticModel::new(db, program_file);
        let analysis = match target {
            CompletionTarget::Attribute(attribute) => CompletionAnalysis::Name(NameContext::Dot {
                receiver_ty: attribute.value.inferred_type(&model)?,
            }),
            CompletionTarget::Scoped(_) => {
                if cursor.is_in_definition_place() {
                    CompletionAnalysis::Name(NameContext::Def)
                } else {
                    let mut is_in_def = false;
                    let mut is_in_for = false;
                    for ancestor in cursor.ancestors() {
                        match ancestor {
                            AnyNodeRef::StmtFunctionDef(_) => is_in_def = true,
                            AnyNodeRef::StmtFor(_) => is_in_for |= !is_in_def,
                            _ => {}
                        }
                    }
                    let is_annotation = cursor.is_in_annotation();
                    let usage = if is_annotation {
                        BuiltinUsage::Annotation
                    } else {
                        BuiltinUsage::Runtime
                    };
                    CompletionAnalysis::NameRef(NameRefContext {
                        names: lexical_names(db, file, &model, cursor.scope(&model)?, usage),
                        params: cursor
                            .keyword_call()
                            .map_or_else(Vec::new, |call| keyword_parameters(&model, call)),
                        is_in_def,
                        is_in_for,
                        is_lone_expr: !is_annotation && cursor.is_statement_start(),
                        is_annotation,
                    })
                }
            }
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
    fn argument_completion_ownership() {
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
    fn native_annotation_completions_use_the_type_namespace() {
        let mut failures = Vec::new();
        let types = &["Info", "Label", "list", "str", "string", "Sequence", "None"][..];
        for (expression, expected) in [
            ("def f(value: $0): pass", types),
            ("def f() -> $0: pass", types),
            ("def f(value: st$0r): pass", types),
            ("def f(value: list[$0]): pass", types),
            ("def f(value: list[int | $0]): pass", types),
            ("def f(value: $0", types),
            ("def f(value: api.$0): pass", &["Info"][..]),
            ("assignment: $0 = None", types),
        ] {
            let source =
                format!("Info = provider(fields=[])\napi = struct(Info=Info)\n{expression}");
            let (mut analysis, fixture) = Analysis::from_single_file_fixture(&source);
            analysis
                .set_builtin_defs(
                    starpls_bazel::decode_builtins(include_bytes!(
                        "../../starpls/src/builtin/builtin.pb"
                    ))
                    .unwrap(),
                    starpls_bazel::Builtins::default(),
                )
                .unwrap();
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let items = analysis
                .snapshot()
                .completions(FilePosition { file_id, pos }, None)
                .unwrap()
                .unwrap_or_default();
            let names = items
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>();
            for &expected in expected {
                if !names.contains(&expected) {
                    failures.push(format!("{expression}: missing {expected}: {names:?}"));
                }
            }
            for unexpected in ["fail", "len", "print", "value", "return", "True"] {
                if names.contains(&unexpected) {
                    failures.push(format!("{expression}: unexpected {unexpected}: {names:?}"));
                }
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn excluded_statements_do_not_offer_completions() {
        let (mut analysis, fixture) = Analysis::from_single_file_fixture("");
        let file_id = fixture.main_file();
        for source in [
            "@decorator\ndef f(param):\n    par$0",
            "obj.value: int = par$0",
        ] {
            let pos = TextSize::try_from(source.find("$0").unwrap()).unwrap();
            analysis.update_file(file_id, source.replace("$0", ""));
            assert!(
                analysis
                    .snapshot()
                    .completions(FilePosition { file_id, pos }, None)
                    .unwrap()
                    .is_none(),
                "{source}"
            );
        }
        for source in [
            "def f(param):\n    par$0",
            "def f(param: int):\n    par$0",
            "def f(param: int):\n    pass\n    $0",
            "@decorator\ndef f(param):\n    pass\n$0",
            "def f(param):\n    par$0",
        ] {
            let pos = TextSize::try_from(source.find("$0").unwrap()).unwrap();
            analysis.update_file(file_id, source.replace("$0", ""));
            assert!(
                analysis
                    .snapshot()
                    .completions(FilePosition { file_id, pos }, None)
                    .unwrap()
                    .is_some(),
                "{source}"
            );
        }
    }

    #[test]
    fn lexical_scope_uses_canonical_cursor() {
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
                "global_value = 1\ndef f(param)\n    $0",
                &["global_value", "f"],
                &["param"],
            ),
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
            if source.contains("lambda param=$0") || source.contains("def f(param)\n") {
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
    fn canonical_receiver_follows_edits() {
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
        for (prefix, field, member) in [
            ("", "first", "$0"),
            ("other = [1, 2]\n", "second", "se$0cond"),
            ("", "first", "fi$0rst"),
            ("", "first", "$0"),
        ] {
            let source = format!("{prefix}obj = struct({field}=1)\n(obj).{member}");
            let pos = TextSize::try_from(source.find("$0").unwrap()).unwrap();
            analysis.update_file(file_id, source.replace("$0", ""));
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
    fn cursor_excludes_ignored_source() {
        let mut unexpected = Vec::new();
        for source in [
            "class C:\n    $0",
            "class C:\n    load(\"m\", al$0ias=\"x\")",
            "load(\"m\", f(al$0ias=\"x\"))",
            "d=1\nd[$0\"foo\"]",
            "1 ** $0",
            "{**$0}",
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
                .unwrap()
                .unwrap_or_default();
            if !result.is_empty() {
                unexpected.push((
                    source,
                    result
                        .into_iter()
                        .map(|item| item.label)
                        .collect::<Vec<_>>(),
                ));
            }
        }
        assert!(unexpected.is_empty(), "{unexpected:#?}");
    }

    #[test]
    fn cursor_preserves_names_and_keyword_contexts() {
        for (source, names, statements, flow) in [
            ("$0", true, true, vec![]),
            ("($0)", true, false, vec![]),
            ("xs[$0:]", true, false, vec![]),
            ("for $0 in []: pass", false, false, vec![]),
            ("for first, se$0cond in []: pass", false, false, vec![]),
            ("[x for $0 in []]", false, false, vec![]),
            ("[x for first, se$0cond in []]", false, false, vec![]),
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
