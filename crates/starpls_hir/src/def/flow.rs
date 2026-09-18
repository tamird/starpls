//! Starlark control flow, with reaching bindings and constraint algebra supplied by Ty.

use either::Either;
use rustc_hash::FxHashMap;
use starpls_common::File;
use ty_flow::bindings::Bindings;
use ty_flow::bindings::FutureDefinitions;
use ty_flow::bindings::PreviousDefinitions;
use ty_flow::bindings::ScopedDefinitionId;
use ty_flow::narrowing_constraints::NarrowingConstraintsBuilder;
use ty_flow::predicate::ScopedPredicateId;
use ty_flow::reachability_constraints::ReachabilityConstraints;
use ty_flow::reachability_constraints::ReachabilityConstraintsBuilder;
use ty_flow::reachability_constraints::ScopedReachabilityConstraintId as Constraint;

use crate::def::ops::BinaryOp;
use crate::def::ops::LogicOp;
use crate::def::scope::module_scopes;
use crate::def::scope::ExecutionScopeId;
use crate::def::scope::Scopes;
use crate::def::CompClause;
use crate::def::Expr;
use crate::def::Stmt;
use crate::def::StmtId;
use crate::lower;
use crate::Db;
use crate::ExprId;
use crate::Module;
use crate::Name;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Predicate {
    /// Each occurrence is independent: we do not yet narrow types on conditions.
    Branch,
    /// Literal truthiness selects a short-circuit or conditional-expression arm.
    Truthiness(ExprId),
    /// A call continues unless Starpls infers its result as Never.
    CallReturns(ExprId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BindingSource {
    Unbound,
    /// Preserve the existing conservative analysis across a loop back edge.
    Loop,
    Assignment {
        target: ExprId,
        source: ExprId,
        execution_scope: ExecutionScopeId,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FlowIndex {
    pub(crate) uses: FxHashMap<ExprId, Bindings>,
    pub(crate) statements: FxHashMap<StmtId, Constraint>,
    pub(crate) definitions: Vec<BindingSource>,
    pub(crate) predicates: Vec<Predicate>,
    pub(crate) constraints: ReachabilityConstraints,
}

type Place = (ExecutionScopeId, Name);

#[derive(Clone)]
struct State {
    reachability: Constraint,
    places: FxHashMap<Place, Bindings>,
    /// State for names not yet encountered, including outer-scope references.
    initial: Bindings,
}

impl Default for State {
    fn default() -> Self {
        Self {
            reachability: Constraint::ALWAYS_TRUE,
            places: FxHashMap::default(),
            initial: Bindings::unbound(Constraint::ALWAYS_TRUE),
        }
    }
}

struct Builder<'a> {
    module: &'a Module,
    scopes: &'a Scopes,
    state: State,
    uses: FxHashMap<ExprId, Bindings>,
    statements: FxHashMap<StmtId, Constraint>,
    definitions: Vec<BindingSource>,
    predicates: Vec<Predicate>,
    reachability: ReachabilityConstraintsBuilder,
    narrowing: NarrowingConstraintsBuilder,
}

impl Builder<'_> {
    fn predicate(&mut self, predicate: Predicate) -> Constraint {
        let id = ScopedPredicateId::from_u32(self.predicates.len().try_into().unwrap());
        self.predicates.push(predicate);
        self.reachability.add_atom(id)
    }

    fn restrict(&mut self, constraint: Constraint) {
        self.state.reachability = self
            .reachability
            .add_and_constraint(self.state.reachability, constraint);
    }

    fn merge(&mut self, mut other: State) {
        // Materialize pending path restrictions only at joins and uses. Calls in
        // straight-line code must not revisit every known place.
        for state in [&mut self.state, &mut other] {
            state
                .initial
                .record_reachability_constraint(&mut self.reachability, state.reachability);
            for bindings in state.places.values_mut() {
                bindings.record_reachability_constraint(&mut self.reachability, state.reachability);
            }
        }
        let State {
            reachability,
            mut places,
            initial,
        } = other;
        for (place, bindings) in &mut self.state.places {
            let other = places.remove(place).unwrap_or_else(|| initial.clone());
            bindings.merge(other, &mut self.narrowing, &mut self.reachability);
        }
        for (place, other) in places {
            let mut bindings = self.state.initial.clone();
            bindings.merge(other, &mut self.narrowing, &mut self.reachability);
            self.state.places.insert(place, bindings);
        }
        self.state
            .initial
            .merge(initial, &mut self.narrowing, &mut self.reachability);
        self.state.reachability = self
            .reachability
            .add_or_constraint(self.state.reachability, reachability);
    }

    /// Split an occurrence, without assuming repeated tests have the same value.
    fn branch(&mut self, predicate: Predicate) -> State {
        let condition = self.predicate(predicate);
        let before = self.state.clone();
        self.restrict(condition);
        let positive = std::mem::replace(&mut self.state, before);
        let negative = self.reachability.add_not_constraint(condition);
        self.restrict(negative);
        std::mem::replace(&mut self.state, positive)
    }

    fn statements(&mut self, stmts: &[StmtId]) {
        for stmt in stmts {
            self.statements.insert(*stmt, self.state.reachability);
            // Continue recording statement ranges, but keep syntactically dead names Never.
            if self.state.reachability != Constraint::ALWAYS_FALSE {
                self.statement(*stmt);
            }
        }
    }

    fn statement(&mut self, stmt: StmtId) {
        match &self.module[stmt] {
            Stmt::Assign {
                lhs,
                rhs,
                op: _,
                type_ref: _,
            } => {
                self.expression(*rhs);
                self.assignment(*lhs, *rhs);
            }
            Stmt::Def { func: _, stmts } => {
                let outer = std::mem::take(&mut self.state);
                self.statements(stmts);
                self.state = outer;
            }
            Stmt::If {
                test,
                if_stmts,
                elif_or_else_stmts,
            } => {
                self.expression(*test);
                let negative = self.branch(Predicate::Branch);
                self.statements(if_stmts);
                let positive = std::mem::replace(&mut self.state, negative);
                if let Some(stmts) = elif_or_else_stmts {
                    match stmts {
                        Either::Left(stmt) => self.statements(&[*stmt]),
                        Either::Right(stmts) => self.statements(stmts),
                    }
                }
                self.merge(positive);
            }
            Stmt::Return { expr } => {
                if let Some(expr) = expr {
                    self.expression(*expr);
                }
                self.restrict(Constraint::ALWAYS_FALSE);
            }
            Stmt::Expr { expr } => self.expression(*expr),
            Stmt::For {
                iterable,
                targets,
                stmts,
            } => {
                self.expression(*iterable);
                for target in targets {
                    self.assignment(*target, *iterable);
                }
                // The old analysis falls back to the effective type at every loop header.
                // A synthetic binding expresses that policy; later assignments shadow it.
                let mut initial = Bindings::default();
                initial.record_binding(
                    ScopedDefinitionId::from_u32(1),
                    self.state.reachability,
                    PreviousDefinitions::AreShadowed,
                    FutureDefinitions::ShadowThisOne,
                );
                self.state.places.clear();
                self.state.initial = initial;
                let after = self.state.clone();
                self.statements(stmts);
                // Include zero iterations and arbitrary back edges conservatively. Breaks
                // and continues do not establish a definite assignment after the loop.
                self.state = after;
            }
            Stmt::Break => self.restrict(Constraint::ALWAYS_FALSE),
            Stmt::Continue => self.restrict(Constraint::ALWAYS_FALSE),
            Stmt::Pass => {}
            Stmt::Load {
                load_stmt: _,
                items: _,
            } => {}
        }
    }

    fn expression(&mut self, expr: ExprId) {
        match &self.module[expr] {
            Expr::Name { name } => {
                let scope = self.scopes.execution_scope_for_hir_id(expr).unwrap();
                let bindings = self
                    .state
                    .places
                    .get(&(scope, name.clone()))
                    .unwrap_or(&self.state.initial);
                let mut bindings = bindings.clone();
                bindings.record_reachability_constraint(
                    &mut self.reachability,
                    self.state.reachability,
                );
                self.uses.insert(expr, bindings);
            }
            Expr::If {
                if_expr,
                test,
                else_expr,
            } => {
                self.expression(*test);
                let negative = self.branch(Predicate::Truthiness(*test));
                self.expression(*if_expr);
                let positive = std::mem::replace(&mut self.state, negative);
                self.expression(*else_expr);
                self.merge(positive);
            }
            Expr::Binary { lhs, rhs, op } => {
                self.expression(*lhs);
                let logic = op.as_ref().and_then(|op| match op {
                    BinaryOp::Logic(op) => Some(op),
                    _ => None,
                });
                if let Some(logic) = logic {
                    let mut skipped = self.branch(Predicate::Truthiness(*lhs));
                    if *logic == LogicOp::Or {
                        std::mem::swap(&mut self.state, &mut skipped);
                    }
                    self.expression(*rhs);
                    self.merge(skipped);
                } else {
                    self.expression(*rhs);
                }
            }
            Expr::Lambda { func: _, body } => {
                let outer = std::mem::take(&mut self.state);
                self.expression(*body);
                self.state = outer;
            }
            Expr::ListComp { expr, comp_clauses } => {
                let skipped = self.clauses(comp_clauses);
                self.expression(*expr);
                self.merge(skipped);
            }
            Expr::DictComp {
                entry,
                comp_clauses,
            } => {
                let skipped = self.clauses(comp_clauses);
                self.expression(entry.key);
                self.expression(entry.value);
                self.merge(skipped);
            }
            node @ Expr::Call { callee: _, args: _ } => {
                node.walk_child_exprs(|expr| self.expression(expr));
                let returns = self.predicate(Predicate::CallReturns(expr));
                self.restrict(returns);
            }
            node => node.walk_child_exprs(|expr| self.expression(expr)),
        }
    }

    fn assignment(&mut self, expr: ExprId, source: ExprId) {
        match &self.module[expr] {
            Expr::Name { name } => {
                let execution_scope = self.scopes.execution_scope_for_hir_id(expr).unwrap();
                let id = ScopedDefinitionId::from_u32(self.definitions.len().try_into().unwrap());
                self.definitions.push(BindingSource::Assignment {
                    target: expr,
                    source,
                    execution_scope,
                });
                let bindings = self
                    .state
                    .places
                    .entry((execution_scope, name.clone()))
                    .or_insert_with(|| self.state.initial.clone());
                bindings.record_binding(
                    id,
                    self.state.reachability,
                    PreviousDefinitions::AreShadowed,
                    FutureDefinitions::ShadowThisOne,
                );
                // Inspecting the assignment itself still shows its type after a
                // non-returning call. Subsequent reads respect reachability.
                let mut assigned = Bindings::default();
                assigned.record_binding(
                    id,
                    Constraint::ALWAYS_TRUE,
                    PreviousDefinitions::AreShadowed,
                    FutureDefinitions::ShadowThisOne,
                );
                self.uses.insert(expr, assigned);
            }
            Expr::Paren { expr } => self.assignment(*expr, source),
            Expr::Tuple { exprs } => {
                for expr in exprs {
                    self.assignment(*expr, source);
                }
            }
            Expr::List { exprs } => {
                for expr in exprs {
                    self.assignment(*expr, source);
                }
            }
            node => node.walk_child_exprs(|expr| self.expression(expr)),
        }
    }

    fn clauses(&mut self, clauses: &[CompClause]) -> State {
        let mut skipped = None;
        for clause in clauses {
            match clause {
                CompClause::For { iterable, targets } => {
                    self.expression(*iterable);
                    // The outermost iterable is evaluated even for an empty
                    // comprehension. Only its iterations may be skipped.
                    if skipped.is_none() {
                        skipped = Some(self.branch(Predicate::Branch));
                    }
                    for target in targets {
                        self.assignment(*target, *iterable);
                    }
                }
                CompClause::If { test } => self.expression(*test),
            }
        }
        skipped.unwrap_or_else(|| self.state.clone())
    }

    fn finish(mut self) -> FlowIndex {
        for bindings in self.uses.values_mut() {
            bindings.finish(&mut self.narrowing, &mut self.reachability);
        }
        for constraint in self.statements.values() {
            self.reachability.mark_used(*constraint);
        }
        FlowIndex {
            uses: self.uses,
            statements: self.statements,
            definitions: self.definitions,
            predicates: self.predicates,
            constraints: self.reachability.build(),
        }
    }
}

pub(crate) fn flow_index(db: &dyn Db, file: File) -> &FlowIndex {
    let File {
        source,
        dialect,
        info,
    } = file;
    flow_index_query(db, source, (dialect, info))
}

#[salsa::tracked(returns(ref))]
pub(crate) fn flow_index_query(
    db: &dyn Db,
    source: ruff_db::files::File,
    context: (starpls_common::Dialect, Option<starpls_common::FileInfo>),
) -> FlowIndex {
    let (dialect, info) = context;
    let file = File {
        source,
        dialect,
        info,
    };
    let info = lower(db, file);
    let scopes = module_scopes(db, file);
    let module = &info.module;
    let mut builder = Builder {
        module,
        scopes,
        state: State::default(),
        uses: FxHashMap::default(),
        statements: FxHashMap::default(),
        definitions: vec![BindingSource::Unbound, BindingSource::Loop],
        predicates: Vec::new(),
        reachability: ReachabilityConstraintsBuilder::default(),
        narrowing: NarrowingConstraintsBuilder::default(),
    };
    builder.statements(&module.top_level);
    builder.finish()
}
