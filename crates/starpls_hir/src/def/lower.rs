use std::sync::Arc;

use either::Either;
use ruff_python_ast as py;
use ruff_python_ast::token::parentheses_iterator;
use ruff_python_ast::token::TokenKind;
use ruff_python_ast::token::Tokens;
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::ArgOrKeyword;
use ruff_python_ast::HasNodeIndex;
use ruff_text_size::Ranged;
use ruff_text_size::TextRange;
use ruff_text_size::TextSize;
use salsa::Accumulator;
use starpls_common::diagnostic;
use starpls_common::line_index;
use starpls_common::DiagnosticId;
use starpls_common::Diagnostics;
use starpls_common::File;
use starpls_common::Severity;
use starpls_intern::Interned;
use starpls_syntax::ast::AstNode;
use starpls_syntax::ast::{self};
use starpls_syntax::source::expr_range;
use starpls_syntax::source::string_value;
use starpls_syntax::source::suite_range;
use starpls_syntax::TypeComment;

use crate::def::ops;
use crate::def::Argument;
use crate::def::AssignmentSource;
use crate::def::CompClause;
use crate::def::DictEntry;
use crate::def::Expr;
use crate::def::ExprId;
use crate::def::FunctionData;
use crate::def::Literal;
use crate::def::LoadItem;
use crate::def::LoadItemId;
use crate::def::LoadStmtData;
use crate::def::Module;
use crate::def::ModuleSourceMap;
use crate::def::Name;
use crate::def::Param;
use crate::def::ParamId;
use crate::def::Stmt;
use crate::def::StmtId;
use crate::def::TypeCommentOwner;
use crate::typeck::FunctionTypeRef;
use crate::Db;
use crate::TypeRef;

pub(super) fn lower_module(db: &dyn Db, file: File) -> (Module, ModuleSourceMap) {
    let source = file.contents(db);
    let parsed = starpls_common::parsed_module(db, file).load(db);
    let comments = starpls_common::syntax_info(db, file);
    LoweringContext {
        db,
        file,
        source: &source,
        tokens: parsed.tokens(),
        comments,
        module: Default::default(),
        source_map: ModuleSourceMap {
            root: source_range(TextRange::up_to(TextSize::of(&*source))),
            expr_nodes: Default::default(),
            stmt_nodes: Default::default(),
            param_nodes: Default::default(),
            load_item_nodes: Default::default(),
            function_names: Default::default(),
            keyword_names: Default::default(),
            type_comment_owners: Default::default(),
            expr_map_back: Default::default(),
            stmt_map_back: Default::default(),
            param_map_back: Default::default(),
            load_item_map_back: Default::default(),
        },
    }
    .lower(parsed.syntax())
}

fn source_range(range: TextRange) -> starpls_syntax::TextRange {
    starpls_syntax::TextRange::new(
        u32::from(range.start()).into(),
        u32::from(range.end()).into(),
    )
}

struct LoweredClause {
    statements: Either<StmtId, Box<[StmtId]>>,
    range: TextRange,
}

struct LoweringContext<'a> {
    db: &'a dyn Db,
    file: File,
    source: &'a str,
    tokens: &'a Tokens,
    comments: &'a [TypeComment],
    module: Module,
    source_map: ModuleSourceMap,
}

impl<'a> LoweringContext<'a> {
    fn lower(mut self, syntax: &py::ModModule) -> (Module, ModuleSourceMap) {
        let line_index = line_index(self.db, self.file);
        self.module.type_ignore_comment_lines = self
            .comments
            .iter()
            .filter(|comment| {
                comment
                    .parsed
                    .syntax()
                    .descendants()
                    .any(|node| ast::IgnoreType::cast(node).is_some())
            })
            .map(|comment| {
                line_index
                    .line_index(comment.range.start())
                    .to_zero_indexed() as u32
            })
            .collect();
        let mut top_level = Vec::new();
        for statement in &syntax.body {
            let Some(stmt) = self.lower_stmt(statement) else {
                continue;
            };
            top_level.push(stmt);
            match &self.module.stmts[stmt] {
                Stmt::If { .. } => self.add_error_diagnostic(
                    "Starlark does not allow top-level if statements",
                    self.source_map.stmt_map_back[&stmt],
                ),
                Stmt::For { .. } => self.add_error_diagnostic(
                    "Starlark does not allow top-level for statements",
                    self.source_map.stmt_map_back[&stmt],
                ),
                _ => {}
            }
        }
        self.module.top_level = top_level.into_boxed_slice();
        (self.module, self.source_map)
    }

    fn comment_in(&self, range: TextRange) -> Option<&'a TypeComment> {
        let comments = &self.comments[self
            .comments
            .partition_point(|comment| comment.range.start() < range.start())..];
        comments
            .first()
            .filter(|comment| range.contains_range(comment.range))
    }

    fn assignment_comment(&self, range: TextRange) -> Option<&'a TypeComment> {
        let end = self
            .tokens
            .after(range.end())
            .iter()
            .find(|token| matches!(token.kind(), TokenKind::Newline | TokenKind::Semi))
            .map_or(TextSize::of(self.source), Ranged::start);
        self.comment_in(TextRange::new(range.end(), end.max(range.end())))
    }

    fn lower_stmt(&mut self, stmt: &py::Stmt) -> Option<StmtId> {
        stacker::maybe_grow(32 * 1024, 1024 * 1024, || {
            let id = self.lower_stmt_inner(stmt)?;
            self.source_map
                .stmt_nodes
                .insert(stmt.node_index().load(), id);
            Some(id)
        })
    }

    fn lower_stmt_inner(&mut self, stmt: &py::Stmt) -> Option<StmtId> {
        let parent = AnyNodeRef::from(stmt);
        let mut range = stmt.range();
        let mut comment_range = None;
        let statement = match stmt {
            py::Stmt::FunctionDef(def) => {
                let py::StmtFunctionDef {
                    node_index: _,
                    range: _,
                    is_async: _,
                    decorator_list: _,
                    name,
                    type_params: _,
                    parameters,
                    returns: _,
                    body,
                } = def;
                let suite = suite_range(self.tokens, body, parameters.end(), range.end(), None);
                let comment = suite.and_then(|range| {
                    self.comment_in(TextRange::new(
                        range.start(),
                        body.first()
                            .map_or(range.end(), Ranged::start)
                            .max(range.start()),
                    ))
                });
                comment_range = comment.map(|comment| source_range(comment.range));
                let spec = self.lower_func_type_opt(
                    comment.and_then(|comment| comment.parsed.tree().function_type()),
                );
                let doc = suite.and_then(|_| self.doc(body));
                let params =
                    self.lower_params(parameters, spec.as_ref().map_or(&[], |spec| &spec.0), &doc);
                let stmts = self.lower_suite(body, suite);
                range = self.cover_statements(range, &stmts);
                if let Some(suite) = suite {
                    range = range.cover(suite);
                }
                let func = Interned::new(FunctionData {
                    file: self.file,
                    name: self.name(name.as_str()),
                    ret_type_ref: spec.map(|spec| spec.1),
                    doc,
                    range: source_range(range),
                    params,
                });
                let id = self.alloc_stmt(
                    Stmt::Def {
                        func: func.clone(),
                        stmts,
                    },
                    source_range(range),
                    comment_range,
                );
                if !name.is_empty() {
                    self.source_map
                        .function_names
                        .insert(id, source_range(name.range()));
                }
                for (index, param) in func.params.iter().enumerate() {
                    self.module.param_to_def_stmt.insert(*param, (id, index));
                }
                return Some(id);
            }
            py::Stmt::If(node) => {
                let py::StmtIf {
                    node_index: _,
                    range: _,
                    test,
                    body,
                    elif_else_clauses,
                } = node;
                let suite = suite_range(
                    self.tokens,
                    body,
                    test.end(),
                    range.end(),
                    elif_else_clauses.first().map(Ranged::start),
                );
                let test = self.lower_expr(test, parent);
                let if_stmts = self.lower_suite(body, suite);
                let elif_or_else_stmts = self.lower_clauses(elif_else_clauses).map(|clause| {
                    let LoweredClause {
                        statements,
                        range: tail_range,
                    } = clause;
                    range = range.cover(tail_range);
                    statements
                });
                range = self.cover_statements(range, &if_stmts);
                if let Some(suite) = suite {
                    range = range.cover(suite);
                }
                Stmt::If {
                    test,
                    if_stmts,
                    elif_or_else_stmts,
                }
            }
            py::Stmt::For(node) => {
                let py::StmtFor {
                    node_index: _,
                    range: _,
                    is_async: _,
                    target,
                    iter,
                    body,
                    orelse: _,
                } = node;
                let suite = suite_range(self.tokens, body, iter.end(), range.end(), None);
                let iterable = self.lower_expr(iter, parent);
                let targets = self.lower_loop_variables(target, parent);
                let stmts = self.lower_suite(body, suite);
                range = self.cover_statements(range, &stmts);
                if let Some(suite) = suite {
                    range = range.cover(suite);
                }
                Stmt::For {
                    iterable,
                    targets,
                    stmts,
                }
            }
            py::Stmt::Return(node) => Stmt::Return {
                expr: node
                    .value
                    .as_deref()
                    .map(|expr| self.lower_expr(expr, parent)),
            },
            py::Stmt::Break(_) => Stmt::Break,
            py::Stmt::Continue(_) => Stmt::Continue,
            py::Stmt::Pass(_) => Stmt::Pass,
            py::Stmt::Assign(node) => {
                let py::StmtAssign {
                    node_index: _,
                    range: _,
                    targets,
                    value,
                } = node;
                let lhs = match targets.first() {
                    Some(expr) => self.lower_expr(expr, parent),
                    None => self.lower_expr_missing(),
                };
                let rhs = self.lower_expr(value, parent);
                let comment = self.assignment_comment(range);
                comment_range = comment.map(|comment| source_range(comment.range));
                let type_ref = self.lower_type_comment_opt(comment);
                Stmt::Assign {
                    lhs,
                    rhs,
                    op: Some(ops::AssignOp::Normal),
                    type_ref,
                }
            }
            py::Stmt::AugAssign(node) => {
                let py::StmtAugAssign {
                    node_index: _,
                    range: _,
                    target,
                    op,
                    value,
                } = node;
                let lhs = self.lower_expr(target, parent);
                let rhs = self.lower_expr(value, parent);
                let comment = self.assignment_comment(range);
                comment_range = comment.map(|comment| source_range(comment.range));
                Stmt::Assign {
                    lhs,
                    rhs,
                    op: assign_op(*op),
                    type_ref: self.lower_type_comment_opt(comment),
                }
            }
            py::Stmt::Expr(node) => {
                if let py::Expr::Call(call) = node.value.as_ref() {
                    if let py::Expr::Name(name) = call.func.as_ref() {
                        if name.id == "load" {
                            return Some(self.lower_load(call));
                        }
                    }
                }
                let expr = self.lower_expr(&node.value, parent);
                // Unsupported statements have no HIR entry. Parenthesized
                // recovery still has a located Paren containing Missing.
                let range = self.source_map.expr_map_back.get(&expr).copied()?;
                return Some(self.alloc_stmt(Stmt::Expr { expr }, range, None));
            }
            _ => return None,
        };
        Some(self.alloc_stmt(statement, source_range(range), comment_range))
    }

    fn lower_suite(&mut self, body: &[py::Stmt], suite: Option<TextRange>) -> Box<[StmtId]> {
        if suite.is_none() {
            return Box::default();
        }
        body.iter()
            .filter_map(|stmt| self.lower_stmt(stmt))
            .collect()
    }

    fn lower_clauses(&mut self, clauses: &[py::ElifElseClause]) -> Option<LoweredClause> {
        let mut branches = Vec::new();
        let mut tail: Option<LoweredClause> = None;
        for (index, clause) in clauses.iter().enumerate() {
            let py::ElifElseClause {
                node_index: _,
                range,
                test,
                body,
            } = clause;
            let after = test.as_ref().map_or(range.start(), Ranged::end);
            let suite = suite_range(
                self.tokens,
                body,
                after,
                range.end(),
                clauses.get(index + 1).map(Ranged::start),
            );
            let test = test
                .as_ref()
                .map(|test| self.lower_expr(test, clause.into()));
            let stmts = self.lower_suite(body, suite);
            let range =
                self.cover_statements(suite.map_or(*range, |suite| range.cover(suite)), &stmts);
            match test {
                Some(test) => branches.push((range, test, stmts)),
                None => {
                    tail = Some(LoweredClause {
                        statements: Either::Right(stmts),
                        range,
                    });
                }
            }
        }
        // Ruff stores elif clauses flat; constructing nested HIR must not grow
        // the stack with the number of clauses.
        for (range, test, if_stmts) in branches.into_iter().rev() {
            let mut range = range;
            let elif_or_else_stmts = tail.map(|clause| {
                let LoweredClause {
                    statements,
                    range: tail_range,
                } = clause;
                range = range.cover(tail_range);
                statements
            });
            let stmt = self.alloc_stmt(
                Stmt::If {
                    test,
                    if_stmts,
                    elif_or_else_stmts,
                },
                source_range(range),
                None,
            );
            tail = Some(LoweredClause {
                statements: Either::Left(stmt),
                range,
            });
        }
        tail
    }

    fn cover_statements(&self, range: TextRange, stmts: &[StmtId]) -> TextRange {
        stmts.iter().fold(range, |range, stmt| {
            let child = self.source_map.stmt_map_back[stmt];
            range.cover(TextRange::new(
                u32::from(child.start()).into(),
                u32::from(child.end()).into(),
            ))
        })
    }

    fn lower_expr_missing(&mut self) -> ExprId {
        self.module.exprs.alloc(Expr::Missing)
    }

    fn lower_expr(&mut self, expr: &py::Expr, parent: AnyNodeRef<'_>) -> ExprId {
        stacker::maybe_grow(32 * 1024, 1024 * 1024, || {
            let mut id = self.lower_bare_expr(expr);
            if !matches!(self.module.exprs[id], Expr::Missing) {
                self.source_map
                    .expr_nodes
                    .insert(expr.node_index().load(), id);
            }
            for range in parentheses_iterator(expr.into(), Some(parent), self.tokens)
                .take_while(|range| range.start() >= parent.start())
            {
                id = self.alloc_expr(Expr::Paren { expr: id }, source_range(range));
            }
            id
        })
    }

    fn lower_bare_expr(&mut self, expr: &py::Expr) -> ExprId {
        let parent = expr.into();
        let range = expr.range();
        if !starpls_syntax::supports_expr(expr.into(), self.tokens) {
            return self.lower_expr_missing();
        }
        let lowered = match expr {
            py::Expr::Name(node) => {
                if node.id.is_empty() {
                    return self.lower_expr_missing();
                }
                Expr::Name {
                    name: self.name(node.id.as_str()),
                }
            }
            py::Expr::NumberLiteral(node) => Expr::Literal {
                literal: match &node.value {
                    py::Number::Int(value) => Literal::Int(value.as_u64().unwrap_or(0)),
                    py::Number::Float(_) => Literal::Float,
                    py::Number::Complex { .. } => unreachable!("unsupported expression"),
                },
            },
            py::Expr::BooleanLiteral(node) => Expr::Literal {
                literal: Literal::Bool(node.value),
            },
            py::Expr::NoneLiteral(_) => Expr::Literal {
                literal: Literal::None,
            },
            py::Expr::StringLiteral(_) => Expr::Literal {
                literal: Literal::String(Arc::from(self.string(range))),
            },
            py::Expr::BytesLiteral(_) => Expr::Literal {
                literal: Literal::Bytes,
            },
            py::Expr::If(node) => {
                let py::ExprIf {
                    node_index: _,
                    range: _,
                    test,
                    body,
                    orelse,
                } = node;
                let if_expr = self.lower_expr(body, parent);
                let test = self.lower_expr(test, parent);
                let else_expr = self.lower_expr(orelse, parent);
                Expr::If {
                    if_expr,
                    test,
                    else_expr,
                }
            }
            py::Expr::UnaryOp(node) => {
                let py::ExprUnaryOp {
                    node_index: _,
                    range: _,
                    op,
                    operand,
                } = node;
                let expr = self.lower_expr(operand, parent);
                let op = match op {
                    py::UnaryOp::Invert => ops::UnaryOp::Inv,
                    py::UnaryOp::Not => ops::UnaryOp::Not,
                    py::UnaryOp::UAdd => ops::UnaryOp::Arith(ops::UnaryArithOp::Add),
                    py::UnaryOp::USub => ops::UnaryOp::Arith(ops::UnaryArithOp::Sub),
                };
                Expr::Unary { expr, op: Some(op) }
            }
            py::Expr::BinOp(node) => {
                let py::ExprBinOp {
                    node_index: _,
                    range: _,
                    left,
                    op,
                    right,
                } = node;
                let lhs = self.lower_expr(left, parent);
                let rhs = self.lower_expr(right, parent);
                Expr::Binary {
                    lhs,
                    rhs,
                    op: binary_op(*op),
                }
            }
            py::Expr::BoolOp(node) => {
                let py::ExprBoolOp {
                    node_index: _,
                    range: _,
                    op,
                    values,
                } = node;
                let op = ops::BinaryOp::Logic(match op {
                    py::BoolOp::And => ops::LogicOp::And,
                    py::BoolOp::Or => ops::LogicOp::Or,
                });
                return self.lower_chain(
                    values,
                    std::iter::repeat_n(Some(op), values.len().saturating_sub(1)),
                    parent,
                );
            }
            py::Expr::Compare(node) => {
                let py::ExprCompare {
                    node_index: _,
                    range: _,
                    operands,
                    ops,
                } = node;
                return self.lower_chain(operands, ops.iter().map(|op| compare_op(*op)), parent);
            }
            py::Expr::Lambda(node) => {
                let py::ExprLambda {
                    node_index: _,
                    range: _,
                    parameters,
                    body,
                } = node;
                let params = match parameters {
                    Some(params) => self.lower_params(params, &[], &None),
                    None => Box::default(),
                };
                let func = Interned::new(FunctionData {
                    file: self.file,
                    name: Name::new_inline("lambda"),
                    ret_type_ref: None,
                    doc: None,
                    range: source_range(range),
                    params,
                });
                let body = self.lower_expr(body, parent);
                Expr::Lambda { func, body }
            }
            py::Expr::List(node) => Expr::List {
                exprs: node
                    .elts
                    .iter()
                    .map(|expr| self.lower_expr(expr, parent))
                    .collect(),
            },
            py::Expr::Tuple(node) => Expr::Tuple {
                exprs: node
                    .elts
                    .iter()
                    .map(|expr| self.lower_expr(expr, parent))
                    .collect(),
            },
            py::Expr::ListComp(node) => {
                let py::ExprListComp {
                    node_index: _,
                    range: _,
                    elt,
                    generators,
                } = node;
                let expr = self.lower_expr(elt, parent);
                let comp_clauses = self.lower_comp_clauses(generators);
                Expr::ListComp { expr, comp_clauses }
            }
            py::Expr::Dict(node) => {
                let entries = node
                    .items
                    .iter()
                    .filter_map(|item| {
                        let py::DictItem { key, value } = item;
                        let key = self.lower_expr(key.as_ref()?, parent);
                        let value = self.lower_expr(value, parent);
                        Some(DictEntry { key, value })
                    })
                    .collect();
                Expr::Dict { entries }
            }
            py::Expr::DictComp(node) => {
                let py::ExprDictComp {
                    node_index: _,
                    range: _,
                    key,
                    value,
                    generators,
                } = node;
                let key = self.lower_expr(
                    key.as_deref().expect("supported dict comprehension"),
                    parent,
                );
                let value = self.lower_expr(value, parent);
                let comp_clauses = self.lower_comp_clauses(generators);
                Expr::DictComp {
                    entry: DictEntry { key, value },
                    comp_clauses,
                }
            }
            py::Expr::Attribute(node) => {
                let py::ExprAttribute {
                    node_index: _,
                    range: _,
                    value,
                    attr,
                    ctx: _,
                } = node;
                let field = self.name(attr.as_str());
                let expr = self.lower_expr(value, parent);
                Expr::Dot { expr, field }
            }
            py::Expr::Call(node) => {
                let py::ExprCall {
                    node_index: _,
                    range_start: _,
                    func,
                    arguments,
                } = node;
                let callee = self.lower_expr(func, parent);
                let args = self.lower_args(arguments);
                let impl_fn_name = args.iter().find_map(|arg| {
                    if let Argument::Keyword { name, expr } = arg {
                        if name.as_str() == "implementation" {
                            if let Expr::Name { name } = &self.module.exprs[*expr] {
                                return Some(name.clone());
                            }
                        }
                    }
                    None
                });
                let id = self.alloc_expr(Expr::Call { callee, args }, source_range(range));
                if let Some(name) = impl_fn_name {
                    self.module.call_expr_with_impl_fn.insert(name, id);
                }
                return id;
            }
            py::Expr::Subscript(node) => {
                let py::ExprSubscript {
                    node_index: _,
                    range: _,
                    value,
                    slice,
                    ctx: _,
                } = node;
                let lhs = self.lower_expr(value, parent);
                match slice.as_ref() {
                    py::Expr::Slice(slice) => {
                        let py::ExprSlice {
                            node_index: _,
                            range: _,
                            lower,
                            upper,
                            step,
                        } = slice;
                        let start = lower.as_deref().map(|expr| self.lower_expr(expr, parent));
                        let end = upper.as_deref().map(|expr| self.lower_expr(expr, parent));
                        let step = step.as_deref().map(|expr| self.lower_expr(expr, parent));
                        Expr::Slice {
                            lhs,
                            start,
                            end,
                            step,
                        }
                    }
                    expr => Expr::Index {
                        lhs,
                        index: self.lower_expr(expr, parent),
                    },
                }
            }
            _ => return self.lower_expr_missing(),
        };
        self.alloc_expr(lowered, source_range(range))
    }

    fn lower_chain(
        &mut self,
        values: &[py::Expr],
        ops: impl IntoIterator<Item = Option<ops::BinaryOp>>,
        parent: AnyNodeRef<'_>,
    ) -> ExprId {
        let Some((first, rest)) = values.split_first() else {
            return self.lower_expr_missing();
        };
        let mut id = self.lower_expr(first, parent);
        let mut range = expr_range(first, parent, self.tokens);
        for (value, op) in rest.iter().zip(ops) {
            let rhs = self.lower_expr(value, parent);
            range = range.cover(expr_range(value, parent, self.tokens));
            id = self.alloc_expr(Expr::Binary { lhs: id, rhs, op }, source_range(range));
        }
        id
    }

    fn lower_args(&mut self, arguments: &py::Arguments) -> Box<[Argument]> {
        arguments
            .iter_source_order()
            .map(|argument| match argument {
                ArgOrKeyword::Arg(arg) => {
                    if let py::Expr::Starred(starred) = arg {
                        return Argument::UnpackedList {
                            expr: self.lower_expr(&starred.value, starred.into()),
                        };
                    }
                    Argument::Simple {
                        expr: self.lower_expr(arg, arguments.into()),
                    }
                }
                ArgOrKeyword::Keyword(keyword) => {
                    let py::Keyword {
                        node_index: _,
                        range: _,
                        arg,
                        value,
                    } = keyword;
                    let expr = self.lower_expr(value, arguments.into());
                    match arg {
                        Some(name) => {
                            self.source_map
                                .keyword_names
                                .insert(expr, source_range(name.range()));
                            Argument::Keyword {
                                name: self.name(name.as_str()),
                                expr,
                            }
                        }
                        None => Argument::UnpackedDict { expr },
                    }
                }
            })
            .collect()
    }

    fn lower_comp_clauses(&mut self, generators: &[py::Comprehension]) -> Box<[CompClause]> {
        let mut clauses = Vec::new();
        for generator in generators {
            let py::Comprehension {
                node_index: _,
                range: _,
                target,
                iter,
                ifs,
                is_async: _,
            } = generator;
            let parent = generator.into();
            let iterable = self.lower_expr(iter, parent);
            let targets = self.lower_loop_variables(target, parent);
            clauses.push(CompClause::For { iterable, targets });
            clauses.extend(ifs.iter().map(|test| CompClause::If {
                test: self.lower_expr(test, parent),
            }));
        }
        clauses.into_boxed_slice()
    }

    fn lower_loop_variables(&mut self, target: &py::Expr, parent: AnyNodeRef<'_>) -> Box<[ExprId]> {
        if let py::Expr::Tuple(tuple) = target {
            if !tuple.parenthesized {
                return tuple
                    .elts
                    .iter()
                    .map(|expr| self.lower_expr(expr, target.into()))
                    .collect();
            }
        }
        Box::new([self.lower_expr(target, parent)])
    }

    fn lower_load(&mut self, call: &py::ExprCall) -> StmtId {
        let range = source_range(call.range());
        let mut args = call.arguments.iter_source_order().peekable();
        let module = match args.peek() {
            Some(ArgOrKeyword::Arg(_)) => {
                let arg = args.next().expect("peeked argument");
                self.load_string(arg.range())
            }
            _ => Box::default(),
        };
        let load_stmt = Arc::new(LoadStmtData { module, range });
        let items = args
            .map(|arg| {
                let item = match arg {
                    ArgOrKeyword::Arg(expr) => LoadItem::Direct {
                        name: self.load_string(expr.range()),
                        load_stmt: load_stmt.clone(),
                    },
                    ArgOrKeyword::Keyword(keyword) => LoadItem::Aliased {
                        alias: keyword
                            .arg
                            .as_ref()
                            .map_or_else(Name::missing, |name| self.name(name.as_str())),
                        name: self.load_string(keyword.value.range()),
                        load_stmt: load_stmt.clone(),
                    },
                };
                let id = self.alloc_load_item(item, source_range(arg.range()));
                self.source_map
                    .load_item_nodes
                    .insert(crate::def::load_item_node(arg), id);
                id
            })
            .collect();
        let id = self.alloc_stmt(Stmt::Load { load_stmt, items }, range, None);
        self.source_map
            .stmt_nodes
            .insert(call.node_index().load(), id);
        id
    }

    fn name(&self, name: &str) -> Name {
        if name.is_empty() || name == "load" {
            Name::missing()
        } else {
            Name::from_str(name)
        }
    }

    fn string(&self, range: TextRange) -> Box<str> {
        string_value(&self.source[range])
            .map(|(value, _)| value)
            .unwrap_or_default()
    }

    fn load_string(&self, range: TextRange) -> Box<str> {
        self.tokens
            .in_range(range)
            .iter()
            .find(|token| {
                token.kind() == TokenKind::String && !token.unwrap_string_flags().is_byte_string()
            })
            .map(|token| self.string(token.range()))
            .unwrap_or_default()
    }

    fn doc(&self, body: &[py::Stmt]) -> Option<Box<str>> {
        // Preserve Starpls' first direct literal rule, including literals after
        // other statements. A preceding non-string literal prevents a docstring.
        let expr = body.iter().find_map(|stmt| {
            let py::Stmt::Expr(stmt) = stmt else {
                return None;
            };
            if !starpls_syntax::supports_expr(stmt.value.as_ref().into(), self.tokens)
                || expr_range(&stmt.value, stmt.into(), self.tokens) != stmt.value.range()
            {
                return None;
            }
            match stmt.value.as_ref() {
                py::Expr::NumberLiteral(_) => Some(stmt.value.as_ref()),
                py::Expr::BooleanLiteral(_) => Some(stmt.value.as_ref()),
                py::Expr::NoneLiteral(_) => Some(stmt.value.as_ref()),
                py::Expr::StringLiteral(_) => Some(stmt.value.as_ref()),
                py::Expr::BytesLiteral(_) => Some(stmt.value.as_ref()),
                _ => None,
            }
        })?;
        if matches!(expr, py::Expr::StringLiteral(_)) {
            return string_value(&self.source[expr.range()]).map(|(value, _)| value);
        }
        None
    }
    fn lower_params(
        &mut self,
        syntax: &py::Parameters,
        spec_type_refs: &[TypeRef],
        doc: &Option<Box<str>>,
    ) -> Box<[ParamId]> {
        let mut parameters = syntax
            .iter_source_order()
            .map(|param| {
                let range = starpls_syntax::source::parameter_range(param, syntax, self.tokens);
                (Some(param), range)
            })
            .collect::<Vec<_>>();
        if let Some(range) = starpls_syntax::source::bare_star_range(syntax, self.tokens) {
            parameters.push((None, range));
        }
        parameters.sort_by_key(|(_, range)| range.start());
        let find_doc = |name: &str| {
            let prefix = format!("{name}:");
            doc.as_ref().and_then(|doc| {
                doc.lines().find_map(|line| {
                    line.trim()
                        .trim_start_matches('*')
                        .strip_prefix(&prefix)
                        .map(|text| text.trim().to_owned().into_boxed_str())
                })
            })
        };
        let mut saw_star_arg = false;
        let mut saw_star_star_arg = false;
        let mut saw_default_param = false;
        let mut saw_names = Vec::new();
        let mut params = Vec::new();
        for (index, (parameter, range)) in parameters.iter().copied().enumerate() {
            let end = parameters
                .get(index + 1)
                .map_or(syntax.end(), |(_, range)| range.start());
            let comment = self.comment_in(TextRange::new(range.end(), end.max(range.end())));
            let comment_range = comment.map(|comment| source_range(comment.range));
            let type_ref = self
                .lower_type_comment_opt(comment)
                .map(|(ty, _)| ty)
                .or_else(|| spec_type_refs.get(index).cloned());
            let param = match parameter {
                Some(parameter) => match parameter {
                    py::AnyParameterRef::NonVariadic(param) => {
                        let py::ParameterWithDefault {
                            range: _,
                            node_index: _,
                            parameter,
                            default,
                        } = param;
                        let name = self.name(parameter.name.as_str());
                        let doc = find_doc(name.as_str());
                        let default = default
                            .as_deref()
                            .map(|expr| self.lower_expr(expr, param.into()));
                        Param::Simple {
                            name,
                            default,
                            type_ref,
                            doc,
                        }
                    }
                    py::AnyParameterRef::Variadic(param) => {
                        let name = self.name(param.name.as_str());
                        let doc = find_doc(name.as_str());
                        if syntax
                            .kwarg
                            .as_deref()
                            .is_some_and(|kwarg| kwarg.range() == param.range())
                        {
                            Param::KwargsDict {
                                name,
                                type_ref,
                                doc,
                            }
                        } else {
                            Param::ArgsList {
                                name,
                                type_ref,
                                doc,
                            }
                        }
                    }
                },
                None => Param::ArgsList {
                    name: Name::missing(),
                    type_ref,
                    doc: None,
                },
            };
            let range = source_range(range);
            let name = param.name();
            if !name.is_missing() && saw_names.contains(name) {
                self.add_error_diagnostic(&format!("Duplicate parameter {}", name.as_str()), range);
            } else {
                saw_names.push(name.clone());
            }
            match &param {
                Param::Simple {
                    name: _,
                    default,
                    type_ref: _,
                    doc: _,
                } => {
                    if saw_default_param && !saw_star_arg && default.is_none() {
                        self.add_error_diagnostic(
                            "Non-default parameter cannot follow default parameter",
                            range,
                        );
                    }
                    if default.is_some() {
                        saw_default_param = true;
                    }
                    if saw_star_star_arg {
                        self.add_error_diagnostic(
                            "Parameter cannot follow \"**\" parameter",
                            range,
                        );
                    }
                }
                Param::ArgsList {
                    name: _,
                    type_ref: _,
                    doc: _,
                } => {
                    if saw_star_arg {
                        self.add_error_diagnostic("Only one \"*\" parameter is allowed", range);
                    }
                    if saw_star_star_arg {
                        self.add_error_diagnostic(
                            "Parameter cannot follow \"**\" parameter",
                            range,
                        );
                    }
                    saw_star_arg = true;
                }
                Param::KwargsDict {
                    name: _,
                    type_ref: _,
                    doc: _,
                } => {
                    if saw_star_star_arg {
                        self.add_error_diagnostic("Only one \"**\" parameter is allowed", range);
                    }
                    saw_star_star_arg = true;
                }
            }
            let id = self.alloc_param(param, range);
            if let Some(parameter) = parameter {
                let node = match parameter {
                    py::AnyParameterRef::NonVariadic(param) => &param.parameter,
                    py::AnyParameterRef::Variadic(param) => param,
                };
                self.source_map
                    .param_nodes
                    .insert(node.node_index().load(), id);
            }
            if let Some(range) = comment_range {
                self.source_map
                    .type_comment_owners
                    .insert(range, TypeCommentOwner::Parameter(id));
            }
            params.push(id);
        }
        params.into_boxed_slice()
    }
    fn lower_type_comment_opt(
        &self,
        node: Option<&TypeComment>,
    ) -> Option<(TypeRef, starpls_syntax::TextRange)> {
        node.map(|node| {
            let range = source_range(node.range);
            (self.lower_type_comment(node.parsed.tree()), range)
        })
    }

    fn lower_type_comment(&self, node: ast::TypeComment) -> TypeRef {
        node.type_()
            .map(Self::lower_type)
            .unwrap_or_else(|| TypeRef::Unknown)
    }

    fn lower_func_type_opt(&self, node: Option<ast::FunctionType>) -> Option<FunctionTypeRef> {
        node.map(|func_type| {
            let params = match func_type.parameter_types() {
                Some(params) => params
                    .types()
                    .map(|param| self.lower_type_opt(param.type_()))
                    .collect(),
                None => vec![],
            };
            let ret_type_ref = func_type
                .ret_type()
                .map(Self::lower_type)
                .unwrap_or(TypeRef::Unknown);
            FunctionTypeRef(params, ret_type_ref)
        })
    }

    fn lower_type(node: ast::Type) -> TypeRef {
        match node {
            ast::Type::PathType(node) => {
                let segments = node
                    .segments()
                    .flat_map(|segment| segment.value())
                    .map(|token| Name::from_str(token.text()))
                    .collect();
                Some(TypeRef::Path(
                    segments,
                    node.generic_arguments().map(|args| {
                        let args = args.types().map(Self::lower_type);
                        args.collect::<Vec<_>>().into_boxed_slice()
                    }),
                ))
            }
            ast::Type::UnionType(node) => {
                Some(TypeRef::Union(node.types().map(Self::lower_type).collect()))
            }
            ast::Type::NoneType(_) => Some(TypeRef::Name(Name::new_inline("None"), None)),
            ast::Type::EllipsisType(_) => Some(TypeRef::Ellipsis),
        }
        .unwrap_or_else(|| TypeRef::Unknown)
    }

    fn lower_type_opt(&self, node: Option<ast::Type>) -> TypeRef {
        node.map(Self::lower_type).unwrap_or(TypeRef::Unknown)
    }

    fn alloc_stmt(
        &mut self,
        stmt: Stmt,
        range: starpls_syntax::TextRange,
        type_comment: Option<starpls_syntax::TextRange>,
    ) -> StmtId {
        let Self {
            db: _,
            file: _,
            source: _,
            tokens: _,
            comments: _,
            module,
            source_map,
        } = self;
        let id = module.stmts.alloc(stmt);
        if let Some(range) = type_comment {
            source_map
                .type_comment_owners
                .insert(range, TypeCommentOwner::Statement(id));
        }
        let source = match &module.stmts[id] {
            Stmt::Assign {
                lhs: _,
                rhs,
                op: _,
                type_ref: _,
            } => Some(*rhs),
            Stmt::For {
                iterable,
                targets: _,
                stmts: _,
            } => Some(*iterable),
            _ => None,
        };
        if let Some(source) = source {
            // Synthetic missing expressions had no syntax parent to infer from.
            if source_map.expr_map_back.contains_key(&source) {
                module
                    .assignment_sources
                    .insert(source, AssignmentSource::Statement(id));
            }
        }
        source_map.stmt_map_back.insert(id, range);
        id
    }

    fn alloc_expr(&mut self, expr: Expr, range: starpls_syntax::TextRange) -> ExprId {
        let Self {
            db: _,
            file: _,
            source: _,
            tokens: _,
            comments: _,
            module,
            source_map,
        } = self;
        let id = module.exprs.alloc(expr);
        let clauses = match &module.exprs[id] {
            Expr::ListComp {
                expr: _,
                comp_clauses,
            } => Some(comp_clauses),
            Expr::DictComp {
                entry: _,
                comp_clauses,
            } => Some(comp_clauses),
            _ => None,
        };
        if let Some(clauses) = clauses {
            for (clause, comp_clause) in clauses.iter().enumerate() {
                if let CompClause::For {
                    iterable,
                    targets: _,
                } = comp_clause
                {
                    if source_map.expr_map_back.contains_key(iterable) {
                        module.assignment_sources.insert(
                            *iterable,
                            AssignmentSource::Comprehension {
                                expression: id,
                                clause,
                            },
                        );
                    }
                }
            }
        }
        source_map.expr_map_back.insert(id, range);
        id
    }

    fn alloc_param(&mut self, param: Param, range: starpls_syntax::TextRange) -> ParamId {
        let id = self.module.params.alloc(param);
        self.source_map.param_map_back.insert(id, range);
        id
    }

    fn alloc_load_item(
        &mut self,
        load_item: LoadItem,
        range: starpls_syntax::TextRange,
    ) -> LoadItemId {
        let id = self.module.load_items.alloc(load_item);
        self.source_map.load_item_map_back.insert(id, range);
        id
    }

    fn add_error_diagnostic(&self, message: &str, range: starpls_syntax::TextRange) {
        Diagnostics(diagnostic(
            self.file,
            DiagnosticId::InvalidSyntax,
            Severity::Error,
            range,
            message,
            [],
        ))
        .accumulate(self.db);
    }
}

fn binary_op(op: py::Operator) -> Option<ops::BinaryOp> {
    Some(match op {
        py::Operator::Add => ops::BinaryOp::Arith(ops::ArithOp::Add),
        py::Operator::Sub => ops::BinaryOp::Arith(ops::ArithOp::Sub),
        py::Operator::Mult => ops::BinaryOp::Arith(ops::ArithOp::Mul),
        py::Operator::Div => ops::BinaryOp::Arith(ops::ArithOp::Div),
        py::Operator::FloorDiv => ops::BinaryOp::Arith(ops::ArithOp::Flr),
        py::Operator::Mod => ops::BinaryOp::Arith(ops::ArithOp::Mod),
        py::Operator::BitAnd => ops::BinaryOp::Bitwise(ops::BitwiseOp::And),
        py::Operator::BitOr => ops::BinaryOp::Bitwise(ops::BitwiseOp::Or),
        py::Operator::BitXor => ops::BinaryOp::Bitwise(ops::BitwiseOp::Xor),
        py::Operator::LShift => ops::BinaryOp::Bitwise(ops::BitwiseOp::Shl),
        py::Operator::RShift => ops::BinaryOp::Bitwise(ops::BitwiseOp::Shr),
        py::Operator::Pow => return None,
        py::Operator::MatMult => return None,
    })
}

fn assign_op(op: py::Operator) -> Option<ops::AssignOp> {
    Some(match op {
        py::Operator::Add => ops::AssignOp::Arith(ops::ArithAssignOp::Add),
        py::Operator::Sub => ops::AssignOp::Arith(ops::ArithAssignOp::Sub),
        py::Operator::Mult => ops::AssignOp::Arith(ops::ArithAssignOp::Mul),
        py::Operator::Div => ops::AssignOp::Arith(ops::ArithAssignOp::Div),
        py::Operator::FloorDiv => ops::AssignOp::Arith(ops::ArithAssignOp::Flr),
        py::Operator::Mod => ops::AssignOp::Arith(ops::ArithAssignOp::Mod),
        py::Operator::BitAnd => ops::AssignOp::Bitwise(ops::BitwiseAssignOp::And),
        py::Operator::BitOr => ops::AssignOp::Bitwise(ops::BitwiseAssignOp::Or),
        py::Operator::BitXor => ops::AssignOp::Bitwise(ops::BitwiseAssignOp::Xor),
        py::Operator::LShift => ops::AssignOp::Bitwise(ops::BitwiseAssignOp::Shl),
        py::Operator::RShift => ops::AssignOp::Bitwise(ops::BitwiseAssignOp::Shr),
        py::Operator::Pow => return None,
        py::Operator::MatMult => return None,
    })
}

fn compare_op(op: py::CmpOp) -> Option<ops::BinaryOp> {
    Some(match op {
        py::CmpOp::Eq => ops::BinaryOp::Cmp(ops::CmpOp::Eq),
        py::CmpOp::NotEq => ops::BinaryOp::Cmp(ops::CmpOp::Ne),
        py::CmpOp::Lt => ops::BinaryOp::Cmp(ops::CmpOp::Lt),
        py::CmpOp::LtE => ops::BinaryOp::Cmp(ops::CmpOp::Le),
        py::CmpOp::Gt => ops::BinaryOp::Cmp(ops::CmpOp::Gt),
        py::CmpOp::GtE => ops::BinaryOp::Cmp(ops::CmpOp::Ge),
        py::CmpOp::In => ops::BinaryOp::MemberOp(ops::MemberOp::In),
        py::CmpOp::NotIn => ops::BinaryOp::MemberOp(ops::MemberOp::NotIn),
        py::CmpOp::Is => return None,
        py::CmpOp::IsNot => return None,
    })
}
