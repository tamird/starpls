use std::sync::Arc;

use def::resolver::Resolver;
use def::scope;
use def::scope::module_scopes_query;
use def::scope::FunctionDef;
use def::scope::ParameterDef;
use def::Function;
use def::LoadItemId;
use def::Stmt;
use smallvec::SmallVec;
use starpls_bazel::Builtins;
use starpls_common::parse;
use starpls_common::Diagnostic;
use starpls_common::Diagnostics;
use starpls_common::Dialect;
use starpls_common::File;
use starpls_common::InFile;
use starpls_common::Parse;
use starpls_syntax::ast;
use starpls_syntax::ast::AstNode;
use starpls_syntax::ast::AstPtr;
use starpls_syntax::TextRange;
use starpls_syntax::TextSize;
use starpls_syntax::T;
use typeck::builtins::BuiltinFunction;
use typeck::intrinsics::IntrinsicFunction;
use typeck::queries;
use typeck::Field;
use typeck::FieldInner;
use typeck::Macro;
use typeck::Provider;
use typeck::Rule;
use typeck::RuleParam;
use typeck::Substitution;
use typeck::TagClass;
use typeck::TagParam;
use typeck::Tuple;

use crate::def::Argument;
use crate::def::AssignmentSource;
use crate::def::Expr;
use crate::def::ExprId;
use crate::def::Literal;
use crate::def::Module;
use crate::def::ModuleSourceMap;
pub use crate::def::Name;
pub use crate::test_database::Fixture;
pub use crate::typeck::builtins::BuiltinDefs;
pub use crate::typeck::queries::diagnostics as inference_diagnostics;
pub use crate::typeck::Cancelled;
pub use crate::typeck::InferenceOptions;
pub(crate) use crate::typeck::Ty;
use crate::typeck::TyKind;
use crate::typeck::TypeRef;

mod def;
mod display;
mod test_database;
mod typeck;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ModuleInfo {
    pub(crate) file: File,
    pub(crate) module: Module,
    pub(crate) source_map: ModuleSourceMap,
}

/// Documentation for the `Target` type defined by Bazel.
/// TODO(withered-magic): Find a better place to put this.
const TARGET_DOC: &str = "The BUILD target for a dependency. Appears in the fields of `ctx.attr` corresponding to dependency attributes (`label` or `label_list`).";

#[salsa::db]
pub trait Db: starpls_common::Db {
    fn environment(&self) -> Environment;

    fn set_builtin_defs(&mut self, dialect: Dialect, builtins: Builtins, rules: Builtins);

    fn get_builtin_defs(&self, dialect: &Dialect) -> BuiltinDefs;

    fn set_bazel_prelude_file(&mut self, file_id: File);
    fn get_bazel_prelude_file(&self) -> Option<File>;
    fn set_all_workspace_targets(&mut self, targets: Vec<String>);
    fn get_all_workspace_targets(&self) -> Arc<Vec<String>>;
}

/// Inputs shared by semantic queries. Input identities remain stable when the
/// host changes configuration or discovers previously unavailable modules.
#[salsa::input(debug)]
pub struct Environment {
    #[returns(ref)]
    pub options: InferenceOptions,
    #[returns(clone)]
    pub standard_builtins: BuiltinDefs,
    #[returns(clone)]
    pub bazel_builtins: BuiltinDefs,
    #[returns(clone)]
    pub prelude_file: Option<File>,
    #[returns(clone)]
    pub all_workspace_targets: Arc<Vec<String>>,
    #[returns(clone)]
    pub load_revision: u64,
}

impl Environment {
    pub fn initialize(db: &dyn Db, options: InferenceOptions) -> Self {
        let standard = BuiltinDefs::new(db, Builtins::default(), Builtins::default());
        let bazel = BuiltinDefs::new(db, Builtins::default(), Builtins::default());
        Self::new(db, options, standard, bazel, None, Arc::default(), 0)
    }

    pub fn builtin_defs(self, db: &dyn Db, dialect: Dialect) -> BuiltinDefs {
        match dialect {
            Dialect::Standard => self.standard_builtins(db),
            Dialect::Bazel => self.bazel_builtins(db),
        }
    }
}

/// Return the diagnostics accumulated by Salsa queries on the given file.
/// This does not include diagnostics from type inference, which are reported
/// by [`inference_diagnostics`] instead.
pub fn diagnostics_for_file(db: &dyn Db, file: File) -> impl Iterator<Item = Diagnostic> + '_ {
    module_scopes_query::accumulated::<Diagnostics>(db, file.source, (file.dialect, file.info))
        .into_iter()
        .map(|diagnostic| diagnostic.0.clone())
}

/// Semantic views for one database revision.
///
/// Types and definitions borrow this database and use it for every lookup, so
/// their revision-local IDs cannot survive an edit or be queried in another
/// database. Editor requests convert these views into owned response data.
///
/// Materialize response data before editing the database:
///
/// ```no_run
/// use starpls_common::File;
/// use starpls_hir::{Db, Semantics};
///
/// fn inspect_then_edit(db: &mut dyn Db, file: File) -> String {
///     let (_, definition) = Semantics::new(db).scope_for_module(file).exports().next().unwrap();
///     let result = definition.ty().to_string();
///     starpls_common::update_file(db, file, String::new());
///     result
/// }
/// ```
///
/// Keeping a definition in use across an edit is rejected:
///
/// ```compile_fail,E0502
/// use starpls_common::File;
/// use starpls_hir::{Db, Semantics};
///
/// fn edit_then_inspect(db: &mut dyn Db, file: File) -> String {
///     let (_, definition) = Semantics::new(db).scope_for_module(file).exports().next().unwrap();
///     starpls_common::update_file(db, file, String::new());
///     definition.ty().to_string()
/// }
/// ```
#[derive(Clone, Copy)]
pub struct Semantics<'a> {
    pub db: &'a dyn Db,
}

impl std::fmt::Debug for Semantics<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { db: _ } = self;
        f.debug_struct("Semantics").finish_non_exhaustive()
    }
}

impl PartialEq for Semantics<'_> {
    fn eq(&self, other: &Self) -> bool {
        let Self { db } = self;
        let Self { db: other } = other;
        std::ptr::addr_eq(*db, *other)
    }
}

impl Eq for Semantics<'_> {}

impl<'a> Semantics<'a> {
    pub fn new(db: &'a dyn Db) -> Self {
        Self { db }
    }

    pub fn parse(&self, file: File) -> &'a Parse {
        parse(self.db, file)
    }

    pub fn callable_for_def(&self, file: File, node: ast::DefStmt) -> Option<Callable<'a>> {
        let ptr = AstPtr::new(&ast::Statement::cast(node.syntax().clone())?);
        let stmt = source_map(self.db, file).stmt_map.get(&ptr)?;
        match &module(self.db, file)[*stmt] {
            Stmt::Def { func, .. } => Some(Callable::new(
                *self,
                CallableInner::HirDef(FunctionDef::Def {
                    func: func.clone(),
                    stmt: InFile { file, value: *stmt },
                }),
            )),
            _ => None,
        }
    }

    pub fn resolve_path_type(&self, file: File, node: &ast::PathType) -> Option<Type<'a>> {
        let usage = node
            .syntax()
            .ancestors()
            .find_map(ast::TypeComment::cast)
            .and_then(|type_comment| {
                let parent = type_comment.syntax().parent()?;
                let ptr = if ast::Suite::can_cast(parent.kind()) {
                    let grandparent = parent.parent()?;
                    if ast::DefStmt::can_cast(grandparent.kind()) {
                        AstPtr::new(&ast::Statement::cast(grandparent)?)
                    } else {
                        return None;
                    }
                } else {
                    let assign_stmt = type_comment
                        .syntax()
                        .siblings_with_tokens(ast::Direction::Prev)
                        .take_while(|el| !matches!(el.kind(), T!['\n'] | T![;]))
                        .filter_map(|el| el.into_node())
                        .find_map(ast::AssignStmt::cast)?;
                    AstPtr::new(&ast::Statement::Assign(assign_stmt))
                };

                let stmt = source_map(self.db, file).stmt_map.get(&ptr)?;
                Some(InFile { file, value: *stmt })
            });
        let segments = node
            .segments()
            .flat_map(|segment| segment.value())
            .map(|token| Name::from_str(token.text()))
            .collect::<SmallVec<_>>();
        Some(Type::new(
            *self,
            queries::resolve_type(self.db, TypeRef::Path(segments, None), usage),
        ))
    }

    pub fn resolve_call_expr(&self, file: File, expr: &ast::CallExpr) -> Option<Callable<'a>> {
        let ty = self.type_of_expr(file, &expr.callee()?)?;
        Some(match ty.ty.kind() {
            TyKind::Function(def) => Callable::new(*self, CallableInner::HirDef(def.clone())),
            TyKind::IntrinsicFunction(func, subst) => Callable::new(
                *self,
                CallableInner::IntrinsicFunction(func.clone(), Some(subst.clone())),
            ),
            TyKind::BuiltinFunction(func) => {
                Callable::new(*self, CallableInner::BuiltinFunction(func.clone()))
            }
            TyKind::Rule(rule) => Callable::new(*self, CallableInner::Rule(rule.clone())),
            TyKind::Provider(provider) => {
                Callable::new(*self, CallableInner::Provider(provider.clone()))
            }
            TyKind::ProviderRawConstructor(name, provider) => Callable::new(
                *self,
                CallableInner::ProviderRawConstructor(name.clone(), provider.clone()),
            ),
            TyKind::Tag(tag_class) => Callable::new(*self, CallableInner::Tag(tag_class.clone())),
            TyKind::Macro(makro) => Callable::new(*self, CallableInner::Macro(makro.clone())),
            _ => return None,
        })
    }

    pub fn resolve_def_stmt(&self, file: File, def_stmt: &ast::DefStmt) -> Option<Callable<'a>> {
        let module = module(self.db, file);
        let stmt = source_map(self.db, file)
            .stmt_map
            .get(&AstPtr::new(&ast::Statement::Def(def_stmt.clone())))?;
        let Stmt::Def { ref func, .. } = module[*stmt] else {
            return None;
        };
        Some(Callable::new(
            *self,
            CallableInner::HirDef(FunctionDef::Def {
                func: func.clone(),
                stmt: InFile { file, value: *stmt },
            }),
        ))
    }

    pub fn type_of_expr(&self, file: File, expr: &ast::Expression) -> Option<Type<'a>> {
        let ptr = AstPtr::new(expr);
        let expr = source_map(self.db, file).expr_map.get(&ptr)?;
        Some(Type::new(*self, queries::infer_expr(self.db, file, *expr)))
    }

    pub fn resolve_param(
        &self,
        file: File,
        param: &ast::Parameter,
    ) -> Option<(Param<'a>, Type<'a>)> {
        let module = module(self.db, file);
        let param = source_map(self.db, file)
            .param_map
            .get(&AstPtr::new(param))?;
        let (func, index) = module
            .param_to_def_stmt
            .get(param)
            .and_then(|(stmt, index)| match module[*stmt] {
                Stmt::Def { ref func, .. } => Some((func.clone(), index)),
                _ => None,
            })?;
        Some((
            Param::new(
                *self,
                ParamInner::Param {
                    func,
                    index: *index,
                },
            ),
            Type::new(*self, queries::infer_param(self.db, file, *param)),
        ))
    }

    pub fn resolve_load_stmt(&self, file: File, load_stmt: &ast::LoadStmt) -> Option<File> {
        let ptr = AstPtr::new(&ast::Statement::Load(load_stmt.clone()));
        let stmt = source_map(self.db, file).stmt_map.get(&ptr)?;
        let load_stmt = match module(self.db, file)[*stmt] {
            Stmt::Load { ref load_stmt, .. } => load_stmt.clone(),
            _ => return None,
        };
        queries::resolve_load_stmt(self.db, file, load_stmt)
    }

    pub fn resolve_load_item(&self, file: File, load_item: &ast::LoadItem) -> Option<LoadItem<'a>> {
        let ptr = AstPtr::new(load_item);
        let load_item = source_map(self.db, file).load_item_map.get(&ptr)?;
        Some(LoadItem {
            sema: *self,
            id: InFile {
                file,
                value: *load_item,
            },
        })
    }

    pub fn scope_for_module(&self, file: File) -> SemanticsScope<'a> {
        let resolver = Resolver::new_for_module(self.db, file);
        SemanticsScope {
            sema: *self,
            resolver,
        }
    }

    pub fn scope_for_expr(&self, file: File, expr: &ast::Expression) -> Option<SemanticsScope<'a>> {
        let ptr = AstPtr::new(expr);
        let expr = source_map(self.db, file).expr_map.get(&ptr)?;
        let resolver = Resolver::new_for_expr(self.db, file, *expr);
        Some(SemanticsScope {
            sema: *self,
            resolver,
        })
    }

    pub fn scope_for_offset(&self, file: File, offset: TextSize) -> SemanticsScope<'a> {
        let resolver = Resolver::new_for_offset(self.db, file, offset);
        SemanticsScope {
            sema: *self,
            resolver,
        }
    }

    pub fn resolve_call_expr_active_param(
        &self,
        file: File,
        expr: &ast::CallExpr,
        active_arg: usize,
    ) -> Option<usize> {
        let ptr = AstPtr::new(&ast::Expression::Call(expr.clone()));
        let expr = source_map(self.db, file).expr_map.get(&ptr)?;
        queries::active_parameter(self.db, file, *expr, active_arg)
    }
}

pub struct SemanticsScope<'a> {
    sema: Semantics<'a>,
    resolver: Resolver<'a>,
}

impl<'a> SemanticsScope<'a> {
    pub fn names(&self) -> impl Iterator<Item = (Name, ScopeDef<'a>)> + 'a {
        let Self { sema, resolver } = self;
        let sema = *sema;
        resolver
            .names()
            .into_iter()
            .map(move |(name, def)| (name, ScopeDef::new(sema, def)))
    }

    pub fn exports(&self) -> impl Iterator<Item = (Name, ScopeDef<'a>)> + 'a {
        let Self { sema, resolver } = self;
        let sema = *sema;
        resolver
            .module_defs(true)
            .into_iter()
            .map(move |(name, def)| (name, ScopeDef::new(sema, def)))
    }

    pub fn resolve_name(&self, name: &Name) -> Vec<ScopeDef<'a>> {
        let mut defs: Vec<ScopeDef<'a>> = match self.resolver.resolve_name(name) {
            Some((_, defs)) => defs
                .map(|def| ScopeDef::new(self.sema, def.def.clone()))
                .collect(),
            None => Vec::new(),
        };
        if defs.is_empty() {
            if let Some(def) = self.resolver.resolve_name_in_prelude_or_builtins(name) {
                defs.push(ScopeDef::new(self.sema, def));
            }
        }
        defs
    }
}

/// A type. Mostly serves as a public API for [`typeck::Ty`].
#[derive(Clone, Debug)]
pub struct Type<'a> {
    sema: Semantics<'a>,
    pub(crate) ty: Ty,
}

impl<'a> Type<'a> {
    fn new(sema: Semantics<'a>, ty: Ty) -> Self {
        Self { sema, ty }
    }

    pub fn is_function(&self) -> bool {
        let Self { sema: _, ty } = self;
        matches!(
            ty.kind(),
            TyKind::Function(_) | TyKind::BuiltinFunction(_) | TyKind::IntrinsicFunction(_, _)
        )
    }

    pub fn is_callable(&self) -> bool {
        let Self { sema: _, ty } = self;
        self.is_function()
            || matches!(
                ty.kind(),
                TyKind::Rule(_)
                    | TyKind::Provider(_)
                    | TyKind::ProviderRawConstructor(_, _)
                    | TyKind::Tag(_)
                    | TyKind::Macro(_)
            )
    }

    pub fn is_unknown(&self) -> bool {
        let Self { sema: _, ty } = self;
        ty.kind() == &TyKind::Unknown
    }

    pub fn is_user_defined_function(&self) -> bool {
        let Self { sema: _, ty } = self;
        matches!(ty.kind(), TyKind::Function(_))
    }

    pub fn params(&self) -> Vec<(Param<'a>, Type<'a>)> {
        let Self { sema, ty } = self;
        let db = sema.db;
        match ty.params(db) {
            Some(params) => params
                .map(|(param, ty)| (Param::new(*sema, param), Type::new(*sema, ty)))
                .collect(),
            None => Vec::new(),
        }
    }

    pub fn doc(&self) -> Option<String> {
        let Self { sema: _, ty } = self;
        match ty.kind() {
            TyKind::BuiltinFunction(func) => Some(func.doc.clone()),
            TyKind::BuiltinType(ty, _) => Some(ty.doc.clone()),
            TyKind::Function(def) => def.func().doc.as_ref().map(|doc| doc.to_string()),
            TyKind::IntrinsicFunction(func, _) => Some(func.doc.clone()),
            TyKind::Rule(rule) => rule.doc.as_ref().map(Box::to_string),
            TyKind::Provider(provider) | TyKind::ProviderInstance(provider) => provider.doc(),
            TyKind::ModuleExtension(module_extension)
            | TyKind::ModuleExtensionProxy(module_extension) => {
                module_extension.doc.as_ref().map(Box::to_string)
            }
            TyKind::Target => Some(TARGET_DOC.into()),
            TyKind::Macro(makro) => makro.doc.as_ref().map(|doc| doc.as_ref().to_string()),
            _ => None,
        }
    }

    pub fn fields(&self) -> Vec<(Field, Type<'a>)> {
        let Self { sema, ty } = self;
        let db = sema.db;
        let fields = match ty.fields(db) {
            Some(fields) => fields,
            None => return Vec::new(),
        };

        let mut fields = fields
            .map(|(name, ty)| (name, Type::new(*sema, ty)))
            .collect::<Vec<_>>();

        // TODO(withered-magic): This ideally should be handled in `Ty::fields()` instead.
        if let TyKind::Struct(Some(typeck::Struct::RuleAttributes { rule_kind, attrs })) = ty.kind()
        {
            fields.extend(attrs.attrs.iter().filter_map(|(name, attr)| {
                attr.as_ref().map(|attr| {
                    (
                        Field(FieldInner::StructField {
                            name: name.clone(),
                            doc: attr.doc.as_ref().map(|doc| doc.as_ref().to_string()),
                        }),
                        Type::new(*sema, attr.resolved_ty(rule_kind)),
                    )
                })
            }));
        }

        fields
    }

    /// The original declaration of a struct or provider field, when known.
    pub fn field_definition(&self, name: &str) -> Option<InFile<TextRange>> {
        let Self { sema, ty } = self;
        match ty.kind() {
            TyKind::Struct(strukt) => {
                let typeck::Struct::Inline {
                    fields: _,
                    call_expr,
                } = strukt.as_ref()?
                else {
                    return None;
                };
                let InFile { file, value } = *call_expr;
                let Expr::Call { callee: _, args } = &module(sema.db, file)[value] else {
                    return None;
                };
                args.iter().find_map(|arg| {
                    let Argument::Keyword {
                        name: keyword,
                        expr,
                    } = arg
                    else {
                        return None;
                    };
                    if keyword.as_str() != name {
                        return None;
                    }
                    let range = *source_map(sema.db, file).keyword_names.get(expr)?;
                    Some(InFile { file, value: range })
                })
            }
            TyKind::Provider(provider) => provider_field_definition(sema.db, provider, name),
            TyKind::ProviderInstance(provider) => {
                provider_field_definition(sema.db, provider, name)
            }
            _ => None,
        }
    }

    pub fn known_keys(&self) -> Option<Vec<String>> {
        let Self { sema: _, ty } = self;
        ty.known_keys().map(|known_keys| {
            known_keys
                .iter()
                .map(|(name, _)| name.as_ref().to_string())
                .collect()
        })
    }

    pub fn dict_value_ty(&self) -> Option<Type<'a>> {
        let Self { sema, ty } = self;
        match ty.kind() {
            TyKind::Dict(_, value_ty, _) => Some(Type::new(*sema, value_ty.clone())),
            _ => None,
        }
    }

    pub fn variable_tuple_element_ty(&self) -> Option<Type<'a>> {
        let Self { sema, ty } = self;
        match ty.kind() {
            TyKind::Tuple(Tuple::Variable(ty)) => Some(Type::new(*sema, ty.clone())),
            _ => None,
        }
    }
}

fn expr_source_range(db: &dyn Db, expr: InFile<ExprId>) -> Option<InFile<TextRange>> {
    let InFile { file, value } = expr;
    let ptr = source_map(db, file).expr_map_back.get(&value)?;
    Some(InFile {
        file,
        value: ptr.syntax_node_ptr().text_range(),
    })
}

fn provider_field_definition(
    db: &dyn Db,
    provider: &Provider,
    name: &str,
) -> Option<InFile<TextRange>> {
    let expr = match provider {
        Provider::Builtin(_) => return None,
        Provider::Custom(provider) => provider.fields.as_ref()?.expr?,
    };
    dict_key_definition(db, expr, name)
}

fn dict_key_definition(db: &dyn Db, expr: InFile<ExprId>, name: &str) -> Option<InFile<TextRange>> {
    let InFile { file, value } = expr;
    let module = module(db, file);
    let Expr::Dict { entries } = &module[value] else {
        return None;
    };
    entries.iter().find_map(|def::DictEntry { key, value: _ }| {
        let Expr::Literal { literal } = &module[*key] else {
            return None;
        };
        let Literal::String(key_name) = literal else {
            return None;
        };
        if key_name.as_ref() != name {
            return None;
        }
        expr_source_range(db, InFile { file, value: *key })
    })
}

/// A variable definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Variable<'a> {
    sema: Semantics<'a>,
    def: Option<scope::VariableDef>,
}

impl<'a> Variable<'a> {
    /// Whether the variable is user-defined; `false` in the case of
    /// variables from e.g. Bazel builtins.
    pub fn is_user_defined(&self) -> bool {
        let Self { sema: _, def } = self;
        def.is_some()
    }

    /// The load item directly assigned to this variable, if any.
    pub fn re_export(&self) -> Option<LoadItem<'a>> {
        let Self { sema, def } = self;
        let scope::VariableDef { file, expr, source } = def.as_ref()?;
        let source = (*source)?;
        let module = module(sema.db, *file);
        let AssignmentSource::Statement(stmt) = module.assignment_sources.get(&source)? else {
            return None;
        };
        let Stmt::Assign {
            lhs,
            rhs,
            op: _,
            type_ref: _,
        } = &module[*stmt]
        else {
            return None;
        };
        if lhs != expr {
            return None;
        }
        let Expr::Name { name } = &module[*rhs] else {
            return None;
        };
        let scope = SemanticsScope {
            sema: *sema,
            resolver: Resolver::new_for_expr(sema.db, *file, *rhs),
        };
        scope
            .resolve_name(name)
            .into_iter()
            .find_map(|def| match def {
                ScopeDef::LoadItem(item) => Some(item),
                _ => None,
            })
    }
}

/// A callable value, e.g. a function, rule, etc.
/// The actual data is stored in [`CallableInner`], which wraps some
/// crate-internal data types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Callable<'a> {
    sema: Semantics<'a>,
    inner: CallableInner,
}

impl<'a> Callable<'a> {
    fn new(sema: Semantics<'a>, inner: CallableInner) -> Self {
        Self { sema, inner }
    }

    pub fn name(&self) -> Name {
        let Self { sema: _, inner } = self;
        match *inner {
            CallableInner::HirDef(ref def) => def.func().name.clone(),
            CallableInner::IntrinsicFunction(ref func, _) => func.name.clone(),
            CallableInner::BuiltinFunction(ref func) => func.name.clone(),
            CallableInner::Rule(_) => Name::new_inline("rule"),
            CallableInner::Provider(ref provider) => provider
                .name()
                .cloned()
                .unwrap_or_else(|| Name::new_inline("provider")),
            CallableInner::ProviderRawConstructor(ref name, _) => name.clone(),
            CallableInner::Tag(_) => Name::new_inline("tag"),
            CallableInner::Macro(_) => Name::new_inline("macro"),
        }
    }

    pub fn params(&self) -> Vec<(Param<'a>, Type<'a>)> {
        let Self { sema: _, inner: _ } = self;
        self.ty().params()
    }

    pub fn ty(&self) -> Type<'a> {
        let Self { sema, inner } = self;
        let ty = match *inner {
            CallableInner::HirDef(ref def) => TyKind::Function(def.clone()).intern(),
            CallableInner::IntrinsicFunction(ref func, ref subst) => TyKind::IntrinsicFunction(
                func.clone(),
                subst
                    .clone()
                    .unwrap_or_else(|| Substitution::new_identity(func.num_vars)),
            )
            .intern(),
            CallableInner::BuiltinFunction(ref func) => {
                TyKind::BuiltinFunction(func.clone()).intern()
            }
            CallableInner::Rule(ref rule) => TyKind::Rule(rule.clone()).intern(),
            CallableInner::Provider(ref provider) => TyKind::Provider(provider.clone()).intern(),
            CallableInner::ProviderRawConstructor(ref name, ref provider) => {
                TyKind::ProviderRawConstructor(name.clone(), provider.clone()).intern()
            }
            CallableInner::Tag(ref tag_class) => TyKind::Tag(tag_class.clone()).intern(),
            CallableInner::Macro(ref makro) => TyKind::Macro(makro.clone()).intern(),
        };
        Type::new(*sema, ty)
    }

    pub fn ret_ty(&self) -> Type<'a> {
        let Self { sema, inner: _ } = self;
        let db = sema.db;
        let ty = self.ty().ty.ret_ty(db).expect("expected return type");
        Type::new(*sema, ty)
    }

    pub fn doc(&self) -> Option<String> {
        let Self { sema: _, inner } = self;
        match *inner {
            CallableInner::HirDef(ref def) => def.func().doc.as_ref().map(|doc| doc.to_string()),
            CallableInner::BuiltinFunction(ref func) => Some(func.doc.clone()),
            CallableInner::IntrinsicFunction(ref func, _) => Some(func.doc.clone()),
            CallableInner::Rule(ref rule) => rule.doc.as_ref().map(Box::to_string),
            CallableInner::Provider(ref provider)
            | CallableInner::ProviderRawConstructor(_, ref provider) => match provider {
                Provider::Builtin(provider) => Some(provider.doc.clone()),
                Provider::Custom(provider) => {
                    provider.doc.as_ref().map(|doc| doc.as_ref().to_string())
                }
            },
            CallableInner::Tag(ref tag_class) => {
                tag_class.doc.as_ref().map(|doc| doc.as_ref().to_string())
            }
            CallableInner::Macro(ref makro) => {
                makro.doc.as_ref().map(|doc| doc.as_ref().to_string())
            }
        }
    }

    pub fn file(&self) -> Option<File> {
        let Self { sema: _, inner } = self;
        match *inner {
            CallableInner::HirDef(ref def) => def.stmt().map(|stmt| stmt.file),
            _ => None,
        }
    }

    pub fn is_user_defined(&self) -> bool {
        let Self { sema: _, inner } = self;
        matches!(*inner, CallableInner::HirDef(_))
    }

    pub fn is_rule(&self) -> bool {
        let Self { sema: _, inner } = self;
        matches!(*inner, CallableInner::Rule(_))
    }

    pub fn is_tag(&self) -> bool {
        let Self { sema: _, inner } = self;
        matches!(*inner, CallableInner::Tag(_))
    }

    pub fn is_macro(&self) -> bool {
        let Self { sema: _, inner } = self;
        matches!(*inner, CallableInner::Macro(_))
    }

    /// The declaration associated with a named argument at a call site.
    pub fn keyword_definition(&self, name: &str) -> Option<InFile<TextRange>> {
        let Self { sema, inner } = self;
        if let CallableInner::Rule(rule) = inner {
            if let Some(expr) = rule.attrs.as_ref().and_then(|attrs| attrs.expr) {
                return dict_key_definition(sema.db, expr, name);
            }
        }
        let (param, _) = self
            .params()
            .into_iter()
            .find(|(param, _)| param.name().as_ref().map(Name::as_str) == Some(name))?;
        param.source_range()
    }
}

/// Reperesents different types of callables.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CallableInner {
    /// A user-defined function.
    HirDef(FunctionDef),

    // An intrinsic function, i.e. a function defined by the Starlark spec.
    IntrinsicFunction(IntrinsicFunction, Option<Substitution>),

    /// A builtin function (e.g. Bazel builtins).
    BuiltinFunction(BuiltinFunction),

    /// A Bazel rule.
    Rule(Rule),

    /// A Bazel provider.
    Provider(Provider),

    /// The raw constructor for a Bazel provider.
    /// See: https://bazel.build/rules/lib/globals/bzl.html#provider
    ProviderRawConstructor(Name, Provider),

    /// A Bazel tag.
    Tag(Arc<TagClass>),

    /// A Bazel symbolic macro.
    Macro(Macro),
}

/// A parameter for a function, rule, etc.
/// The actual data is stored in [`ParamInner`], which wraps some
/// crate-internal data types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Param<'a> {
    sema: Semantics<'a>,
    inner: ParamInner,
}

impl<'a> Param<'a> {
    fn new(sema: Semantics<'a>, inner: ParamInner) -> Self {
        Self { sema, inner }
    }
}

/// Reperesents parameters for different types of callables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ParamInner {
    /// A function or lambda parameter.
    Param { func: Function, index: usize },

    /// A parameter for an intrinsic function (i.e. a Starlark builtin).
    IntrinsicParam {
        parent: IntrinsicFunction,
        index: usize,
    },

    /// A parameter for a builtin function (e.g. Bazel builtins).
    BuiltinParam {
        parent: BuiltinFunction,
        index: usize,
    },

    /// Corresponds to a rule attribute.
    RuleParam(RuleParam),

    /// Corresponds to a provider field.
    ProviderParam { provider: Provider, index: usize },

    /// Corresponds to a tag attribute.
    TagParam(TagParam),
}

/// An item in a load statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadItem<'a> {
    sema: Semantics<'a>,
    id: InFile<LoadItemId>,
}

impl<'a> LoadItem<'a> {
    pub fn definition(&self) -> Option<ScopeDef<'a>> {
        let Self { sema, id } = self;
        let load_stmt = match &module(sema.db, id.file).load_items[id.value] {
            def::LoadItem::Direct { name: _, load_stmt } => load_stmt,
            def::LoadItem::Aliased {
                alias: _,
                name: _,
                load_stmt,
            } => load_stmt,
        };
        let loaded_file = queries::resolve_load_stmt(sema.db, id.file, load_stmt.clone())?;
        sema.scope_for_module(loaded_file)
            .resolve_name(&self.name())
            .into_iter()
            .next()
    }

    /// The name of the item being loaded by the load statement.
    pub fn name(&self) -> Name {
        let Self { sema, id } = self;
        let db = sema.db;
        match &module(db, id.file).load_items[id.value] {
            def::LoadItem::Direct { name, .. } | def::LoadItem::Aliased { name, .. } => {
                Name::from_str(name)
            }
        }
    }
}

/// Represents the different types of definition present within a scope.
/// Mostly provides a nicer API for [`scope::ScopeDef`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeDef<'a> {
    /// A function definition.
    Callable(Callable<'a>),

    /// A variable definition.
    Variable(Variable<'a>),

    /// A parameter for a user-defined function.
    Parameter(Param<'a>),

    /// An item loaded by a `load` statement.
    LoadItem(LoadItem<'a>),
}

impl<'a> ScopeDef<'a> {
    fn semantics(&self) -> Semantics<'a> {
        match self {
            Self::Callable(Callable { sema, inner: _ }) => *sema,
            Self::Variable(Variable { sema, def: _ }) => *sema,
            Self::Parameter(Param { sema, inner: _ }) => *sema,
            Self::LoadItem(LoadItem { sema, id: _ }) => *sema,
        }
    }

    /// The full source range of the definition, including a function's body.
    pub fn source_range(&self) -> Option<InFile<TextRange>> {
        let db = self.semantics().db;
        match self {
            Self::Callable(Callable { sema: _, inner }) => {
                let CallableInner::HirDef(def) = inner else {
                    return None;
                };
                let func = def.func();
                Some(InFile {
                    file: func.file,
                    value: func.ptr.text_range(),
                })
            }
            Self::Variable(Variable { sema: _, def }) => {
                let scope::VariableDef {
                    file,
                    expr,
                    source: _,
                } = def.as_ref()?;
                expr_source_range(
                    db,
                    InFile {
                        file: *file,
                        value: *expr,
                    },
                )
            }
            Self::Parameter(param) => param.source_range(),
            Self::LoadItem(LoadItem { sema: _, id }) => {
                let ptr = source_map(db, id.file).load_item_map_back.get(&id.value)?;
                Some(InFile {
                    file: id.file,
                    value: ptr.syntax_node_ptr().text_range(),
                })
            }
        }
    }

    /// The navigation target: a function's name or another definition's range.
    pub fn definition_range(&self) -> Option<InFile<TextRange>> {
        if let Self::Callable(Callable { sema, inner }) = self {
            let CallableInner::HirDef(def) = inner else {
                return None;
            };
            let InFile { file, value } = def.stmt()?;
            let range = *source_map(sema.db, file).function_names.get(&value)?;
            return Some(InFile { file, value: range });
        }
        self.source_range()
    }

    pub fn ty(&self) -> Type<'a> {
        let db = self.semantics().db;
        let ty = match self {
            ScopeDef::Variable(Variable { sema: _, def }) => {
                if let Some(scope::VariableDef {
                    file,
                    expr,
                    source: _,
                }) = def
                {
                    queries::infer_expr(db, *file, *expr)
                } else {
                    Ty::unknown()
                }
            }
            ScopeDef::Callable(callable) => return callable.ty(),
            ScopeDef::LoadItem(LoadItem { sema: _, id }) => {
                queries::infer_load_item(db, id.file, id.value)
            }
            _ => Ty::unknown(),
        };
        Type::new(self.semantics(), ty)
    }

    pub fn is_user_defined(&self) -> bool {
        match self {
            ScopeDef::Callable(it) => it.is_user_defined(),
            ScopeDef::Variable(it) => it.is_user_defined(),
            _ => true,
        }
    }
}

impl<'a> ScopeDef<'a> {
    fn new(sema: Semantics<'a>, value: scope::ScopeDef) -> Self {
        match value {
            scope::ScopeDef::Function(it) => {
                ScopeDef::Callable(Callable::new(sema, CallableInner::HirDef(it)))
            }
            scope::ScopeDef::IntrinsicFunction(it) => ScopeDef::Callable(Callable::new(
                sema,
                CallableInner::IntrinsicFunction(it, None),
            )),
            scope::ScopeDef::BuiltinFunction(it) => {
                ScopeDef::Callable(Callable::new(sema, CallableInner::BuiltinFunction(it)))
            }
            scope::ScopeDef::Variable(it) => ScopeDef::Variable(Variable {
                sema,
                def: Some(it),
            }),
            scope::ScopeDef::BuiltinVariable(type_ref) => match type_ref {
                TypeRef::Provider(provider) => ScopeDef::Callable(Callable::new(
                    sema,
                    CallableInner::Provider(Provider::Builtin(provider)),
                )),
                _ => ScopeDef::Variable(Variable { sema, def: None }),
            },
            scope::ScopeDef::Parameter(ParameterDef { func, index }) => {
                ScopeDef::Parameter(Param::new(sema, ParamInner::Param { func, index }))
            }
            scope::ScopeDef::LoadItem(def) => ScopeDef::LoadItem(LoadItem {
                sema,
                id: InFile {
                    file: def.file,
                    value: def.load_item,
                },
            }),
        }
    }
}

pub(crate) fn lower(db: &dyn Db, file: File) -> &ModuleInfo {
    let File {
        source,
        dialect,
        info,
    } = file;
    lower_query(db, source, (dialect, info))
}

#[salsa::tracked(returns(ref))]
pub(crate) fn lower_query(
    db: &dyn Db,
    source: ruff_db::files::File,
    context: (starpls_common::Dialect, Option<starpls_common::FileInfo>),
) -> ModuleInfo {
    let (dialect, info) = context;
    let file = File {
        source,
        dialect,
        info,
    };
    let parse = parse(db, file);
    let (module, source_map) = Module::new_with_source_map(db, file, parse.tree());
    ModuleInfo {
        file,
        module,
        source_map,
    }
}

/// Shortcut to immediately access a `lower` query's `Module`.
pub(crate) fn module(db: &dyn Db, file: File) -> &Module {
    &lower(db, file).module
}

/// Shortcut to immediately access a `lower` query's `ModuleSourceMap`.
pub(crate) fn source_map(db: &dyn Db, file: File) -> &ModuleSourceMap {
    &lower(db, file).source_map
}
