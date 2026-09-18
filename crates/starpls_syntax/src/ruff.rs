//! Translate Ruff's recovered syntax into the lossless tree used by the IDE.
//!
//! Ruff owns grammatical structure. Starpls owns the Starlark subset, `load`,
//! type comments, and the CST shape expected by its language services.

use rowan::GreenNode;
use rowan::GreenNodeBuilder;
use rowan::Language;
use ruff_python_ast as py;
use ruff_python_ast::token::parentheses_iterator;
use ruff_python_ast::token::Token;
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::token::Tokens;
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::ArgOrKeyword;
use ruff_python_ast::Expr;
use ruff_python_ast::Stmt;
use ruff_python_parser::Parsed;
use ruff_text_size::Ranged;
use ruff_text_size::TextRange;
use ruff_text_size::TextSize;

use crate::parser::build_type_comment;
use crate::StarlarkLanguage;
use crate::SyntaxError;
use crate::SyntaxKind;
use crate::SyntaxKind::*;

struct Node {
    kind: SyntaxKind,
    range: TextRange,
    children: Vec<Node>,
}

impl Node {
    fn new(kind: SyntaxKind, range: TextRange, children: Vec<Node>) -> Self {
        let range = children
            .iter()
            .fold(range, |range, child| range.cover(child.range));
        Self {
            kind,
            range,
            children,
        }
    }

    fn leaf(kind: SyntaxKind, range: TextRange) -> Self {
        Self::new(kind, range, Vec::new())
    }
}

pub(super) fn parse(
    source: &str,
    parsed: &Parsed<py::ModModule>,
    errors: &mut dyn FnMut(SyntaxError),
) -> GreenNode {
    crate::validation::validate(source, parsed, errors);
    let mut adapter = Adapter {
        tokens: parsed.tokens(),
    };
    let children = parsed
        .syntax()
        .body
        .iter()
        .map(|stmt| adapter.statement(stmt))
        .collect();
    let root = Node::new(MODULE, TextRange::up_to(TextSize::of(source)), children);
    let mut builder = GreenNodeBuilder::new();
    let mut writer = Writer {
        source,
        tokens: parsed.tokens().iter().copied().peekable(),
        offset: TextSize::new(0),
        builder: &mut builder,
        errors,
    };
    writer.node(root);
    builder.finish()
}

struct Adapter<'a> {
    tokens: &'a Tokens,
}

impl Adapter<'_> {
    fn unsupported(&self, range: TextRange) -> Node {
        Node::leaf(ERROR, range)
    }

    fn statement(&mut self, stmt: &Stmt) -> Node {
        stacker::maybe_grow(32 * 1024, 1024 * 1024, || self.statement_inner(stmt))
    }

    fn statement_inner(&mut self, stmt: &Stmt) -> Node {
        let parent = AnyNodeRef::from(stmt);
        match stmt {
            Stmt::FunctionDef(def) => {
                let py::StmtFunctionDef {
                    node_index: _,
                    range,
                    is_async: _,
                    decorator_list: _,
                    name,
                    type_params: _,
                    parameters,
                    returns: _,
                    body,
                } = def;
                let name = Node::leaf(NAME, name.range());
                let params = self.parameters(parameters);
                let suite = self.suite(body, parameters.end(), range.end(), None);
                Node::new(DEF_STMT, *range, vec![name, params, suite])
            }
            Stmt::If(stmt) => {
                let py::StmtIf {
                    node_index: _,
                    range,
                    test,
                    body,
                    elif_else_clauses,
                } = stmt;
                let test_node = self.expr(test, parent);
                let next_clause = elif_else_clauses.first().map(Ranged::start);
                let suite = self.suite(body, test.end(), range.end(), next_clause);
                let mut children = vec![test_node, suite];
                if let Some(clause) = self.clauses(elif_else_clauses) {
                    children.push(clause);
                }
                Node::new(IF_STMT, *range, children)
            }
            Stmt::For(stmt) => {
                let py::StmtFor {
                    node_index: _,
                    range,
                    is_async: _,
                    target,
                    iter,
                    body,
                    orelse: _,
                } = stmt;
                let target = self.loop_variables(target, parent);
                let iterable = self.expr(iter, parent);
                let suite = self.suite(body, iter.end(), range.end(), None);
                Node::new(FOR_STMT, *range, vec![target, iterable, suite])
            }
            Stmt::Assign(stmt) => {
                let py::StmtAssign {
                    node_index: _,
                    range,
                    targets,
                    value,
                } = stmt;
                let mut children = targets
                    .iter()
                    .map(|target| self.expr(target, parent))
                    .collect::<Vec<_>>();
                children.push(self.expr(value, parent));
                Node::new(ASSIGN_STMT, *range, children)
            }
            Stmt::AugAssign(stmt) => {
                let py::StmtAugAssign {
                    node_index: _,
                    range,
                    target,
                    op: _,
                    value,
                } = stmt;
                let target = self.expr(target, parent);
                let value = self.expr(value, parent);
                Node::new(ASSIGN_STMT, *range, vec![target, value])
            }
            Stmt::Return(stmt) => {
                let py::StmtReturn {
                    node_index: _,
                    range,
                    value,
                } = stmt;
                let children = value.iter().map(|value| self.expr(value, parent)).collect();
                Node::new(RETURN_STMT, *range, children)
            }
            Stmt::Break(stmt) => Node::leaf(BREAK_STMT, stmt.range()),
            Stmt::Continue(stmt) => Node::leaf(CONTINUE_STMT, stmt.range()),
            Stmt::Pass(stmt) => Node::leaf(PASS_STMT, stmt.range()),
            Stmt::Expr(stmt) => {
                let py::StmtExpr {
                    node_index: _,
                    range: _,
                    value,
                } = stmt;
                if let Expr::Call(call) = value.as_ref() {
                    if let Expr::Name(name) = call.func.as_ref() {
                        if name.id == "load" {
                            return self.load(call);
                        }
                    }
                }
                self.expr(value, parent)
            }
            _ => self.unsupported(stmt.range()),
        }
    }

    fn clauses(&mut self, clauses: &[py::ElifElseClause]) -> Option<Node> {
        let mut tail: Option<Node> = None;
        for clause in clauses.iter().rev() {
            let py::ElifElseClause {
                node_index: _,
                range,
                test,
                body,
            } = clause;
            let parent = AnyNodeRef::from(clause);
            tail = Some(match test {
                Some(test) => {
                    let expr = self.expr(test, parent);
                    let next_clause = tail.as_ref().map(|tail| tail.range.start());
                    let suite = self.suite(body, test.end(), range.end(), next_clause);
                    let mut children = vec![expr, suite];
                    children.extend(tail);
                    Node::new(IF_STMT, *range, children)
                }
                None => self.suite(body, range.start(), range.end(), None),
            });
        }
        tail
    }

    fn suite(
        &mut self,
        body: &[Stmt],
        after: TextSize,
        fallback_end: TextSize,
        next_clause: Option<TextSize>,
    ) -> Node {
        let fallback_end = next_clause.map_or(fallback_end, |next| fallback_end.min(next));
        let header_end = body
            .first()
            .map_or(fallback_end, Ranged::start)
            .min(fallback_end)
            .max(after);
        let header = self.tokens.in_range(TextRange::new(after, header_end));
        let Some(colon) = header.iter().find(|token| token.kind() == TokenKind::Colon) else {
            return Node::leaf(SUITE, TextRange::empty(after));
        };
        let start = colon.end();
        let Some(first) = body.first() else {
            return Node::leaf(SUITE, TextRange::new(start, fallback_end.max(start)));
        };
        let mut depth = 0;
        let mut end = fallback_end;
        for token in self.tokens.after(start) {
            let boundary = match token.kind() {
                TokenKind::Indent => {
                    depth += 1;
                    None
                }
                TokenKind::Dedent => {
                    depth -= 1;
                    (depth == 0).then_some(token.start())
                }
                TokenKind::Newline => {
                    (depth == 0 && first.start() < token.start()).then_some(token.end())
                }
                _ => None,
            };
            if let Some(boundary) = boundary {
                // Recovery may omit a dedent. Never consume a later clause.
                end = next_clause.map_or(boundary, |next| boundary.min(next));
                break;
            }
        }
        let children = body.iter().map(|stmt| self.statement(stmt)).collect();
        Node::new(SUITE, TextRange::new(start, end.max(start)), children)
    }

    fn loop_variables(&mut self, target: &Expr, parent: AnyNodeRef<'_>) -> Node {
        let children = match target {
            Expr::Tuple(tuple) => {
                if tuple.parenthesized {
                    vec![self.expr(target, parent)]
                } else {
                    tuple
                        .elts
                        .iter()
                        .map(|expr| self.expr(expr, target.into()))
                        .collect()
                }
            }
            _ => vec![self.expr(target, parent)],
        };
        Node::new(LOOP_VARIABLES, target.range(), children)
    }

    fn load(&mut self, call: &py::ExprCall) -> Node {
        let mut children = Vec::new();
        for (index, argument) in call.arguments.iter_source_order().enumerate() {
            let node = match argument {
                ArgOrKeyword::Arg(expr) => Node::leaf(
                    if index == 0 {
                        LOAD_MODULE
                    } else {
                        DIRECT_LOAD_ITEM
                    },
                    expr.range(),
                ),
                ArgOrKeyword::Keyword(keyword) => {
                    let names = keyword.arg.iter().map(|name| self.name(name)).collect();
                    Node::new(ALIASED_LOAD_ITEM, keyword.range(), names)
                }
            };
            children.push(node);
        }
        Node::new(LOAD_STMT, call.range(), children)
    }

    fn parameters(&mut self, parameters: &py::Parameters) -> Node {
        let py::Parameters {
            node_index: _,
            range,
            posonlyargs: _,
            args,
            vararg,
            kwonlyargs,
            kwarg,
        } = parameters;
        let mut children = Vec::new();
        for parameter in parameters.iter_source_order() {
            let node = match parameter {
                py::AnyParameterRef::NonVariadic(param) => {
                    let py::ParameterWithDefault {
                        range,
                        node_index: _,
                        parameter,
                        default,
                    } = param;
                    let mut children = vec![self.parameter_name(parameter)];
                    if let Some(default) = default {
                        children.push(self.expr(default, param.into()));
                    }
                    Node::new(SIMPLE_PARAMETER, *range, children)
                }
                py::AnyParameterRef::Variadic(param) => {
                    let is_kwargs = kwarg
                        .as_deref()
                        .is_some_and(|kwargs| kwargs.range() == param.range());
                    let marker = if is_kwargs {
                        TokenKind::DoubleStar
                    } else {
                        TokenKind::Star
                    };
                    let start = self
                        .tokens
                        .before(param.name.start())
                        .iter()
                        .rev()
                        .find(|token| token.kind() == marker)
                        .map_or(param.start(), Ranged::start);
                    let name = self.parameter_name(param);
                    Node::new(
                        if is_kwargs {
                            KWARGS_DICT_PARAMETER
                        } else {
                            ARGS_LIST_PARAMETER
                        },
                        TextRange::new(start, param.end()),
                        vec![name],
                    )
                }
            };
            children.push(node);
        }
        if vararg.is_none() && !kwonlyargs.is_empty() {
            let start = args.last().map_or(range.start(), Ranged::end);
            let end = kwonlyargs[0].start();
            if let Some(star) = self
                .tokens
                .in_range(TextRange::new(start, end))
                .iter()
                .find(|token| token.kind() == TokenKind::Star)
            {
                children.push(Node::leaf(ARGS_LIST_PARAMETER, star.range()));
            }
        }
        children.sort_by_key(|child| child.range.start());
        Node::new(PARAMETERS, *range, children)
    }

    fn parameter_name(&mut self, param: &py::Parameter) -> Node {
        let py::Parameter {
            range: _,
            node_index: _,
            name,
            annotation: _,
        } = param;
        self.name(name)
    }

    fn name(&mut self, name: &py::Identifier) -> Node {
        Node::leaf(NAME, name.range())
    }

    fn expr(&mut self, expr: &Expr, parent: AnyNodeRef<'_>) -> Node {
        stacker::maybe_grow(32 * 1024, 1024 * 1024, || self.expr_inner(expr, parent))
    }

    fn expr_inner(&mut self, expr: &Expr, parent: AnyNodeRef<'_>) -> Node {
        let mut node = self.bare_expr(expr);
        for range in parentheses_iterator(expr.into(), Some(parent), self.tokens) {
            if range.start() < parent.start() {
                break;
            }
            node = Node::new(PAREN_EXPR, range, vec![node]);
        }
        node
    }

    fn bare_expr(&mut self, expr: &Expr) -> Node {
        let parent = AnyNodeRef::from(expr);
        let range = expr.range();
        if !crate::validation::supports_expr(expr, self.tokens) {
            return self.unsupported(range);
        }
        match expr {
            Expr::Name(name) => {
                Node::leaf(if name.id.is_empty() { ERROR } else { NAME_REF }, range)
            }
            Expr::NumberLiteral(_) => Node::leaf(LITERAL_EXPR, range),
            Expr::BooleanLiteral(_) | Expr::NoneLiteral(_) => Node::leaf(LITERAL_EXPR, range),
            Expr::StringLiteral(_) | Expr::BytesLiteral(_) => Node::leaf(LITERAL_EXPR, range),
            Expr::BinOp(expr) => {
                let py::ExprBinOp {
                    node_index: _,
                    range,
                    left,
                    op: _,
                    right,
                } = expr;
                let left = self.expr(left, parent);
                let right = self.expr(right, parent);
                Node::new(BINARY_EXPR, *range, vec![left, right])
            }
            Expr::BoolOp(expr) => self.binary_chain(expr.values.iter(), parent, range),
            Expr::Compare(expr) => {
                let py::ExprCompare {
                    node_index: _,
                    range,
                    operands,
                    ops: _,
                } = expr;
                self.binary_chain(operands, parent, *range)
            }
            Expr::UnaryOp(expr) => {
                let py::ExprUnaryOp {
                    node_index: _,
                    range,
                    op: _,
                    operand,
                } = expr;
                let operand = self.expr(operand, parent);
                Node::new(UNARY_EXPR, *range, vec![operand])
            }
            Expr::If(expr) => {
                let py::ExprIf {
                    node_index: _,
                    range,
                    test,
                    body,
                    orelse,
                } = expr;
                let body = self.expr(body, parent);
                let test = self.expr(test, parent);
                let orelse = self.expr(orelse, parent);
                Node::new(IF_EXPR, *range, vec![body, test, orelse])
            }
            Expr::Lambda(expr) => {
                let py::ExprLambda {
                    node_index: _,
                    range,
                    parameters,
                    body,
                } = expr;
                let mut children = Vec::new();
                if let Some(parameters) = parameters {
                    children.push(self.parameters(parameters));
                }
                children.push(self.expr(body, parent));
                Node::new(LAMBDA_EXPR, *range, children)
            }
            Expr::List(expr) => {
                let children = expr
                    .elts
                    .iter()
                    .map(|expr| self.expr(expr, parent))
                    .collect();
                Node::new(LIST_EXPR, range, children)
            }
            Expr::Tuple(expr) => {
                let children = expr
                    .elts
                    .iter()
                    .map(|expr| self.expr(expr, parent))
                    .collect();
                Node::new(TUPLE_EXPR, range, children)
            }
            Expr::Dict(expr) => {
                let children = expr
                    .items
                    .iter()
                    .map(|item| {
                        let py::DictItem { key, value } = item;
                        match key {
                            Some(key) => {
                                let key = self.expr(key, parent);
                                let value = self.expr(value, parent);
                                Node::new(
                                    DICT_ENTRY,
                                    key.range.cover(value.range),
                                    vec![key, value],
                                )
                            }
                            None => self.unsupported(value.range()),
                        }
                    })
                    .collect();
                Node::new(DICT_EXPR, range, children)
            }
            Expr::ListComp(expr) => {
                let py::ExprListComp {
                    node_index: _,
                    range,
                    elt,
                    generators,
                } = expr;
                let mut children = vec![self.expr(elt, parent)];
                self.comprehensions(generators, &mut children);
                Node::new(LIST_COMP, *range, children)
            }
            Expr::DictComp(expr) => {
                let py::ExprDictComp {
                    node_index: _,
                    range,
                    key,
                    value,
                    generators,
                } = expr;
                let Some(key) = key else {
                    return self.unsupported(*range);
                };
                let key = self.expr(key, parent);
                let value = self.expr(value, parent);
                let mut children = vec![Node::new(
                    DICT_ENTRY,
                    key.range.cover(value.range),
                    vec![key, value],
                )];
                self.comprehensions(generators, &mut children);
                Node::new(DICT_COMP, *range, children)
            }
            Expr::Attribute(expr) => {
                let py::ExprAttribute {
                    node_index: _,
                    range,
                    value,
                    attr,
                    ctx: _,
                } = expr;
                let value = self.expr(value, parent);
                let name = Node::leaf(NAME, attr.range());
                Node::new(DOT_EXPR, *range, vec![value, name])
            }
            Expr::Call(expr) => {
                let py::ExprCall {
                    node_index: _,
                    range_start: _,
                    func,
                    arguments,
                } = expr;
                let func = self.expr(func, parent);
                let mut children = Vec::new();
                for arg in arguments.iter_source_order() {
                    let node = match arg {
                        ArgOrKeyword::Arg(arg) => match arg {
                            Expr::Starred(arg) => {
                                let value = self.expr(&arg.value, arg.into());
                                Node::new(UNPACKED_LIST_ARGUMENT, arg.range(), vec![value])
                            }
                            arg => {
                                let value = self.expr(arg, arguments.into());
                                Node::new(SIMPLE_ARGUMENT, value.range, vec![value])
                            }
                        },
                        ArgOrKeyword::Keyword(arg) => {
                            let py::Keyword {
                                node_index: _,
                                range,
                                arg: name,
                                value,
                            } = arg;
                            let mut children = Vec::new();
                            if let Some(name) = name {
                                children.push(self.name(name));
                            }
                            children.push(self.expr(value, arguments.into()));
                            Node::new(
                                if name.is_some() {
                                    KEYWORD_ARGUMENT
                                } else {
                                    UNPACKED_DICT_ARGUMENT
                                },
                                *range,
                                children,
                            )
                        }
                    };
                    children.push(node);
                }
                let arguments = Node::new(ARGUMENTS, arguments.range(), children);
                Node::new(CALL_EXPR, range, vec![func, arguments])
            }
            Expr::Subscript(expr) => {
                let py::ExprSubscript {
                    node_index: _,
                    range,
                    value,
                    slice,
                    ctx: _,
                } = expr;
                let mut children = vec![self.expr(value, parent)];
                let kind = match slice.as_ref() {
                    Expr::Slice(slice) => {
                        let py::ExprSlice {
                            node_index: _,
                            range: _,
                            lower,
                            upper,
                            step,
                        } = slice;
                        for expr in [lower, upper, step].into_iter().flatten() {
                            children.push(self.expr(expr, parent));
                        }
                        SLICE_EXPR
                    }
                    expr => {
                        children.push(self.expr(expr, parent));
                        INDEX_EXPR
                    }
                };
                Node::new(kind, *range, children)
            }
            _ => self.unsupported(range),
        }
    }

    fn binary_chain<'a>(
        &mut self,
        values: impl IntoIterator<Item = &'a Expr>,
        parent: AnyNodeRef<'_>,
        range: TextRange,
    ) -> Node {
        let mut values = values.into_iter();
        let Some(first) = values.next() else {
            return Node::leaf(ERROR, range);
        };
        let mut node = self.expr(first, parent);
        for value in values {
            let right = self.expr(value, parent);
            node = Node::new(
                BINARY_EXPR,
                node.range.cover(right.range),
                vec![node, right],
            );
        }
        node
    }

    fn comprehensions(&mut self, generators: &[py::Comprehension], children: &mut Vec<Node>) {
        for generator in generators {
            let py::Comprehension {
                node_index: _,
                range,
                target,
                iter,
                ifs,
                is_async: _,
            } = generator;
            let parent = generator.into();
            let target = self.loop_variables(target, parent);
            let iterable = self.expr(iter, parent);
            let end = iterable.range.end();
            children.push(Node::new(
                COMP_CLAUSE_FOR,
                TextRange::new(range.start(), end),
                vec![target, iterable],
            ));
            for test in ifs {
                let start = self
                    .tokens
                    .before(test.start())
                    .iter()
                    .rev()
                    .find(|token| token.kind() == TokenKind::If)
                    .map_or(test.start(), Ranged::start);
                let test = self.expr(test, parent);
                children.push(Node::new(
                    COMP_CLAUSE_IF,
                    TextRange::new(start, test.range.end()),
                    vec![test],
                ));
            }
        }
    }
}

struct Writer<'a, 'b> {
    source: &'a str,
    tokens: std::iter::Peekable<std::iter::Copied<std::slice::Iter<'a, Token>>>,
    offset: TextSize,
    builder: &'b mut GreenNodeBuilder<'static>,
    errors: &'b mut dyn FnMut(SyntaxError),
}

impl Writer<'_, '_> {
    fn node(&mut self, node: Node) {
        enum Event {
            Enter(Node),
            Exit(TextSize),
        }
        // Boolean and comparison chains are flat in Ruff's AST and nested in
        // the CST. Traverse our nodes iteratively so such chains do not use
        // one stack frame per operand (including when dropping the nodes).
        let mut events = vec![Event::Enter(node)];
        while let Some(event) = events.pop() {
            match event {
                Event::Enter(node) => {
                    let Node {
                        kind,
                        range,
                        children,
                    } = node;
                    assert!(
                        range.start() >= self.offset,
                        "overlapping {kind:?}: {range:?}, consumed {:?}",
                        self.offset
                    );
                    self.tokens_until(range.start());
                    self.builder.start_node(StarlarkLanguage::kind_to_raw(kind));
                    events.push(Event::Exit(range.end()));
                    events.extend(children.into_iter().rev().map(Event::Enter));
                }
                Event::Exit(end) => {
                    self.tokens_until(end);
                    self.builder.finish_node();
                }
            }
        }
    }

    fn tokens_until(&mut self, end: TextSize) {
        while let Some(token) = self.tokens.peek().copied() {
            if token.start() >= end {
                break;
            }
            let _ = self.tokens.next();
            if token.range().is_empty() {
                continue;
            }
            self.gap(token.start());
            let text = &self.source[token.range()];
            let kind = token_kind(token, text);
            if kind == COMMENT && text.starts_with("# type: ") {
                build_type_comment(self.builder, text, usize::from(token.start()), self.errors);
            } else {
                self.builder
                    .token(StarlarkLanguage::kind_to_raw(kind), text);
            }
            self.offset = token.end();
        }
        self.gap(end);
    }

    fn gap(&mut self, end: TextSize) {
        if self.offset < end {
            let text = &self.source[TextRange::new(self.offset, end)];
            self.builder
                .token(StarlarkLanguage::kind_to_raw(WHITESPACE), text);
            self.offset = end;
        }
    }
}

pub(super) fn token_kind(token: Token, text: &str) -> SyntaxKind {
    use TokenKind as T;
    match token.kind() {
        T::Identifier => {
            if text == "load" {
                LOAD
            } else {
                IDENT
            }
        }
        T::Int => INT,
        T::Float => FLOAT,
        T::String => {
            if token.unwrap_string_flags().is_byte_string() {
                BYTES
            } else {
                STRING
            }
        }
        T::Comment => COMMENT,
        T::Newline => NEWLINE,
        T::NonLogicalNewline | T::Indent | T::Dedent => WHITESPACE,
        T::Lpar => OPEN_PAREN,
        T::Rpar => CLOSE_PAREN,
        T::Lsqb => OPEN_BRACK,
        T::Rsqb => CLOSE_BRACK,
        T::Lbrace => OPEN_BRACE,
        T::Rbrace => CLOSE_BRACE,
        T::Colon => COLON,
        T::Comma => COMMA,
        T::Semi => SEMI,
        T::Plus => PLUS,
        T::Minus => MINUS,
        T::Star => STAR,
        T::Slash => SLASH,
        T::Vbar => BAR,
        T::Amper => AMPERSAND,
        T::Less => LT,
        T::Greater => GT,
        T::Equal => EQ,
        T::Dot => DOT,
        T::Percent => MOD,
        T::EqEqual => EQ_EQ,
        T::NotEqual => BANG_EQ,
        T::LessEqual => LE,
        T::GreaterEqual => GE,
        T::Tilde => TILDE,
        T::CircumFlex => CARET,
        T::LeftShift => LT_LT,
        T::RightShift => GT_GT,
        T::DoubleStar => STAR_STAR,
        T::PlusEqual => PLUS_EQ,
        T::MinusEqual => MINUS_EQ,
        T::StarEqual => STAR_EQ,
        T::SlashEqual => SLASH_EQ,
        T::PercentEqual => MOD_EQ,
        T::AmperEqual => AMPERSAND_EQ,
        T::VbarEqual => BAR_EQ,
        T::CircumflexEqual => CARET_EQ,
        T::LeftShiftEqual => LT_LT_EQ,
        T::RightShiftEqual => GT_GT_EQ,
        T::DoubleSlash => SLASH_SLASH,
        T::DoubleSlashEqual => SLASH_SLASH_EQ,
        T::Rarrow => ARROW,
        T::Ellipsis => ELLIPSIS,
        T::And => AND,
        T::As => AS,
        T::Assert => ASSERT,
        T::Async => ASYNC,
        T::Await => AWAIT,
        T::Break => BREAK,
        T::Class => CLASS,
        T::Continue => CONTINUE,
        T::Def => DEF,
        T::Del => DEL,
        T::Elif => ELIF,
        T::Else => ELSE,
        T::Except => EXCEPT,
        T::False => FALSE,
        T::Finally => FINALLY,
        T::For => FOR,
        T::From => FROM,
        T::Global => GLOBAL,
        T::If => IF,
        T::Import => IMPORT,
        T::In => IN,
        T::Is => IS,
        T::Lambda => LAMBDA,
        T::None => NONE,
        T::Nonlocal => NONLOCAL,
        T::Not => NOT,
        T::Or => OR,
        T::Pass => PASS,
        T::Raise => RAISE,
        T::Return => RETURN,
        T::True => TRUE,
        T::Try => TRY,
        T::While => WHILE,
        T::With => WITH,
        T::Yield => YIELD,
        T::Case | T::Lazy | T::Match | T::Type => IDENT,
        _ => ERROR,
    }
}

#[cfg(test)]
mod tests {
    use crate::parse_module;

    #[test]
    fn preserves_source_including_incomplete_edits() {
        let check = |input: &str| {
            let tree = std::panic::catch_unwind(|| parse_module(input, &mut |_| {}))
                .unwrap_or_else(|_| panic!("could not parse {input:?}"));
            assert_eq!(
                tree.syntax().text().to_string(),
                input,
                "{:#?}",
                tree.syntax()
            );
        };
        for source in [
            "",
            "# comment\n",
            "x = ((a + b))\n",
            "f((a), key=(b))\n",
            "def f(x=1*2, *, y=0): pass\n",
            "def broken()\nx = {\"k\": 1}\ny = 2\n",
            "def f(x, *, y=1, **kwargs):\n    # type: (int, int) -> int\n    return x\n# after\n",
            "if x:\n    a = 1\nelif y:\n    a = 2\nelse:\n    a = 3\n",
            "[x for x in xs if x for y in ys if y]\n",
            "{x: y for x, y in items if y}\n",
            "load(\":defs.bzl\", alias=\"name\", \"other\")\n",
            "x = 1 # type: int\n",
            "f(\n",
            "x.\n",
            "x[\n",
            "f(x, key=\n",
            "def f(x,\n",
            "if x:\n    # comment\n",
            "x = 'unterminated\ny = 1\n",
            "x = r'\\\''\n",
            "x = 1\r\ny = '😀'\r\n",
            "if x\n    pass\ny = {\"a\": 1}\n",
            "if x:\nif y:\n    pass\nz = 1\n",
            "if x:\nelse:\n    pass\n",
            "if x\nelse:\n    pass\n",
        ] {
            check(source);
            for (offset, character) in source.char_indices() {
                check(&source[..offset]);
                let mut deleted = source.to_owned();
                deleted.replace_range(offset..offset + character.len_utf8(), "");
                check(&deleted);
            }
        }
    }
}

#[cfg(test)]
mod compatibility_tests {
    use crate::ast;
    use crate::ast::AstNode;
    use crate::parse_module;

    #[test]
    fn starlark_load_grammar() {
        let source = "load(\":defs.bzl\", alias=\"name\", \"other\")\n";
        let mut errors = Vec::new();
        let tree = parse_module(source, &mut |error| errors.push(error));
        assert_eq!(errors, []);
        let load = tree
            .syntax()
            .children()
            .find_map(ast::LoadStmt::cast)
            .unwrap();
        assert_eq!(
            load.module().unwrap().name().unwrap().text(),
            "\":defs.bzl\""
        );
        assert_eq!(load.items().count(), 2);
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
            parse_module(source, &mut |error| errors.push(error));
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
            parse_module(source, &mut |error| errors.push(error));
            assert!(!errors.is_empty(), "{source}");
        }
    }

    #[test]
    fn starlark_literal_values() {
        for (source, expected) in [(r#"x = r"a\"b""#, "a\"b"), (r#"x = "\x7f""#, "\x7f")] {
            let mut errors = Vec::new();
            let tree = parse_module(source, &mut |error| errors.push(error));
            assert_eq!(errors, [], "{source}");
            let literal = tree
                .syntax()
                .descendants()
                .find_map(ast::LiteralExpr::cast)
                .unwrap();
            let ast::LiteralKind::String(string) = literal.kind() else {
                panic!("expected string");
            };
            assert_eq!(string.value().unwrap().as_ref(), expected);
        }
        let mut errors = Vec::new();
        parse_module("x = b'é'", &mut |error| errors.push(error));
        assert_eq!(errors, []);
    }

    #[test]
    fn bare_star_after_multiplication_default() {
        let tree = parse_module("def f(x=1*2, *, y=0): pass\n", &mut |error| {
            panic!("{error:?}")
        });
        let star = tree
            .syntax()
            .descendants()
            .find_map(ast::ArgsListParameter::cast)
            .unwrap();
        assert_eq!(star.syntax().text().to_string(), "*");
        assert_eq!(u32::from(star.syntax().text_range().start()), 13);
    }

    #[test]
    fn flat_chains_do_not_require_a_deep_writer_stack() {
        let source = std::iter::repeat_n("x", 10_000)
            .collect::<Vec<_>>()
            .join(" or ");
        let tree = parse_module(&source, &mut |error| panic!("{error:?}"));
        assert_eq!(tree.syntax().text().to_string(), source);
        // Rowan 0.15 recursively drops a deeply nested green tree. Isolate
        // that existing limitation from the adapter traversal under test.
        stacker::grow(16 * 1024 * 1024, || drop(tree));
    }

    #[test]
    fn nested_ruff_expressions() {
        for separator in [" + ", "."] {
            let source = std::iter::repeat_n("x", 10_000)
                .collect::<Vec<_>>()
                .join(separator);
            let tree = parse_module(&source, &mut |error| panic!("{error:?}"));
            assert_eq!(tree.syntax().text().to_string(), source);
            stacker::grow(16 * 1024 * 1024, || drop(tree));
        }
    }

    #[test]
    fn indentation_requires_spaces() {
        let mut errors = Vec::new();
        parse_module("if x:\n\tpass\n", &mut |error| errors.push(error));
        assert!(errors
            .iter()
            .any(|error| error.message == "Starlark indentation must use spaces"));
        for source in ["x = (\n\t1\n)\n", "x = '''\n\ttext\n'''\n"] {
            parse_module(source, &mut |error| panic!("{source}: {error:?}"));
        }
    }
}
