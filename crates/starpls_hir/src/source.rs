//! Source-only Starlark conventions over the canonical parsed module.

use ruff_python_ast as py;
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::token::Tokens;
use ruff_python_ast::visitor::source_order;
use ruff_python_ast::visitor::source_order::SourceOrderVisitor;
use ruff_python_ast::HasNodeIndex;
use ruff_python_ast::NodeIndex;
use ruff_text_size::Ranged;
use ruff_text_size::TextRange;
use ruff_text_size::TextSize;
use rustc_hash::FxHashMap;
use rustc_hash::FxHashSet;
use starpls_common::Diagnostic;
use starpls_common::File;
use starpls_syntax::ast::AstNode;
use starpls_syntax::source::parameter_range;
use starpls_syntax::source::string_value;
use starpls_syntax::source::suite_range;
use starpls_syntax::TypeComment;

use crate::Db;

/// Comments and loads have source conventions independent of inferred types.
#[derive(Clone, Copy)]
pub struct Source<'db> {
    pub db: &'db dyn Db,
}

impl<'db> Source<'db> {
    pub fn new(db: &'db dyn Db) -> Self {
        Self { db }
    }

    pub fn type_comment_annotation(&self, file: File, owner: NodeIndex) -> Option<TextRange> {
        source_info(self.db, file).annotations.get(&owner).copied()
    }

    pub fn type_comment_owner(&self, file: File, offset: TextSize) -> Option<NodeIndex> {
        source_info(self.db, file)
            .annotations
            .iter()
            .find_map(|(owner, range)| range.contains_inclusive(offset).then_some(*owner))
    }

    pub fn is_load_stmt(&self, file: File, call: &py::ExprCall) -> bool {
        source_info(self.db, file)
            .loads
            .contains_key(&call.node_index().load())
    }

    pub fn resolve_load_stmt(&self, file: File, call: &py::ExprCall) -> Option<File> {
        let module = source_info(self.db, file)
            .loads
            .get(&call.node_index().load())?;
        self.db.load_file(module, file.dialect, file).ok().flatten()
    }
}

pub fn diagnostics_for_file(db: &dyn Db, file: File) -> impl Iterator<Item = Diagnostic> + '_ {
    starpls_common::syntax_diagnostics(db, file)
        .iter()
        .chain(source_info(db, file).diagnostics.iter())
        .cloned()
}

#[derive(Debug, PartialEq, Eq)]
struct SourceInfo {
    annotations: FxHashMap<NodeIndex, TextRange>,
    loads: FxHashMap<NodeIndex, Box<str>>,
    diagnostics: Vec<Diagnostic>,
}

fn source_info(db: &dyn Db, file: File) -> &SourceInfo {
    source_info_query(db, file.source, (file.dialect, file.info))
}

#[salsa::tracked(returns(ref))]
fn source_info_query(
    db: &dyn Db,
    source: ruff_db::files::File,
    context: (starpls_common::Dialect, Option<starpls_common::FileInfo>),
) -> SourceInfo {
    let (dialect, info) = context;
    let file = File {
        source,
        dialect,
        info,
    };
    let source = file.contents(db);
    let parsed = starpls_common::parsed_module(db, file).load(db);
    let mut visitor = SourceVisitor {
        source: &source,
        tokens: parsed.tokens(),
        comments: starpls_common::syntax_info(db, file),
        excluded: starpls_common::syntax_exclusions(db, file)
            .iter()
            .copied()
            .collect(),
        info: SourceInfo {
            annotations: FxHashMap::default(),
            loads: FxHashMap::default(),
            diagnostics: Vec::new(),
        },
    };
    for statement in parsed.suite() {
        let kind = match statement {
            py::Stmt::If(_) => Some("if"),
            py::Stmt::For(_) => Some("for"),
            _ => None,
        };
        if let Some(kind) = kind {
            visitor.info.diagnostics.push(starpls_common::diagnostic(
                file,
                starpls_common::DiagnosticId::InvalidSyntax,
                starpls_common::Severity::Error,
                starpls_syntax::TextRange::new(
                    u32::from(statement.start()).into(),
                    u32::from(statement.end()).into(),
                ),
                format!("Starlark does not allow top-level {kind} statements"),
                [],
            ));
        }
        visitor.visit_stmt(statement);
    }
    visitor.info
}

struct SourceVisitor<'a> {
    source: &'a str,
    tokens: &'a Tokens,
    comments: &'a [TypeComment],
    excluded: FxHashSet<NodeIndex>,
    info: SourceInfo,
}

fn comment_type_range(comment: &TypeComment, ty: &starpls_syntax::ast::Type) -> TextRange {
    let range = ty.syntax().text_range();
    TextRange::new(
        comment.range.start() + TextSize::from(u32::from(range.start())),
        comment.range.start() + TextSize::from(u32::from(range.end())),
    )
}

impl<'a> SourceVisitor<'a> {
    fn comment_in(&self, range: TextRange) -> Option<&'a TypeComment> {
        self.comments[self
            .comments
            .partition_point(|comment| comment.range.start() < range.start())..]
            .first()
            .filter(|comment| range.contains_range(comment.range))
    }

    fn parameters(&mut self, syntax: &py::Parameters, specification: &[Option<TextRange>]) {
        let mut parameters = syntax
            .iter_source_order()
            .map(|parameter| {
                (
                    Some(parameter),
                    parameter_range(parameter, syntax, self.tokens),
                )
            })
            .collect::<Vec<_>>();
        if let Some(range) = starpls_syntax::source::bare_star_range(syntax, self.tokens) {
            parameters.push((None, range));
        }
        parameters.sort_by_key(|(_, range)| range.start());
        for (index, (parameter, range)) in parameters.iter().copied().enumerate() {
            let Some(parameter) = parameter else {
                continue;
            };
            let end = parameters
                .get(index + 1)
                .map_or(syntax.end(), |(_, range)| range.start());
            let annotation = self
                .comment_in(TextRange::new(range.end(), end.max(range.end())))
                .and_then(|comment| {
                    comment
                        .parsed
                        .tree()
                        .type_()
                        .map(|ty| comment_type_range(comment, &ty))
                })
                .or_else(|| specification.get(index).copied().flatten());
            if let Some(annotation) = annotation {
                self.info
                    .annotations
                    .insert(parameter.as_parameter().node_index().load(), annotation);
            }
        }
    }

    fn statement(&mut self, statement: &py::Stmt) {
        if self.excluded.contains(&statement.node_index().load()) {
            return;
        }
        match statement {
            py::Stmt::FunctionDef(function) => {
                let suite = suite_range(
                    self.tokens,
                    &function.body,
                    function.parameters.end(),
                    function.end(),
                    None,
                );
                let comment = suite.and_then(|range| {
                    self.comment_in(TextRange::new(
                        range.start(),
                        function
                            .body
                            .first()
                            .map_or(range.end(), Ranged::start)
                            .max(range.start()),
                    ))
                });
                let mut specification = Vec::new();
                if let Some(comment) = comment {
                    if let Some(function_type) = comment.parsed.tree().function_type() {
                        if let Some(parameters) = function_type.parameter_types() {
                            specification.extend(parameters.types().map(|parameter| {
                                parameter.type_().map(|ty| comment_type_range(comment, &ty))
                            }));
                        }
                        if let Some(ty) = function_type.ret_type() {
                            self.info.annotations.insert(
                                function.node_index().load(),
                                comment_type_range(comment, &ty),
                            );
                        }
                    }
                }
                self.parameters(&function.parameters, &specification);
            }
            py::Stmt::Assign(assignment) => {
                let end = self
                    .tokens
                    .after(assignment.end())
                    .iter()
                    .find(|token| matches!(token.kind(), TokenKind::Newline | TokenKind::Semi))
                    .map_or(TextSize::of(self.source), Ranged::start);
                if let Some(py::Expr::Name(target)) = assignment.targets.first() {
                    if let Some(comment) =
                        self.comment_in(TextRange::new(assignment.end(), end.max(assignment.end())))
                    {
                        if let Some(ty) = comment.parsed.tree().type_() {
                            self.info.annotations.insert(
                                target.node_index().load(),
                                comment_type_range(comment, &ty),
                            );
                        }
                    }
                }
            }
            py::Stmt::Expr(statement) => {
                if let py::Expr::Call(call) = statement.value.as_ref() {
                    if call
                        .func
                        .as_name_expr()
                        .is_some_and(|name| name.id == "load")
                    {
                        if let Some(module) = call
                            .arguments
                            .args
                            .first()
                            .and_then(|module| string_value(&self.source[module.range()]))
                        {
                            self.info.loads.insert(call.node_index().load(), module.0);
                        }
                        return;
                    }
                }
            }
            _ => {}
        }
        source_order::walk_stmt(self, statement);
    }
}

impl<'a> SourceOrderVisitor<'a> for SourceVisitor<'_> {
    fn visit_stmt(&mut self, statement: &'a py::Stmt) {
        stacker::maybe_grow(32 * 1024, 1024 * 1024, || self.statement(statement));
    }

    fn visit_expr(&mut self, expression: &'a py::Expr) {
        stacker::maybe_grow(32 * 1024, 1024 * 1024, || {
            if let py::Expr::Lambda(lambda) = expression {
                if let Some(parameters) = &lambda.parameters {
                    self.parameters(parameters, &[]);
                }
            }
            source_order::walk_expr(self, expression);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_database::TestDatabase;

    #[test]
    fn comments_keep_source_attachment_boundaries() {
        let mut db = TestDatabase::default();
        let file = starpls_common::open_document(
            &mut db,
            std::path::Path::new("comments.bzl"),
            starpls_common::Dialect::Standard,
            None,
            String::new(),
            0,
        )
        .unwrap();
        for (source, expected) in [
            (
                r#"a = 1; b = 2 # type: string
def f(
    x = (
        1 # type: bool
    ),
    # ordinary comment
    # type: int
    *, # type: float
    y, # type: string
):
    # type: bool
    # type: (string, string, string) -> int
    pass
"#,
                vec![("b", "string"), ("x", "int"), ("y", "string")],
            ),
            (
                r#"def f(x, y):
    # type: (int, string) -> bool
    return True
"#,
                vec![("x", "int"), ("y", "string"), ("f", "bool")],
            ),
        ] {
            starpls_common::update_file(&mut db, file, source.to_owned());
            let parsed = starpls_common::parsed_module(&db, file).load(&db);
            let mut annotations = source_info(&db, file)
                .annotations
                .iter()
                .collect::<Vec<_>>();
            annotations.sort_by_key(|(_, range)| range.start());
            let actual = annotations
                .into_iter()
                .map(|(owner, range)| {
                    let name = match parsed.get_by_index(*owner) {
                        py::AnyRootNodeRef::Stmt(py::Stmt::FunctionDef(function)) => {
                            function.name.as_str()
                        }
                        py::AnyRootNodeRef::Parameter(parameter) => parameter.name.as_str(),
                        py::AnyRootNodeRef::Expr(py::Expr::Name(name)) => name.id.as_str(),
                        other => panic!("unexpected annotation owner {other:?}"),
                    };
                    (name, &source[*range])
                })
                .collect::<Vec<_>>();
            assert_eq!(actual, expected);
        }
    }
}
