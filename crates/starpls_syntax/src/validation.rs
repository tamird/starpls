//! Starlark validation over Ruff's recovered AST and original tokens.

use ruff_python_ast as py;
use ruff_python_ast::token::parentheses_iterator;
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::token::Tokens;
use ruff_python_ast::visitor::source_order::SourceOrderVisitor;
use ruff_python_ast::visitor::source_order::{self};
use ruff_python_ast::ArgOrKeyword;
use ruff_python_ast::Expr;
use ruff_python_ast::HasNodeIndex;
use ruff_python_ast::NodeIndex;
use ruff_python_ast::Stmt;
use ruff_python_parser::ParseErrorType;
use ruff_python_parser::Parsed;
use ruff_python_parser::UnsupportedSyntaxErrorKind;
use ruff_text_size::Ranged;
use ruff_text_size::TextRange;

use crate::SyntaxError;

/// Validate Starlark syntax and identify complete subtrees with no Starlark semantics.
/// Invalid headers and unsupported container forms are recovered as opaque owners; valid
/// siblings remain available to semantic analysis.
pub fn validate(
    source: &str,
    parsed: &Parsed<py::ModModule>,
    errors: &mut dyn FnMut(SyntaxError),
) -> Vec<NodeIndex> {
    crate::lexical::validate(source, parsed.tokens(), errors);
    let mut validator = Validator {
        tokens: parsed.tokens(),
        errors,
        loads: Vec::new(),
        excluded: Vec::new(),
        statement_node: None,
    };
    let py::ModModule {
        node_index: _,
        range: _,
        body,
    } = parsed.syntax();
    validator.visit_body(body);
    for error in parsed.unsupported_syntax_errors() {
        // Other entries concern Python versions, not the Starlark grammar.
        if error.kind == UnsupportedSyntaxErrorKind::ParenthesizedKeywordArgumentName {
            validator.error(
                error.range(),
                "Keyword argument names cannot be parenthesized",
            );
        }
    }
    for error in parsed.errors() {
        // Only validated loads may interleave aliases and direct imports.
        if error.error == ParseErrorType::PositionalAfterKeywordArgument
            && validator
                .loads
                .iter()
                .any(|range| range.contains_range(error.range()))
        {
            continue;
        }
        // The local decoder already checked Starlark bytes, which allow UTF-8.
        if matches!(
            error.error,
            ParseErrorType::Lexical(ruff_python_parser::LexicalErrorType::InvalidByteLiteral)
        ) {
            continue;
        }
        validator.error(error.range(), error.error.to_string());
    }
    validator.excluded
}

/// Whether an expression root belongs to Starlark syntax. Validation excludes
/// its owning statement from semantic analysis when this returns false.
pub fn supports_expr(expr: py::ExprRef<'_>, tokens: &Tokens) -> bool {
    match expr {
        py::ExprRef::Name(py::ExprName {
            node_index: _,
            range: _,
            id,
            ctx: _,
        }) => id != "load",
        py::ExprRef::NumberLiteral(py::ExprNumberLiteral {
            node_index: _,
            range: _,
            value,
        }) => !value.is_complex(),
        py::ExprRef::StringLiteral(_) => single_string_token(expr.range(), tokens),
        py::ExprRef::BytesLiteral(_) => single_string_token(expr.range(), tokens),
        py::ExprRef::BinOp(py::ExprBinOp {
            node_index: _,
            range: _,
            left: _,
            op,
            right: _,
        }) => !matches!(op, py::Operator::Pow | py::Operator::MatMult),
        py::ExprRef::Compare(py::ExprCompare {
            node_index: _,
            range: _,
            operands: _,
            ops,
        }) => !ops
            .iter()
            .any(|op| matches!(op, py::CmpOp::Is | py::CmpOp::IsNot)),
        py::ExprRef::DictComp(py::ExprDictComp {
            node_index: _,
            range: _,
            key,
            value: _,
            generators: _,
        }) => key.is_some(),
        py::ExprRef::BooleanLiteral(_) => true,
        py::ExprRef::NoneLiteral(_) => true,
        py::ExprRef::BoolOp(_) => true,
        py::ExprRef::UnaryOp(_) => true,
        py::ExprRef::If(_) => true,
        py::ExprRef::Lambda(_) => true,
        py::ExprRef::List(_) => true,
        py::ExprRef::Tuple(_) => true,
        py::ExprRef::Dict(_) => true,
        py::ExprRef::ListComp(_) => true,
        py::ExprRef::Attribute(_) => true,
        py::ExprRef::Call(_) => true,
        py::ExprRef::Subscript(_) => true,
        _ => false,
    }
}

fn single_string_token(range: TextRange, tokens: &Tokens) -> bool {
    tokens
        .in_range(range)
        .iter()
        .filter(|token| token.kind() == TokenKind::String)
        .count()
        == 1
}

struct Validator<'a> {
    tokens: &'a Tokens,
    errors: &'a mut dyn FnMut(SyntaxError),
    loads: Vec<TextRange>,
    excluded: Vec<NodeIndex>,
    statement_node: Option<NodeIndex>,
}

impl Validator<'_> {
    fn error(&mut self, range: TextRange, message: impl Into<String>) {
        let Self {
            tokens: _,
            errors,
            loads: _,
            excluded: _,
            statement_node: _,
        } = self;
        errors(SyntaxError {
            message: message.into(),
            range: crate::TextRange::new(
                u32::from(range.start()).into(),
                u32::from(range.end()).into(),
            ),
        });
    }

    fn unsupported(&mut self, range: TextRange) {
        self.error(range, "This syntax is not supported in Starlark");
    }

    fn load(&mut self, call: &py::ExprCall) {
        let Self {
            tokens,
            errors: _,
            loads: _,
            excluded: _,
            statement_node: _,
        } = self;
        let tokens = *tokens;
        let py::ExprCall {
            node_index: _,
            range_start: _,
            func: _,
            arguments,
        } = call;
        let mut valid = true;
        let mut count = 0;
        for (index, argument) in arguments.iter_source_order().enumerate() {
            count += 1;
            let value = argument.value();
            let literal = matches!(tokens.in_range(value.range()), [token] if token.kind() == TokenKind::String)
                && parentheses_iterator(value.into(), Some(arguments.into()), tokens)
                    .next()
                    .is_none();
            if !matches!(value, Expr::StringLiteral(_)) || !literal {
                self.error(value.range(), "Expected a string in load statement");
                valid = false;
            }
            if let ArgOrKeyword::Keyword(keyword) = argument {
                let py::Keyword {
                    node_index: _,
                    range,
                    arg,
                    value: _,
                } = keyword;
                if index == 0 || arg.is_none() {
                    self.error(*range, "Expected a module string followed by load items");
                    valid = false;
                }
                if let Some(name) = arg {
                    self.visit_identifier(name);
                }
            }
        }
        if count == 0 {
            self.error(call.range(), "Expected module name");
        } else if valid {
            self.loads.push(call.range());
        }
    }

    fn statement(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::FunctionDef(def) => {
                let py::StmtFunctionDef {
                    node_index: _,
                    range,
                    is_async,
                    decorator_list,
                    name,
                    type_params,
                    parameters,
                    returns,
                    body,
                } = def;
                if *is_async
                    || !decorator_list.is_empty()
                    || type_params.is_some()
                    || returns.is_some()
                {
                    self.error(
                        *range,
                        "Function annotations and decorators are not supported in Starlark",
                    );
                }
                if *is_async
                    || !decorator_list.is_empty()
                    || type_params.is_some()
                    || returns.is_some()
                    || !parameters.posonlyargs.is_empty()
                    || parameters
                        .iter()
                        .any(|parameter| parameter.as_parameter().annotation.is_some())
                    || name.as_str() == "load"
                {
                    self.excluded
                        .push(self.statement_node.expect("visiting a statement"));
                }
                self.visit_identifier(name);
                self.visit_parameters(parameters);
                self.visit_body(body);
            }
            Stmt::For(stmt) => {
                let py::StmtFor {
                    node_index: _,
                    range,
                    is_async,
                    target,
                    iter,
                    body,
                    orelse,
                } = stmt;
                if *is_async || !orelse.is_empty() {
                    self.excluded
                        .push(self.statement_node.expect("visiting a statement"));
                    self.error(
                        *range,
                        "Async loops and for-else are not supported in Starlark",
                    );
                }
                self.visit_expr(target);
                self.visit_expr(iter);
                self.visit_body(body);
            }
            Stmt::Assign(stmt) => {
                let py::StmtAssign {
                    node_index: _,
                    range,
                    targets,
                    value,
                } = stmt;
                if targets.len() != 1 {
                    self.excluded
                        .push(self.statement_node.expect("visiting a statement"));
                    self.error(*range, "Chained assignment is not supported in Starlark");
                }
                for target in targets {
                    self.visit_expr(target);
                }
                self.visit_expr(value);
            }
            Stmt::AugAssign(stmt) => {
                let py::StmtAugAssign {
                    node_index: _,
                    range,
                    target,
                    op,
                    value,
                } = stmt;
                if matches!(op, py::Operator::Pow | py::Operator::MatMult) {
                    self.excluded
                        .push(self.statement_node.expect("visiting a statement"));
                    self.error(*range, "Unsupported Starlark assignment operator");
                }
                self.visit_expr(target);
                self.visit_expr(value);
            }
            Stmt::Expr(stmt) => {
                let py::StmtExpr {
                    node_index: _,
                    range,
                    value,
                } = stmt;
                if let Expr::Call(call) = value.as_ref() {
                    let py::ExprCall {
                        node_index: _,
                        range_start: _,
                        func,
                        arguments: _,
                    } = call;
                    if let Expr::Name(py::ExprName {
                        node_index: _,
                        range: _,
                        id,
                        ctx: _,
                    }) = func.as_ref()
                    {
                        if id == "load" {
                            if parentheses_iterator(
                                value.as_ref().into(),
                                Some(stmt.into()),
                                self.tokens,
                            )
                            .next()
                            .is_some()
                                || parentheses_iterator(
                                    func.as_ref().into(),
                                    Some(call.into()),
                                    self.tokens,
                                )
                                .next()
                                .is_some()
                            {
                                self.error(*range, "load must be a bare statement");
                            }
                            self.load(call);
                            return;
                        }
                    }
                }
                self.visit_expr(value);
            }
            Stmt::If(_) => source_order::walk_stmt(self, stmt),
            Stmt::Return(_) => source_order::walk_stmt(self, stmt),
            Stmt::Break(_) => {}
            Stmt::Continue(_) => {}
            Stmt::Pass(_) => {}
            _ => {
                self.excluded.push(stmt.node_index().load());
                self.unsupported(stmt.range());
            }
        }
    }

    fn expression(&mut self, expr: &Expr) {
        if !supports_expr(expr.into(), self.tokens) {
            self.excluded.push(
                self.statement_node
                    .expect("expressions belong to a statement"),
            );
            self.unsupported(expr.range());
            return;
        }
        let unsupported_owner = match expr {
            Expr::Dict(dict) => dict.items.iter().any(|item| item.key.is_none()),
            Expr::ListComp(comp) => comp.generators.iter().any(|generator| generator.is_async),
            Expr::DictComp(comp) => comp.generators.iter().any(|generator| generator.is_async),
            Expr::List(list) => list.elts.iter().any(Expr::is_starred_expr),
            Expr::Tuple(tuple) => tuple.elts.iter().any(Expr::is_starred_expr),
            Expr::Lambda(lambda) => lambda
                .parameters
                .as_ref()
                .is_some_and(|parameters| !parameters.posonlyargs.is_empty()),
            _ => false,
        };
        if unsupported_owner {
            self.excluded.push(
                self.statement_node
                    .expect("expressions belong to a statement"),
            );
        }
        match expr {
            Expr::Dict(py::ExprDict {
                node_index: _,
                range: _,
                items,
            }) => {
                for py::DictItem { key, value } in items {
                    match key {
                        Some(key) => {
                            self.visit_expr(key);
                            self.visit_expr(value);
                        }
                        None => self.unsupported(value.range()),
                    }
                }
            }
            Expr::Subscript(py::ExprSubscript {
                node_index: _,
                range: _,
                value,
                slice,
                ctx: _,
            }) => {
                self.visit_expr(value);
                match slice.as_ref() {
                    Expr::Slice(py::ExprSlice {
                        node_index: _,
                        range: _,
                        lower,
                        upper,
                        step,
                    }) => {
                        for expr in [lower, upper, step].into_iter().flatten() {
                            self.visit_expr(expr);
                        }
                    }
                    expr => self.visit_expr(expr),
                }
            }
            _ => source_order::walk_expr(self, expr),
        }
    }
}

impl<'a> SourceOrderVisitor<'a> for Validator<'_> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        let previous = self.statement_node.replace(stmt.node_index().load());
        stacker::maybe_grow(32 * 1024, 1024 * 1024, || self.statement(stmt));
        self.statement_node = previous;
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        stacker::maybe_grow(32 * 1024, 1024 * 1024, || self.expression(expr));
    }

    fn visit_parameters(&mut self, parameters: &'a py::Parameters) {
        let py::Parameters {
            node_index: _,
            range,
            posonlyargs,
            args: _,
            vararg: _,
            kwonlyargs: _,
            kwarg: _,
        } = parameters;
        if !posonlyargs.is_empty() {
            self.error(
                *range,
                "Positional-only parameters are not supported in Starlark",
            );
        }
        source_order::walk_parameters(self, parameters);
    }

    fn visit_parameter(&mut self, parameter: &'a py::Parameter) {
        let py::Parameter {
            node_index: _,
            range: _,
            name,
            annotation,
        } = parameter;
        if let Some(annotation) = annotation {
            self.error(
                annotation.range(),
                "Type annotations are not supported in Starlark",
            );
        }
        self.visit_identifier(name);
    }

    fn visit_identifier(&mut self, identifier: &'a py::Identifier) {
        if identifier.as_str() == "load" {
            self.error(identifier.range(), "load is a reserved word");
        }
    }

    fn visit_arguments(&mut self, arguments: &'a py::Arguments) {
        for argument in arguments.iter_source_order() {
            match argument {
                ArgOrKeyword::Arg(expr) => match expr {
                    Expr::Starred(py::ExprStarred {
                        node_index: _,
                        range: _,
                        value,
                        ctx: _,
                    }) => self.visit_expr(value),
                    expr => self.visit_expr(expr),
                },
                ArgOrKeyword::Keyword(keyword) => self.visit_keyword(keyword),
            }
        }
    }

    fn visit_comprehension(&mut self, comprehension: &'a py::Comprehension) {
        let py::Comprehension {
            node_index: _,
            range,
            target: _,
            iter: _,
            ifs: _,
            is_async,
        } = comprehension;
        if *is_async {
            self.error(*range, "Async comprehensions are not supported in Starlark");
        }
        source_order::walk_comprehension(self, comprehension);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn recovered_suites_are_validated_without_a_colon() {
        let source = "if x\n    obj.load\n    load(\":defs.bzl\", alias=\"first\", \"second\")\n";
        let parsed = ruff_python_parser::parse_unchecked_source(
            source,
            ruff_python_ast::PySourceType::Python,
        );
        assert!(parsed.errors().iter().any(|error| error.error
            == ruff_python_parser::ParseErrorType::PositionalAfterKeywordArgument));
        let mut errors = Vec::new();
        super::validate(source, &parsed, &mut |error| errors.push(error.message));
        assert!(
            errors
                .iter()
                .any(|error| error == "load is a reserved word"),
            "{errors:?}"
        );
        assert!(
            !errors.contains(
                &ruff_python_parser::ParseErrorType::PositionalAfterKeywordArgument.to_string()
            ),
            "{errors:?}"
        );
    }

    #[test]
    fn clause_diagnostics_follow_source_order() {
        let source = "if x: pass\nelif y: obj.load\nelif z: other.load\nelse: more.load\n";
        let parsed = ruff_python_parser::parse_unchecked_source(
            source,
            ruff_python_ast::PySourceType::Python,
        );
        let mut errors = Vec::new();
        super::validate(source, &parsed, &mut |error| errors.push(error));
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(
            errors
                .iter()
                .all(|error| error.message == "load is a reserved word"),
            "{errors:?}"
        );
        assert!(
            errors
                .windows(2)
                .all(|pair| pair[0].range.start() < pair[1].range.start()),
            "{errors:?}"
        );
    }

    #[test]
    fn unsupported_subtrees_are_not_validated_as_starlark() {
        for (source, expected) in [
            (
                "class C:\n    x = 2 ** 3\n",
                vec!["This syntax is not supported in Starlark"],
            ),
            (
                "def f(x: 2 ** 3) -> 4 ** 5: pass\n",
                vec![
                    "Function annotations and decorators are not supported in Starlark",
                    "Type annotations are not supported in Starlark",
                ],
            ),
            (
                "for x in xs:\n    pass\nelse:\n    x = load\n",
                vec!["Async loops and for-else are not supported in Starlark"],
            ),
            (
                "x = {**(2 ** 3)}\n",
                vec!["This syntax is not supported in Starlark"],
            ),
        ] {
            let parsed = ruff_python_parser::parse_unchecked_source(
                source,
                ruff_python_ast::PySourceType::Python,
            );
            let mut errors = Vec::new();
            super::validate(source, &parsed, &mut |error| errors.push(error.message));
            assert_eq!(errors, expected, "{source}");
        }
    }
}

#[cfg(test)]
mod dialect_tests {
    fn validate_source(source: &str, errors: &mut dyn FnMut(crate::SyntaxError)) {
        let parsed = ruff_python_parser::parse_unchecked_source(
            source,
            ruff_python_ast::PySourceType::Python,
        );
        super::validate(source, &parsed, errors);
    }

    #[test]
    fn assignment_targets_are_checked_without_semantic_scopes() {
        for source in [
            "1 = value\n",
            "f() = value\n",
            "(a, 1) = value\n",
            "for 1 in values:\n    pass\n",
            "[x for 1 in values]\n",
            "a, *rest = values\n",
        ] {
            let mut errors = Vec::new();
            validate_source(source, &mut |error| errors.push(error));
            assert!(!errors.is_empty(), "{source}");
        }
        for source in [
            "a = value\n",
            "(a, [b, c]) = values\n",
            "obj.field = value\n",
            "items[0] = value\n",
            "items[:] = values\n",
            "for a, b in values:\n    pass\n",
        ] {
            validate_source(source, &mut |error| panic!("{source}: {error:?}"));
        }
    }

    #[test]
    fn starlark_load_grammar() {
        let source = "load(\":defs.bzl\", alias=\"name\", \"other\")\n";
        let mut errors = Vec::new();
        validate_source(source, &mut |error| errors.push(error));
        assert_eq!(errors, []);
        for source in [
            "f(alias=1, 2)",
            "load(module=\":defs.bzl\")",
            "load(\":defs.bzl\", **\"name\")",
            "load((\":defs.bzl\"), \"name\")",
            "load(\":defs\" \".bzl\", \"name\")",
            "load(\":defs.bzl\", load=\"name\")",
            "load(\":defs.bzl\", (alias)=\"name\")",
            "f(load=1)",
            "f((name)=1)",
            "(load)(\":defs.bzl\", \"name\")",
            "(load(\":defs.bzl\", \"name\"))",
        ] {
            let mut errors = Vec::new();
            validate_source(source, &mut |error| errors.push(error));
            assert!(!errors.is_empty(), "{source}");
        }
    }

    #[test]
    fn reject_python_literal_extensions() {
        for source in [
            "x = 'a' 'b'",
            "x = b'a' b'b'",
            "x = 1_000",
            "x = 0b10",
            "x = u'x'",
            "x = R'x'",
            "x = B'x'",
            "x = BR'x'",
            "é = 1",
            r#"x = "\z""#,
            r#"x = "\xff""#,
            r#"x = "\377""#,
            "x = load",
            "load = 1",
            "obj.load",
            "def load(): pass",
        ] {
            let mut errors = Vec::new();
            validate_source(source, &mut |error| errors.push(error));
            assert!(!errors.is_empty(), "{source}");
        }
    }

    #[test]
    fn starlark_literal_values() {
        for (source, expected) in [(r#"x = r"a\"b""#, "a\"b"), (r#"x = "\x7f""#, "\x7f")] {
            let mut errors = Vec::new();
            validate_source(source, &mut |error| errors.push(error));
            assert_eq!(errors, [], "{source}");
            assert_eq!(
                crate::source::string_value(&source[4..])
                    .unwrap()
                    .0
                    .as_ref(),
                expected
            );
        }
        let mut errors = Vec::new();
        validate_source("x = b'é'", &mut |error| errors.push(error));
        assert_eq!(errors, []);
    }

    #[test]
    fn nested_ruff_expressions() {
        for separator in [" + ", "."] {
            let source = std::iter::repeat_n("x", 10_000)
                .collect::<Vec<_>>()
                .join(separator);
            // Ruff's recursive AST drop also needs the larger stack.
            stacker::grow(16 * 1024 * 1024, || {
                validate_source(&source, &mut |error| panic!("{error:?}"));
            });
        }
    }

    #[test]
    fn indentation_requires_spaces() {
        let mut errors = Vec::new();
        validate_source("if x:\n\tpass\n", &mut |error| errors.push(error));
        assert!(errors
            .iter()
            .any(|error| error.message == "Starlark indentation must use spaces"));
        for source in ["x = (\n\t1\n)\n", "x = '''\n\ttext\n'''\n"] {
            validate_source(source, &mut |error| panic!("{source}: {error:?}"));
        }
    }
}
