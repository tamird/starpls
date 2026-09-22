use std::fmt::Write;

use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::ArgOrKeyword;
use ruff_python_ast::Expr;
use ruff_python_ast::Stmt;
use ruff_text_size::Ranged;
use starpls_common::parsed_module;
use starpls_common::syntax_info;
use starpls_common::File;
use starpls_hir::Source;
use starpls_syntax::ast::AstNode;
use starpls_syntax::ast::{self};
use starpls_syntax::source::expr_range;
use starpls_syntax::source::string_value;
use starpls_syntax::TextRange;
use starpls_syntax::T;
use ty_ide::Docstring;
use ty_ide::DocstringFragment;
use ty_ide::MarkupKind;
use ty_python_semantic::types::ide_support::definitions_for_attribute;
use ty_python_semantic::types::ide_support::resolved_call_signature;
use ty_python_semantic::types::CallableTypeKind;
use ty_python_semantic::types::Type;
use ty_python_semantic::HasDefinition;
use ty_python_semantic::HasType;
use ty_python_semantic::SemanticModel;

use crate::selection::Selection;
use crate::util::navigation_token;
use crate::util::pick_best_token;
use crate::util::CursorToken;
use crate::Database;
use crate::FilePosition;

mod docs;

pub struct Markup {
    pub value: String,
}
pub struct Hover {
    pub contents: Markup,
    pub range: Option<TextRange>,
}
impl From<String> for Hover {
    fn from(value: String) -> Self {
        Self {
            contents: Markup { value },
            range: None,
        }
    }
}

pub(crate) fn hover(
    db: &Database,
    FilePosition { file_id: file, pos }: FilePosition,
) -> Option<Hover> {
    let sema = Source::new(db);
    let source = file.contents(db);
    let parsed = parsed_module(db, file).load(db);
    let model = SemanticModel::new(db, db.starlark_program_file(file));
    let offset = u32::from(pos).into();
    let token = navigation_token(&source, parsed.tokens(), offset)?;
    let comments = syntax_info(db, file);
    if let Some(comment) =
        crate::selection::type_comment_at_cursor(comments, offset, token, &source)
    {
        return type_comment_hover(db, &model, &sema, file, comment, offset);
    }
    if let CursorToken::Token(token) = token {
        if token.kind().is_non_soft_keyword()
            || (token.kind() == TokenKind::Identifier && &source[token.range()] == "load")
        {
            return keyword_hover(&source[token.range()]);
        }
    }
    let node = covering_node(parsed.syntax().into(), token.range());
    match crate::selection::classify(&node, token.range())? {
        Selection::Reference(expr) => {
            Some(format_for_name(&model, expr.id.as_str(), expr.inferred_type(&model)?).into())
        }
        Selection::Attribute(expr) => {
            let field_ty = expr.inferred_type(&model)?;
            let display_ty = callable_display_type(&model, field_ty);
            let mut text = String::from("```python\n");
            if is_function_type(display_ty) {
                text.push_str("(method) ");
            } else {
                write!(text, "(field) {}: ", expr.attr).ok()?;
            }
            writeln!(
                text,
                "{}\n```",
                display_ty.display(db, &model.program_environment())
            )
            .ok()?;
            let receiver = expr.value.inferred_type(&model)?;
            let documentation = receiver
                .provided_data(db, &model.program_environment())
                .and_then(|data| data.downcast_ref::<crate::ty::Documentation>())
                .and_then(|docs| {
                    docs.parameters.iter().find_map(|(name, text)| {
                        (name.as_str() == expr.attr.as_str()).then(|| text.to_string())
                    })
                })
                .or_else(|| {
                    definitions_for_attribute(&model, expr)
                        .into_iter()
                        .filter_map(|definition| definition.definition())
                        .find_map(|definition| definition.docstring(db).map(|doc| doc.to_string()))
                });
            if let Some(doc) = documentation {
                text.push_str(&Docstring::new(doc).render(MarkupKind::Markdown));
                text.push('\n');
            }
            Some(text.into())
        }
        Selection::Definition(def) => {
            Some(format_for_name(&model, def.name.as_str(), def.inferred_type(&model)?).into())
        }
        Selection::Parameter(param) => {
            model.scope(param.into())?;
            let documentation = node
                .ancestors()
                .find_map(|node| {
                    let AnyNodeRef::StmtFunctionDef(function) = node else {
                        return None;
                    };
                    function.definition(&model).docstring(db)
                })
                .and_then(|doc| {
                    Docstring::new(doc.to_string())
                        .parameter_documentation()
                        .swap_remove(param.name.as_str())
                })
                .map(|doc| DocstringFragment::new(&doc).render(MarkupKind::Markdown));
            Some(
                format_parameter(
                    &model,
                    param.name.as_str(),
                    param.inferred_type(&model)?,
                    documentation.as_deref(),
                )
                .into(),
            )
        }
        Selection::Keyword { keyword, call } => {
            model.scope(call.into())?;
            let signature = resolved_call_signature(&model, call)?;
            let name = keyword.arg.as_ref()?.as_str();
            let argument = call
                .arguments
                .iter_source_order()
                .position(|argument| argument.range() == keyword.range())?;
            let parameter_index = signature
                .argument_to_displayed_parameter_mapping
                .get(argument)
                .copied()
                .flatten()?;
            let parameter = signature.parameters.get(parameter_index)?;
            let callee = call.func.inferred_type(&model)?;
            let documentation = callee
                .provided_data(db, &model.program_environment())
                .and_then(|data| data.downcast_ref::<crate::ty::Documentation>())
                .and_then(|docs| {
                    docs.parameters.iter().find_map(|(parameter, text)| {
                        (parameter.as_str() == name)
                            .then(|| Docstring::new(text.to_string()).render(MarkupKind::Markdown))
                    })
                })
                .or_else(|| {
                    let doc = signature
                        .definition
                        .and_then(|definition| definition.docstring(db))?;
                    Docstring::new(doc.to_string())
                        .parameter_documentation()
                        .swap_remove(name)
                        .map(|doc| DocstringFragment::new(&doc).render(MarkupKind::Markdown))
                });
            Some(format_parameter(&model, name, parameter.ty, documentation.as_deref()).into())
        }
        Selection::LoadItem { call: _, item } => {
            let (target, value): (AnyNodeRef<'_>, _) = match item {
                ArgOrKeyword::Arg(expr) => (expr.into(), expr),
                ArgOrKeyword::Keyword(keyword) => (keyword.into(), &keyword.value),
            };
            let (name, _) = string_value(&source[value.range()])?;
            let index = ty_python_core::semantic_index(db, model.program_file());
            let [definition] = index.try_definitions(target)? else {
                return None;
            };
            Some(format_for_name(&model, &name, model.definition_type(*definition)).into())
        }
        Selection::LoadModule(call) => {
            let loaded = sema.resolve_load_stmt(file, call)?;
            let mut text = format!("```python\n(module) {}\n```\n", &source[token.range()]);
            if let Some(doc) = module_doc(&sema, loaded) {
                text.push_str(&Docstring::new(doc.to_string()).render(MarkupKind::Markdown));
                text.push('\n');
            }
            Some(text.into())
        }
        Selection::String(_) => None,
    }
}

fn keyword_hover(keyword: &str) -> Option<Hover> {
    let docs = match keyword {
        "break" => docs::BREAK_DOCS,
        "continue" => docs::CONTINUE_DOCS,
        "def" => docs::DEF_DOCS,
        "for" => docs::FOR_DOCS,
        "if" => docs::IF_DOCS,
        "load" => docs::LOAD_DOCS,
        "pass" => docs::PASS_DOCS,
        "return" => docs::RETURN_DOCS,
        _ => return None,
    };
    Some(docs.to_owned().into())
}

fn type_comment_hover(
    db: &Database,
    model: &SemanticModel<'_>,
    sema: &Source<'_>,
    file: File,
    comment: &starpls_syntax::TypeComment,
    offset: ruff_text_size::TextSize,
) -> Option<Hover> {
    let local = u32::from(offset - comment.range.start()).into();
    let tree = comment.parsed.syntax();
    let token = pick_best_token(tree.token_at_offset(local), |kind| match kind {
        T![ident] => 2,
        T!['('] | T![')'] | T!['['] | T![']'] | T!['{'] | T!['}'] => 0,
        kind if kind.is_trivia_token() => 0,
        _ => 1,
    })?;
    if token.kind().is_keyword() {
        return keyword_hover(token.text());
    }
    let segment = ast::PathSegment::cast(token.parent()?)?;
    let path = ast::PathType::cast(segment.syntax().parent()?)?;
    let ty = if let Some(owner) = sema.type_comment_owner(file, offset) {
        model.provided_annotation_type_at(owner, offset)
    } else {
        // Unattached comments can describe builtins, but have no declaration
        // scope in which local or prelude names acquire a type contract.
        let names = path
            .segments()
            .filter_map(|segment| segment.value())
            .collect::<Vec<_>>();
        let [name] = names.as_slice() else {
            return Some("```python\n(type) Unknown\n```\n".to_owned().into());
        };
        let name = name.text();
        db.annotation_builtin(file, name)
            .and_then(|ty| ty.to_instance_approximation(db, &model.program_environment()))
    }
    .unwrap_or(Type::unknown());
    let mut text = format!(
        "```python\n(type) {}\n```\n",
        ty.display(db, &model.program_environment())
    );
    if let Some(doc) = type_documentation(model, ty) {
        text.push_str(&doc.render(MarkupKind::Markdown));
        text.push('\n');
    }
    Some(text.into())
}

fn module_doc(sema: &Source<'_>, file: File) -> Option<Box<str>> {
    let source = file.contents(sema.db);
    let parsed = parsed_module(sema.db, file).load(sema.db);
    let first = parsed.syntax().body.first()?;
    if syntax_info(sema.db, file)
        .first()
        .is_some_and(|comment| comment.range.start() < first.start())
    {
        return None;
    }
    let Stmt::Expr(stmt) = first else {
        return None;
    };
    let Expr::StringLiteral(_) = stmt.value.as_ref() else {
        return None;
    };
    if !starpls_syntax::supports_expr(stmt.value.as_ref().into(), parsed.tokens())
        || expr_range(&stmt.value, stmt.into(), parsed.tokens()) != stmt.value.range()
    {
        return None;
    }
    string_value(&source[stmt.value.range()]).map(|(doc, _)| doc)
}

pub(crate) fn is_function_type(ty: Type<'_>) -> bool {
    matches!(
        ty,
        Type::FunctionLiteral(_)
            | Type::Callable(_)
            | Type::BoundMethod(_)
            | Type::KnownBoundMethod(_)
    )
}

pub(crate) fn callable_display_type<'db>(model: &SemanticModel<'db>, ty: Type<'db>) -> Type<'db> {
    if matches!(ty, Type::NominalInstance(_)) {
        ty.map_callable_signatures(
            model.db(),
            &model.program_environment(),
            CallableTypeKind::Regular,
            std::convert::identity,
        )
        .unwrap_or(ty)
    } else {
        ty
    }
}

fn format_for_name<'db>(model: &SemanticModel<'db>, name: &str, ty: Type<'db>) -> String {
    let db = model.db();
    let environment = model.program_environment();
    let mut text = String::from("```python\n");

    let display_ty = callable_display_type(model, ty);
    // Callable objects retain their call signatures in name hovers.
    if is_function_type(display_ty) {
        text.push_str("(function) ");
    } else {
        text.push_str("(variable) ");
        text.push_str(name);
        text.push_str(": ");
    }

    write!(&mut text, "{}", display_ty.display(db, &environment)).unwrap();
    text.push_str("\n```\n");

    let doc = type_documentation(model, ty);
    if let Some(doc) = doc {
        text.push_str(&doc.render(MarkupKind::Markdown));
        text.push('\n');
    }

    text
}

fn type_documentation<'db>(model: &SemanticModel<'db>, ty: Type<'db>) -> Option<Docstring> {
    let db = model.db();
    let environment = model.program_environment();
    if let Some(doc) = ty
        .provided_data(db, &environment)
        .and_then(|data| data.downcast_ref::<crate::ty::Documentation>())
        .and_then(|docs| docs.text.as_deref())
    {
        return Some(Docstring::new(doc.to_owned()));
    }
    let definition = ty.definition(db, &environment)?.definition()?;
    let native = match definition.program_file(db).file(db).path(db) {
        ruff_db::files::FilePath::SystemVirtual(path) => {
            path.as_str().starts_with("starpls-native:")
        }
        _ => false,
    };
    if native || is_function_type(ty) {
        definition
            .docstring(db)
            .map(|doc| Docstring::new(doc.to_string()))
    } else {
        None
    }
}

fn format_parameter<'db>(
    model: &SemanticModel<'db>,
    name: &str,
    ty: Type<'db>,
    documentation: Option<&str>,
) -> String {
    let mut text = format!(
        "```python\n(parameter) {name}: {}\n```\n",
        ty.display(model.db(), &model.program_environment())
    );
    if let Some(doc) = documentation {
        text.push_str(doc);
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use expect_test::expect;
    use expect_test::Expect;
    use salsa::Setter;
    use starpls_hir::Db;

    use crate::Analysis;
    use crate::FilePosition;

    fn install_native(analysis: &mut Analysis) {
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                Default::default(),
            )
            .unwrap();
    }

    fn check_hover(fixture: &str, expect: Expect) {
        let (mut analysis, fixture) = Analysis::from_single_file_fixture(fixture);
        install_native(&mut analysis);
        let hover = analysis
            .snapshot()
            .hover(
                fixture
                    .cursor_pos
                    .map(|(file_id, pos)| FilePosition { file_id, pos })
                    .unwrap(),
            )
            .unwrap()
            .unwrap();

        expect.assert_eq(&hover.contents.value);
    }

    #[test]
    fn load_cursor_roles() {
        for (input, hover, navigates) in [
            ("load(\"defs.bzl\", al$0ias = \"value\")", None, false),
            (
                "load(\"defs.bzl\", alias = \"val$0ue\")",
                Some("(variable) value: Literal[1]"),
                true,
            ),
            ("load(\"defs.bzl\", \"value\"$0,)", None, false),
            (
                "load(\"defs.bzl\", \"value\"$0)",
                Some("(variable) value: Literal[1]"),
                true,
            ),
            (
                "load(\"defs.bzl\", alias = # type: int$0\n \"value\")",
                Some("(type) int"),
                false,
            ),
        ] {
            let (mut analysis, loader) = Analysis::new_for_test();
            let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
            fixture.add_file(&mut analysis.db, "defs.bzl", "value = 1\n");
            fixture.add_file(&mut analysis.db, "main.bzl", input);
            loader.add_files_from_fixture(&fixture);
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let snapshot = analysis.snapshot();
            let position = FilePosition { file_id, pos };
            let actual = snapshot.hover(position.clone()).unwrap();
            match hover {
                Some(expected) => {
                    assert!(actual.unwrap().contents.value.contains(expected), "{input}")
                }
                None => assert!(actual.is_none(), "{input}"),
            }
            assert_eq!(
                snapshot.goto_definition(position, false).unwrap().is_some(),
                navigates,
                "{input}"
            );
        }
    }

    #[test]
    fn module_documentation_uses_first_direct_child() {
        for (source, documented) in [
            ("# ordinary comment\n\n\"module documentation\"\n", true),
            ("# type: ignore\n\"module documentation\"\n", false),
            ("(\"module documentation\")\n", false),
            ("value = 1\n\"module documentation\"\n", false),
        ] {
            let (mut analysis, loader) = Analysis::new_for_test();
            let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
            fixture.add_file(&mut analysis.db, "defs.bzl", source);
            fixture.add_file(
                &mut analysis.db,
                "main.bzl",
                "load(\"defs$0.bzl\", \"value\")",
            );
            loader.add_files_from_fixture(&fixture);
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let hover = analysis
                .snapshot()
                .hover(FilePosition { file_id, pos })
                .unwrap()
                .unwrap();
            assert_eq!(
                hover.contents.value.contains("module documentation"),
                documented,
                "{source}"
            );
        }
    }

    #[test]
    fn recovery_and_comment_boundaries() {
        for (source, expected) in [
            ("def f(name): pass\nf(na$0me=)", "(parameter) name: Unknown"),
            ("case = 1\ncase$0# type: int", "(variable) case: Literal[1]"),
            ("value = 1 # type: int$0\n", "(type) int"),
        ] {
            let (analysis, fixture) = Analysis::from_single_file_fixture(source);
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let hover = analysis
                .snapshot()
                .hover(FilePosition { file_id, pos })
                .unwrap()
                .unwrap();
            assert!(
                hover.contents.value.contains(expected),
                "{source}: {}",
                hover.contents.value
            );
        }
        let (analysis, fixture) = Analysis::from_single_file_fixture("# type: de$0f");
        let (file_id, pos) = fixture.cursor_pos.unwrap();
        let hover = analysis
            .snapshot()
            .hover(FilePosition { file_id, pos })
            .unwrap()
            .unwrap();
        assert_eq!(hover.contents.value, super::docs::DEF_DOCS);
    }

    #[test]
    fn type_comments_resolve_in_their_declaration_context() {
        for (body, expected) in [
            ("def f(x, # type: P$0\n): pass", "P"),
            ("def f():\n    if True:\n        x = P() # type: P$0", "P"),
            ("def f():\n    Q = provider()\n    x = Q() # type: Q$0", "Q"),
            ("def f(): # type: () -> P$0\n    pass", "P"),
            ("x = 0; # type: P$0", "Unknown"),
            (
                "def f():\n    \"doc\"\n    # type: () -> P$0\n    pass",
                "Unknown",
            ),
            ("def f(x=(\n    1 # type: P$0\n)): pass", "Unknown"),
        ] {
            let input = format!("P = provider()\n{body}\n");
            let (mut analysis, fixture) = Analysis::from_single_file_fixture(&input);
            install_native(&mut analysis);
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let hover = analysis
                .snapshot()
                .hover(FilePosition { file_id, pos })
                .unwrap()
                .unwrap();
            assert_eq!(
                hover.contents.value,
                format!("```python\n(type) {expected}\n```\n"),
                "{input}"
            );
        }
    }

    #[test]
    fn native_record_contracts_and_documentation() {
        for (expression, expected) in [
            ("context.attr.arbitrary", "Unknown"),
            ("context.file.input.path", "str"),
            ("context.files.input[0].path", "str"),
            ("record.arbitrary", "str"),
        ] {
            let source = format!(
                "def inspect(context, record):\n    # type: (ctx, struct[string]) -> None\n    {expression}$0\n"
            );
            let (mut analysis, fixture) = Analysis::from_single_file_fixture(&source);
            install_native(&mut analysis);
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let snapshot = analysis.snapshot();
            let hover = snapshot
                .hover(FilePosition { file_id, pos })
                .unwrap()
                .unwrap();
            let field = expression.rsplit('.').next().unwrap();
            assert!(
                hover
                    .contents
                    .value
                    .starts_with(&format!("```python\n(field) {field}: {expected}\n```\n")),
                "{source}: {}",
                hover.contents.value
            );
            if field == "path" {
                assert!(
                    hover.contents.value.contains("execution path"),
                    "{}",
                    hover.contents.value
                );
            }
            let diagnostics = ty_python_semantic::check_file_unwrap(
                &snapshot.db,
                snapshot.db.starlark_program_file(file_id),
            );
            assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}");
        }
        for source in ["a$0ttr", "# type: at$0tr"] {
            let (mut analysis, fixture) = Analysis::from_single_file_fixture(source);
            install_native(&mut analysis);
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let hover = analysis
                .snapshot()
                .hover(FilePosition { file_id, pos })
                .unwrap()
                .unwrap();
            assert!(
                hover.contents.value.contains("attribute"),
                "{}",
                hover.contents.value
            );
        }
    }

    #[test]
    fn primitive_members_keep_starlark_contracts() {
        for (ty, expected) in [
            (
                "string",
                "capitalize count elems endswith find format index isalnum
                isalpha isdigit islower isspace istitle isupper join lower lstrip
                partition removeprefix removesuffix replace rfind rindex rpartition
                rsplit rstrip split splitlines startswith strip title upper",
            ),
            ("bytes", "elems"),
            ("list[int]", "append clear extend index insert pop remove"),
            (
                "dict[string, int]",
                "clear get items keys pop popitem setdefault update values",
            ),
            ("int", ""),
            ("float", ""),
            ("bool", ""),
            ("tuple[int, ...]", ""),
            ("range", ""),
        ] {
            let source = format!("def inspect(value):\n    # type: ({ty}) -> None\n    value.$0");
            let (analysis, fixture) = Analysis::from_single_file_fixture(&source);
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let items = analysis
                .snapshot()
                .completions(FilePosition { file_id, pos }, None)
                .unwrap()
                .unwrap();
            let mut names: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
            names.sort_unstable();
            assert_eq!(
                names,
                expected.split_whitespace().collect::<Vec<_>>(),
                "{ty}"
            );
        }
        for (expression, declaration, doc) in [
            ("'text'.upper$0", "-> str", "uppercase"),
            (
                "{'key': 1}.items$0",
                "-> list[tuple[str, int]]",
                "key/value",
            ),
        ] {
            let (analysis, fixture) = Analysis::from_single_file_fixture(expression);
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let hover = analysis
                .snapshot()
                .hover(FilePosition { file_id, pos })
                .unwrap()
                .unwrap();
            assert!(
                hover.contents.value.contains(declaration),
                "{}",
                hover.contents.value
            );
            assert!(
                hover.contents.value.contains(doc),
                "{}",
                hover.contents.value
            );
        }
    }

    #[test]
    fn context_attribute_option_updates_hover_and_completion() {
        for factory in ["rule", "repository_rule"] {
            let source = format!(
                r#"def implementation(context):
    context.attr.value

example = {factory}(
    implementation = implementation,
    attrs = {{"value": attr.string()}},
)
"#
            );
            let (mut analysis, fixture) = Analysis::from_single_file_fixture(&source);
            install_native(&mut analysis);
            let file_id = fixture.main_file();
            let offset = source.find("context.attr.value").unwrap() + "context.attr.".len();
            for enabled in [false, true, false, true] {
                analysis.db.environment().set_options(&mut analysis.db).to(
                    crate::InferenceOptions {
                        infer_ctx_attributes: enabled,
                        use_code_flow_analysis: false,
                        allow_unused_definitions: false,
                    },
                );
                let snapshot = analysis.snapshot();
                let hover = snapshot
                    .hover(FilePosition {
                        file_id,
                        pos: (offset as u32 + 2).into(),
                    })
                    .unwrap()
                    .unwrap();
                let expected = if enabled { "str" } else { "Unknown" };
                assert!(
                    hover
                        .contents
                        .value
                        .contains(&format!("(field) value: {expected}\n")),
                    "{factory}, enabled={enabled}: {}",
                    hover.contents.value
                );
                let completions = snapshot
                    .completions(
                        FilePosition {
                            file_id,
                            pos: (offset as u32).into(),
                        },
                        None,
                    )
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    completions.iter().any(|item| item.label == "value"),
                    enabled,
                    "{factory}, enabled={enabled}: {completions:?}"
                );
            }
            for (attribute, expected) in [("int", "int"), ("bool", "bool"), ("string", "str")] {
                analysis.update_file(
                    file_id,
                    source.replace("attr.string()", &format!("attr.{attribute}()")),
                );
                let hover = analysis
                    .snapshot()
                    .hover(FilePosition {
                        file_id,
                        pos: (offset as u32 + 2).into(),
                    })
                    .unwrap()
                    .unwrap();
                assert!(
                    hover
                        .contents
                        .value
                        .contains(&format!("(field) value: {expected}\n")),
                    "{factory}: {}",
                    hover.contents.value
                );
            }
        }
    }

    #[test]
    fn dictionary_attributes_offer_names_and_locations() {
        for (setup, attrs, expected) in [
            ("", "{'_tool': attr.label()}", "Target | None"),
            (
                "attrs = {'_tool': attr.label()}\nattrs.update({'_tool': attr.string()})\n",
                "attrs",
                "Unknown",
            ),
        ] {
            let source = format!(
                "def implementation(context):\n    context.attr._tool\n\n{setup}example = rule(implementation=implementation, attrs={attrs})\n"
            );
            let (mut analysis, fixture) = Analysis::from_single_file_fixture(&source);
            install_native(&mut analysis);
            analysis
                .db
                .environment()
                .set_options(&mut analysis.db)
                .to(crate::InferenceOptions {
                    infer_ctx_attributes: true,
                    use_code_flow_analysis: false,
                    allow_unused_definitions: false,
                });
            let file_id = fixture.main_file();
            let start = source.find("context.attr._tool").unwrap() + "context.attr.".len();
            let position = FilePosition {
                file_id,
                pos: (start as u32 + 2).into(),
            };
            let snapshot = analysis.snapshot();
            let hover = snapshot.hover(position.clone()).unwrap().unwrap();
            assert!(
                hover
                    .contents
                    .value
                    .contains(&format!("(field) _tool: {expected}\n")),
                "{}",
                hover.contents.value
            );
            let completions = snapshot
                .completions(
                    FilePosition {
                        file_id,
                        pos: (start as u32).into(),
                    },
                    None,
                )
                .unwrap()
                .unwrap();
            assert!(
                completions.iter().any(|item| item.label == "_tool"),
                "{completions:?}"
            );
            let locations = snapshot.goto_definition(position, false).unwrap().unwrap();
            let [crate::LocationLink::Local {
                target_file_id,
                target_selection_range,
                origin_selection_range: _,
                target_range: _,
            }] = locations.as_slice()
            else {
                panic!("expected one attribute definition: {locations:?}");
            };
            assert_eq!(*target_file_id, file_id.into());
            assert_eq!(&source[*target_selection_range], "'_tool'");
            let expected_start = source.find("'_tool'").unwrap();
            assert_eq!(
                u32::from(target_selection_range.start()) as usize,
                expected_start
            );
        }
    }

    #[test]
    fn context_registration_requires_resolved_unambiguous_identity() {
        for (annotation, registration, expected) in [
            (
                "",
                "register = rule
callback = implementation
example = register(implementation=callback, attrs={'value': attr.string()})",
                "str",
            ),
            (
                "",
                "first = rule(implementation=implementation, attrs={'value': attr.string()})
second = rule(implementation=implementation, attrs={'value': attr.int()})",
                "Unknown",
            ),
            (
                "    # type: (Unknown) -> None\n",
                "example = rule(implementation=implementation, attrs={'value': attr.string()})",
                "Unknown",
            ),
            (
                "",
                "def rule(implementation, attrs):
    pass
example = rule(implementation=implementation, attrs={'value': attr.string()})",
                "Unknown",
            ),
            (
                "",
                "attrs = {'value': attr.string()}
example = rule(implementation=implementation, attrs=attrs)",
                "Unknown",
            ),
            (
                "",
                "def register():
    return rule(implementation=implementation, attrs={'value': attr.string()})",
                "Unknown",
            ),
            (
                "",
                "example = rule(implementation=implementation, attrs={'value': attr.string()})
other = rule(**unknown_arguments)",
                "Unknown",
            ),
            (
                "",
                "example = rule(implementation=implementation, attrs={'value': attr.string()})
other = rule(implementation=unknown_callback)",
                "Unknown",
            ),
        ] {
            let source = format!(
                "def implementation(context):\n{annotation}    context.attr.val$0ue\n\n{registration}\n"
            );
            let (mut analysis, fixture) = Analysis::from_single_file_fixture(&source);
            install_native(&mut analysis);
            analysis
                .db
                .environment()
                .set_options(&mut analysis.db)
                .to(crate::InferenceOptions {
                    infer_ctx_attributes: true,
                    use_code_flow_analysis: false,
                    allow_unused_definitions: false,
                });
            let (file_id, pos) = fixture.cursor_pos.unwrap();
            let hover = analysis
                .snapshot()
                .hover(FilePosition { file_id, pos })
                .unwrap()
                .unwrap();
            assert!(
                hover
                    .contents
                    .value
                    .contains(&format!("(field) value: {expected}\n")),
                "{source}: {}",
                hover.contents.value
            );
        }
    }

    #[test]
    fn detached_native_comment_does_not_read_prelude() {
        let (mut analysis, _) = Analysis::new_for_test();
        install_native(&mut analysis);
        let prelude = analysis
            .open_document(
                std::path::Path::new("/prelude.bzl"),
                starpls_common::Dialect::Bazel,
                Some(starpls_common::FileInfo::Bazel {
                    api_context: starpls_bazel::APIContext::Prelude,
                    is_external: false,
                }),
                "Label = provider()\n".to_owned(),
                1,
            )
            .unwrap();
        analysis.set_bazel_prelude_file(prelude);
        let file_id = analysis
            .open_document(
                std::path::Path::new("/BUILD"),
                starpls_common::Dialect::Bazel,
                Some(starpls_common::FileInfo::Bazel {
                    api_context: starpls_bazel::APIContext::Build,
                    is_external: false,
                }),
                "# type: Label\n".to_owned(),
                1,
            )
            .unwrap();
        let hover = analysis
            .snapshot()
            .hover(FilePosition {
                file_id,
                pos: 11.into(),
            })
            .unwrap()
            .unwrap();
        assert!(
            hover
                .contents
                .value
                .starts_with("```python\n(type) Label\n```\n"),
            "{}",
            hover.contents.value
        );
        assert!(
            hover.contents.value.contains("target"),
            "{}",
            hover.contents.value
        );
    }

    #[test]
    fn loaded_field_hover_and_completion_follow_edits() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        install_native(&mut analysis);
        let dependency =
            fixture.add_file(&mut analysis.db, "defs.bzl", "record = struct(value=1)\n");
        let source = "load(\"defs.bzl\", \"record\")\nrecord.value\nrecord.";
        let file_id = fixture.add_file(&mut analysis.db, "main.bzl", source);
        loader.add_files_from_fixture(&fixture);
        for (value, expected) in [
            ("1", "Literal[1]"),
            ("\"text\"", "Literal[\"text\"]"),
            ("1", "Literal[1]"),
        ] {
            analysis.update_file(
                dependency,
                format!("Info = provider()\nrecord = struct(value={value}, __explicit__=1, Factory=Info)\n"),
            );
            let snapshot = analysis.snapshot();
            let hover = snapshot
                .hover(FilePosition {
                    file_id,
                    pos: (source.find("record.value").unwrap() as u32 + 9).into(),
                })
                .unwrap()
                .unwrap();
            assert!(
                hover.contents.value.contains(expected),
                "{}",
                hover.contents.value
            );
            let items = snapshot
                .completions(
                    FilePosition {
                        file_id,
                        pos: (source.len() as u32).into(),
                    },
                    None,
                )
                .unwrap()
                .unwrap();
            let mut names: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
            names.sort_unstable();
            assert_eq!(names, ["Factory", "__explicit__", "value"], "{items:?}");
            assert!(
                items.iter().any(|item| item.label == "Factory"
                    && item.kind == crate::completions::CompletionItemKind::Function),
                "{items:?}"
            );
        }
    }

    #[test]
    fn check_variable() {
        check_hover(
            r#"
a$0bc = 123
"#,
            expect![[r#"
                ```python
                (variable) abc: Literal[123]
                ```
            "#]],
        );
    }

    #[test]
    fn parenthesized_singleton_loop_target() {
        check_hover(
            "for (x,) in [(1,)]:\n    x$0\n",
            expect![[r#"
                ```python
                (variable) x: int
                ```
            "#]],
        );
    }

    #[test]
    fn check_def_stmt() {
        check_hover(
            r#"
def f$0oo(x, y):
    """Doc string"""
    pass
"#,
            expect![[r#"
                ```python
                (function) def foo(x, y) -> Unknown
                ```
                Doc string
            "#]],
        );
    }

    #[test]
    fn check_call_expr() {
        check_hover(
            r#"
def foo(x, y):
    """Doc string

    x: Unsectioned parameter text.
    """
    pass

f$0oo(1, 2)
"#,
            expect!["```python\n(function) def foo(x, y) -> Unknown\n```\nDoc string  \n  \nx: Unsectioned parameter text.\n"],
        );
    }

    #[test]
    fn check_type() {
        check_hover(
            r#"
x = 1 # type: i$0nt
"#,
            expect![[r#"
                ```python
                (type) int
                ```
            "#]],
        );
    }

    #[test]
    fn check_param() {
        // Unsectioned text remains in the function hover, but is not a
        // parameter description in the shared docstring parser.
        check_hover(
            "def foo(a$0bc):\n    \"\"\"abc: Unsectioned parameter text.\"\"\"\n    pass",
            expect![[r#"
                ```python
                (parameter) abc: Unknown
                ```
            "#]],
        );
        check_hover(
            r#"
def foo(a$0bc):
    """
    Args:
        abc: Easy as 123!
    """
    pass
"#,
            expect![[r#"
                ```python
                (parameter) abc: Unknown
                ```
                Easy as 123!
            "#]],
        );
    }

    #[test]
    fn check_arg() {
        check_hover(
            r#"
def foo(abc):
    """
    Args:
        abc: Easy as 123!
    """
    pass

foo(a$0bc = 123)
"#,
            expect![[r#"
                ```python
                (parameter) abc: Unknown
                ```
                Easy as 123!
            "#]],
        );
    }

    #[test]
    fn check_field() {
        check_hover(
            r#"
foo = struct(bar = 123)
foo.b$0ar
"#,
            expect![[r#"
                ```python
                (field) bar: Literal[123]
                ```
            "#]],
        );
    }

    #[test]
    fn check_method() {
        check_hover(
            r#"
def bar():
    pass

foo = struct(bar = bar)
foo.b$0ar
"#,
            expect![[r#"
                ```python
                (method) def bar() -> Unknown
                ```
            "#]],
        );
    }

    #[test]
    fn check_provider_doc() {
        check_hover(
            r#"
Foo$0Info = provider(doc = "The foo provider")
"#,
            expect![[r#"
                ```python
                (variable) FooInfo: <class 'FooInfo'>
                ```
                The foo provider
            "#]],
        );
    }

    #[test]
    fn provider_comment_keeps_documentation() {
        check_hover(
            r#"
Info = provider(doc = "Source provider documentation")
value = Info() # type: In$0fo
"#,
            expect![[r#"
                ```python
                (type) Info
                ```
                Source provider documentation
            "#]],
        );
    }

    #[test]
    fn check_provider_field_doc() {
        check_hover(
            r#"
FooInfo = provider(
    doc = "The foo provider",
    fields = {
        "bar": "The bar field",
    },
)

foo = FooInfo(bar = "bar")
foo.b$0ar
"#,
            expect![[r#"
                ```python
                (field) bar: Unknown
                ```
                The bar field
            "#]],
        );
    }

    #[test]
    fn check_rule_attr() {
        check_hover(
            r#"
foo = rule(
    attrs = {
        "bar": attr.string(doc = "The bar attr"),
    },
)

foo(
    name = "foo",
    b$0ar = "bar",
)
"#,
            expect![[r#"
                ```python
                (parameter) bar: str | select[str | None] | None
                ```
                The bar attr
            "#]],
        );
    }
}
