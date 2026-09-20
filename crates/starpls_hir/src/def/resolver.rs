use std::collections::hash_map::Entry;
use std::iter;

use rustc_hash::FxHashMap;
use starpls_bazel::APIContext;
use starpls_common::File;

use crate::def::scope::module_scopes;
use crate::def::scope::ExecutionScopeId;
use crate::def::scope::FunctionDef;
use crate::def::scope::Scope;
use crate::def::scope::ScopeDef;
use crate::def::scope::ScopeHirId;
use crate::def::scope::ScopeId;
use crate::def::scope::Scopes;
use crate::def::scope::VariableDef;
use crate::def::ExprId;
use crate::typeck::builtins::builtin_globals;
use crate::typeck::builtins::APIGlobals;
use crate::typeck::intrinsics::intrinsic_functions;
use crate::Db;
use crate::Name;

/// Resolves things like variables, function definition, etc. For now this is implemented as a simple list
/// of "module" scopes that hold variable declarations, but will need to be updated later to support other
/// features, e.g. type declarations, builtins, etc.
pub(crate) struct Resolver<'a> {
    db: &'a dyn Db,
    file: File,
    scopes: &'a Scopes,
    scope_chain: Vec<ScopeId>,
}

#[derive(Clone, Debug)]
pub(crate) enum Export {
    Variable(VariableDef),
    Function(FunctionDef),
}

impl From<Export> for ScopeDef {
    fn from(value: Export) -> Self {
        match value {
            Export::Variable(def) => ScopeDef::Variable(def),
            Export::Function(func) => ScopeDef::Function(func),
        }
    }
}

impl<'a> Resolver<'a> {
    pub(crate) fn resolve_export_in_file(
        db: &'a dyn Db,
        file: File,
        name: &Name,
    ) -> Option<Export> {
        Self::new_for_module(db, file).resolve_export(name)
    }

    fn resolve_export(&self, name: &Name) -> Option<Export> {
        if name.as_str().starts_with('_') {
            return None;
        }

        self.scopes().find_map(|scope| {
            scope
                .defs
                .get(name)
                .and_then(|defs| defs.last())
                .and_then(|def| {
                    Some(match def {
                        ScopeDef::Variable(def) => Export::Variable(def.clone()),
                        ScopeDef::Function(def) => Export::Function(def.clone()),
                        _ => return None,
                    })
                })
        })
    }

    fn resolve_name_from_prelude(&self, name: &Name) -> Option<ScopeDef> {
        self.scopes().find_map(|scope| {
            scope
                .defs
                .get(name)
                .and_then(|defs| defs.last())
                .and_then(|def| match def {
                    ScopeDef::Variable(_) | ScopeDef::Function(_) | ScopeDef::LoadItem(_) => {
                        Some(def.clone())
                    }
                    _ => None,
                })
        })
    }

    pub(crate) fn resolve_name(
        &'a self,
        name: &'a Name,
    ) -> Option<(ExecutionScopeId, impl Iterator<Item = SymbolDef<'a>> + 'a)> {
        let mut defs = self
            .scopes_with_id()
            .filter_map(move |(scope_id, scope)| {
                scope
                    .defs
                    .get(name)
                    .map(|defs| (scope_id, scope.execution_scope, defs))
            })
            .flat_map(|(scope, execution_scope, defs)| {
                defs.iter().map(move |def| SymbolDef {
                    scope,
                    execution_scope,
                    def,
                })
            });
        let first = defs.next()?;
        let first_execution_scope = first.execution_scope;
        let defs = iter::once(first)
            .chain(defs.take_while(move |def| def.execution_scope == first_execution_scope));
        Some((first_execution_scope, defs))
    }

    pub(crate) fn resolve_name_in_prelude_or_builtins(&self, name: &Name) -> Option<ScopeDef> {
        let mut def = None;

        // Check prelude if this is a BUILD file.
        if self.file.api_context() == Some(APIContext::Build) {
            def = self
                .db
                .get_bazel_prelude_file()
                .and_then(|prelude_file_id| {
                    let prelude_file = prelude_file_id;
                    Self::new_for_module(self.db, prelude_file).resolve_name_from_prelude(name)
                })
        }

        // Otherwise, check the builtins scope.
        def.or_else(|| {
            intrinsic_functions(self.db)
                .functions
                .get(name)
                .cloned()
                .map(ScopeDef::IntrinsicFunction)
        })
        .or_else(|| self.resolve_name_in_builtin_globals(name))
    }

    fn resolve_name_in_builtin_globals(&self, name: &Name) -> Option<ScopeDef> {
        let api_context = self.file.api_context()?;
        let globals = builtin_globals(self.db, self.file.dialect);
        let resolve_in_api_globals = |api_globals: &APIGlobals| {
            api_globals
                .functions
                .get(name.as_str())
                .cloned()
                .map(ScopeDef::BuiltinFunction)
                .or_else(|| {
                    api_globals
                        .variables
                        .get(name.as_str())
                        .cloned()
                        .map(ScopeDef::BuiltinVariable)
                })
        };

        if api_context == APIContext::Repo {
            return resolve_in_api_globals(&globals.repo_globals);
        }
        if api_context == APIContext::Cquery {
            return resolve_in_api_globals(&globals.cquery_globals);
        }
        resolve_in_api_globals(&globals.bzl_globals).or_else(|| match api_context {
            APIContext::Module => resolve_in_api_globals(&globals.bzlmod_globals),
            APIContext::Workspace => resolve_in_api_globals(&globals.workspace_globals),
            _ => None,
        })
    }

    pub(crate) fn module_defs(&self, filter_unexported: bool) -> FxHashMap<Name, ScopeDef> {
        let mut names = FxHashMap::default();
        for scope in self.scopes() {
            for (name, def) in scope.defs.iter() {
                if (filter_unexported && name.as_str().starts_with('_')) || name.is_missing() {
                    continue;
                }
                if let Entry::Vacant(entry) = names.entry(name.clone()) {
                    if let Some(def) = def.first().cloned() {
                        entry.insert(def);
                    }
                }
            }
        }
        names
    }

    fn scopes(&self) -> impl Iterator<Item = &Scope> {
        self.scope_chain
            .iter()
            .rev()
            .map(|scope| &self.scopes.scopes[*scope])
    }

    fn scopes_with_id(&self) -> impl Iterator<Item = (ScopeId, &Scope)> {
        self.scope_chain
            .iter()
            .rev()
            .map(|scope| (*scope, &self.scopes.scopes[*scope]))
    }

    pub(crate) fn new_for_module(db: &'a dyn Db, file: File) -> Self {
        let scopes = module_scopes(db, file);
        let scope = scopes.scope_for_hir_id(ScopeHirId::Module);
        Self::from_parts(db, file, scopes, scope)
    }

    pub(crate) fn new_for_expr(db: &'a dyn Db, file: File, expr: ExprId) -> Self {
        let scopes = module_scopes(db, file);
        let scope = scopes.scope_for_hir_id(expr);
        Self::from_parts(db, file, scopes, scope)
    }

    pub(crate) fn new_for_hir_execution_scope(
        db: &'a dyn Db,
        file: File,
        hir: impl Into<ScopeHirId>,
    ) -> Self {
        let scopes = module_scopes(db, file);
        let scope = scopes.scope_for_hir_execution_scope(hir);
        Self::from_parts(db, file, scopes, scope)
    }

    pub(crate) fn scope_for_hir_id(&self, hir: impl Into<ScopeHirId>) -> Option<ScopeId> {
        self.scopes.scope_for_hir_id(hir)
    }

    pub(crate) fn execution_scope_for_hir_id(
        &self,
        hir: impl Into<ScopeHirId>,
    ) -> Option<ExecutionScopeId> {
        self.scopes.execution_scope_for_hir_id(hir)
    }

    fn from_parts(db: &'a dyn Db, file: File, scopes: &'a Scopes, scope: Option<ScopeId>) -> Self {
        let mut scope_chain = scopes.scope_chain(scope).collect::<Vec<_>>();
        scope_chain.reverse();
        Self {
            db,
            file,
            scopes,
            scope_chain,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SymbolDef<'a> {
    pub(crate) scope: ScopeId,
    pub(crate) execution_scope: ExecutionScopeId,
    pub(crate) def: &'a ScopeDef,
}
