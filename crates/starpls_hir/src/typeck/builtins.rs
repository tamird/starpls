use std::collections::HashSet;
use std::sync::Arc;

use either::Either;
use rustc_hash::FxHashMap;
use smallvec::smallvec;
use starpls_bazel::attr;
use starpls_bazel::builtin::Callable;
use starpls_bazel::builtin::Param;
use starpls_bazel::builtin::Type;
use starpls_bazel::builtin::Value;
use starpls_bazel::env::make_workspace_builtins;
use starpls_bazel::env::{self};
use starpls_bazel::Builtins;
use starpls_bazel::BUILTINS_TYPES_DENY_LIST;
use starpls_bazel::BUILTINS_VALUES_DENY_LIST;
use starpls_bazel::KNOWN_PROVIDER_TYPES;
use starpls_common::Dialect;
use starpls_common::File;
use starpls_common::InFile;
use starpls_intern::impl_internable;
use starpls_intern::Interned;

use crate::def::resolver::Export;
use crate::def::resolver::Resolver;
use crate::def::Argument;
use crate::def::AssignmentSource;
use crate::def::Expr;
use crate::def::Stmt;
use crate::module;
use crate::source_map;
use crate::typeck::Attribute;
use crate::typeck::AttributeData;
use crate::typeck::AttributeKind;
use crate::typeck::CustomProvider;
use crate::typeck::CustomProviderFields;
use crate::typeck::DictLiteral;
use crate::typeck::Macro;
use crate::typeck::ModuleExtension;
use crate::typeck::Provider;
use crate::typeck::ProviderField;
use crate::typeck::Rule as TyRule;
use crate::typeck::RuleAttributes;
use crate::typeck::RuleKind;
use crate::typeck::Struct;
use crate::typeck::TagClass;
use crate::typeck::TagClassData;
use crate::typeck::Tuple;
use crate::typeck::TyContext;
use crate::Db;
use crate::ExprId;
use crate::Name;
use crate::Ty;
use crate::TyKind;
use crate::TypeRef;

const DEFAULT_DOC: &str = "See the [Bazel Build Encyclopedia](https://bazel.build/reference/be/overview) for more details.";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BuiltinTypes {
    pub(crate) types: FxHashMap<String, Ty>,
}

pub(crate) type BuiltinType = Interned<BuiltinTypeData>;

#[derive(Debug, PartialEq, Eq, Hash)]
pub(crate) struct BuiltinTypeData {
    pub(crate) name: Name,
    pub(crate) fields: Vec<BuiltinField>,
    pub(crate) methods: Vec<BuiltinFunction>,
    pub(crate) doc: String,
    pub(crate) indexable_by: Option<(TypeRef, TypeRef)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct BuiltinField {
    pub(crate) name: Name,
    pub(crate) type_ref: TypeRef,
    pub(crate) doc: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BuiltinGlobals {
    pub(crate) bzl_globals: APIGlobals,
    pub(crate) bzlmod_globals: APIGlobals,
    pub(crate) repo_globals: APIGlobals,
    pub(crate) workspace_globals: APIGlobals,
    pub(crate) cquery_globals: APIGlobals,
    pub(crate) vendor_globals: APIGlobals,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct APIGlobals {
    pub(crate) functions: FxHashMap<String, BuiltinFunction>,
    pub(crate) variables: FxHashMap<String, TypeRef>,
}

impl APIGlobals {
    fn from_values<'a, I>(providers: &BuiltinProviders, values: I) -> Self
    where
        I: Iterator<Item = &'a Value>,
    {
        let mut functions = FxHashMap::default();
        let mut variables = FxHashMap::default();
        let providers = &providers.providers;

        for value in values {
            // Skip deny-listed globals, which are handled directly by the
            // language server.
            if value.name.is_empty() || BUILTINS_VALUES_DENY_LIST.contains(&value.name.as_str()) {
                continue;
            }

            match (providers.get(value.name.as_str()), &value.callable) {
                (Some(provider), _) => {
                    variables.insert(value.name.clone(), TypeRef::Provider(provider.clone()));
                }
                (None, Some(callable)) => {
                    functions.insert(
                        value.name.clone(),
                        builtin_function(&value.name, callable, &value.doc, None),
                    );
                }
                (None, None) => {
                    variables.insert(value.name.clone(), parse_type_ref(&value.r#type));
                }
            }
        }

        Self {
            functions,
            variables,
        }
    }
}

pub(crate) type BuiltinFunction = Interned<BuiltinFunctionData>;

#[derive(Debug, PartialEq, Eq, Hash)]
pub(crate) struct BuiltinFunctionData {
    pub(crate) name: Name,
    pub(crate) parent_type: Option<String>,
    pub(crate) params: Vec<BuiltinFunctionParam>,
    pub(crate) ret_type_ref: TypeRef,
    pub(crate) doc: String,
}

impl BuiltinFunctionData {
    pub(crate) fn maybe_unique_ret_type<'a, I>(
        &'a self,
        tcx: &'a mut TyContext,
        file: File,
        call_expr: ExprId,
        mut args: I,
    ) -> Option<Ty>
    where
        I: Iterator<Item = (&'a Argument, &'a Ty)>,
    {
        let resolve_load_like = |db: &dyn Db, args: &mut I| {
            let mut next_string_arg = || {
                args.next().and_then(|(arg, ty)| match (arg, ty.kind()) {
                    (Argument::Simple { .. }, TyKind::String(Some(s))) => Some(s.as_ref()),
                    _ => None,
                })
            };

            let path = next_string_arg()?;
            let name = next_string_arg()?;
            let loaded_file = db.load_file(path, file.dialect, file).ok()??;

            Some(
                match Resolver::resolve_export_in_file(db, loaded_file, &Name::from_str(name))? {
                    Export::Variable(expr) => InFile {
                        file: loaded_file,
                        value: expr.expr,
                    },
                    _ => return None,
                },
            )
        };

        let db = tcx.db;
        let ret_kind = match (self.parent_type.as_deref(), self.name.as_str()) {
            (None, "struct") => {
                let fields = args
                    .filter_map(|(arg, ty)| match arg {
                        Argument::Keyword { name, .. } => Some((name.clone(), ty.clone())),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice();
                TyKind::Struct(Some(Struct::Inline {
                    call_expr: InFile {
                        file,
                        value: call_expr,
                    },
                    fields,
                }))
            }
            (None, "provider") => {
                let mut fields = None;
                let mut doc = None;
                let mut has_init = false;
                for (arg, ty) in args {
                    if let Argument::Keyword { name, .. } = arg {
                        match name.as_str() {
                            "doc" => {
                                if let TyKind::String(Some(s)) = ty.kind() {
                                    doc = Some(s.clone());
                                }
                            }
                            "fields" => {
                                if let TyKind::Dict(_, _, Some(lit)) = ty.kind() {
                                    fields = Some(CustomProviderFields {
                                        expr: lit.expr,
                                        fields: lit
                                            .known_keys
                                            .iter()
                                            .flat_map(|(key, value)| {
                                                let name = &key.as_ref();
                                                if !name.is_empty() {
                                                    Some(ProviderField {
                                                        name: Name::from_str(key.as_ref()),
                                                        doc: match value.kind() {
                                                            TyKind::String(Some(s)) => Some(
                                                                s.as_ref()
                                                                    .to_string()
                                                                    .into_boxed_str(),
                                                            ),
                                                            _ => None,
                                                        },
                                                    })
                                                } else {
                                                    None
                                                }
                                            })
                                            .collect(),
                                    });
                                }
                            }
                            "init" => {
                                has_init = true;
                            }
                            _ => {}
                        }
                    }
                }

                let module = module(db, file);
                let lhs = module
                    .assignment_sources
                    .get(&call_expr)
                    .and_then(|source| {
                        let AssignmentSource::Statement(stmt) = *source else {
                            return None;
                        };
                        match module[stmt] {
                            Stmt::Assign {
                                lhs,
                                rhs: _,
                                op: _,
                                type_ref: _,
                            } => Some(lhs),
                            _ => None,
                        }
                    });
                let extract_name = |expr| match &module[expr] {
                    Expr::Name { name } => {
                        if name.is_missing() || name.as_str().is_empty() {
                            None
                        } else {
                            Some(name.clone())
                        }
                    }
                    _ => None,
                };

                if has_init {
                    let (provider_name, ctor_name) = lhs
                        .and_then(|lhs| match &module[lhs] {
                            Expr::Tuple { exprs } => {
                                let mut elements = exprs.iter().copied();
                                let provider_name = elements.next().and_then(extract_name);
                                let ctor_name = elements.next().and_then(extract_name);
                                Some((provider_name, ctor_name))
                            }
                            _ => None,
                        })
                        .unwrap_or_default();

                    let provider = Provider::Custom(Arc::new(CustomProvider {
                        name: provider_name,
                        doc,
                        fields,
                    }));

                    TyKind::Tuple(Tuple::Simple(smallvec![
                        TyKind::Provider(provider.clone()).intern(),
                        TyKind::ProviderRawConstructor(
                            ctor_name.unwrap_or_else(|| Name::new_inline("ctor")),
                            provider
                        )
                        .intern(),
                    ]))
                } else {
                    let name = lhs.and_then(extract_name);
                    TyKind::Provider(Provider::Custom(Arc::new(CustomProvider {
                        name,
                        doc,
                        fields,
                    })))
                }
            }

            (None, name @ ("rule" | "repository_rule")) => {
                let mut attrs = None;
                let mut doc = None;
                for (arg, ty) in args {
                    if let Argument::Keyword { name, .. } = arg {
                        match name.as_str() {
                            "doc" => {
                                if let TyKind::String(Some(s)) = ty.kind() {
                                    doc = Some(s.clone());
                                }
                            }
                            "attrs" => {
                                if let TyKind::Dict(_, _, Some(lit)) = ty.kind() {
                                    attrs = Some(attrs_from_dict_literal(lit, false))
                                }
                            }
                            _ => {}
                        }
                    }
                }

                TyKind::Rule(TyRule {
                    kind: if name == "rule" {
                        RuleKind::Build
                    } else {
                        RuleKind::Repository
                    },
                    doc: doc.map(|doc| doc.as_ref().into()),
                    attrs: attrs.map(Arc::new),
                })
            }

            (Some("attr"), attr) => {
                let mut doc: Option<Arc<str>> = None;
                let mut mandatory = false;
                let mut default_ptr = None;
                for (arg, ty) in args {
                    if let Argument::Keyword { name, expr } = arg {
                        match name.as_str() {
                            "doc" => {
                                if let TyKind::String(Some(s)) = ty.kind() {
                                    doc = Some(s.clone());
                                }
                            }
                            "mandatory" => {
                                if let TyKind::Bool(Some(b)) = ty.kind() {
                                    mandatory = *b;
                                }
                            }
                            "default" => {
                                if let Some(ptr) = source_map(db, file).expr_map_back.get(expr) {
                                    default_ptr = Some(ptr.syntax_node_ptr());
                                }
                            }
                            _ => {}
                        }
                    }
                }

                TyKind::Attribute(Some(Attribute::new(
                    match attr {
                        "bool" => AttributeKind::Bool,
                        "int" => AttributeKind::Int,
                        "int_list" => AttributeKind::IntList,
                        "label" => AttributeKind::Label,
                        "label_keyed_string_dict" => AttributeKind::LabelKeyedStringDict,
                        "label_list" => AttributeKind::LabelList,
                        "output" => AttributeKind::Output,
                        "output_list" => AttributeKind::OutputList,
                        "string" => AttributeKind::String,
                        "string_keyed_label_dict" => AttributeKind::StringKeyedLabelDict,
                        "string_dict" => AttributeKind::StringDict,
                        "string_list" => AttributeKind::StringList,
                        "string_list_dict" => AttributeKind::StringListDict,
                        _ => return None,
                    },
                    doc,
                    mandatory,
                    default_ptr.map(|text_range| {
                        Either::Left(InFile {
                            file,
                            value: text_range,
                        })
                    }),
                )))
            }

            (None, "tag_class") => {
                let mut attrs = None;
                let mut doc = None;
                for (arg, ty) in args {
                    if let Argument::Keyword { name, .. } = arg {
                        match name.as_str() {
                            "attrs" => {
                                if let TyKind::Dict(_, _, Some(lit)) = ty.kind() {
                                    attrs = Some(
                                        lit.known_keys
                                            .iter()
                                            .filter_map(|(name, ty)| match ty.kind() {
                                                TyKind::Attribute(Some(attr)) => {
                                                    Some(AttributeData {
                                                        name: Name::from_str(name.as_ref()),
                                                        attr: attr.clone(),
                                                    })
                                                }
                                                _ => None,
                                            })
                                            .collect::<Vec<_>>()
                                            .into_boxed_slice(),
                                    )
                                }
                            }
                            "doc" => {
                                if let TyKind::String(Some(s)) = ty.kind() {
                                    doc = Some(s.clone());
                                }
                            }
                            _ => {}
                        }
                    }
                }

                TyKind::TagClass(Arc::new(TagClass { attrs, doc }))
            }

            (None, "module_extension") => {
                let mut doc = None;
                let mut tag_classes = None;
                for (arg, ty) in args {
                    if let Argument::Keyword { name, .. } = arg {
                        match name.as_str() {
                            "doc" => {
                                if let TyKind::String(Some(s)) = ty.kind() {
                                    doc = Some(s.as_ref().into());
                                }
                            }
                            "tag_classes" => {
                                let lit = match ty.kind() {
                                    TyKind::Dict(_, _, Some(lit)) => lit,
                                    _ => continue,
                                };

                                tag_classes = Some(
                                    lit.known_keys
                                        .iter()
                                        .filter_map(|(name, ty)| match ty.kind() {
                                            TyKind::TagClass(tag_class) => Some(TagClassData {
                                                name: Name::from_str(name.as_ref()),
                                                tag_class: tag_class.clone(),
                                            }),
                                            _ => None,
                                        })
                                        .collect::<Vec<_>>()
                                        .into_boxed_slice(),
                                );
                            }
                            _ => {}
                        }
                    }
                }

                TyKind::ModuleExtension(Arc::new(ModuleExtension { doc, tag_classes }))
            }

            (None, "macro") => {
                let mut attrs = None;
                let mut doc = None;
                for (arg, ty) in args {
                    if let Argument::Keyword { name, .. } = arg {
                        match name.as_str() {
                            "doc" => {
                                if let TyKind::String(Some(s)) = ty.kind() {
                                    doc = Some(s.clone());
                                }
                            }
                            "attrs" => {
                                if let TyKind::Dict(_, _, Some(lit)) = ty.kind() {
                                    attrs = Some(Arc::new(attrs_from_dict_literal(lit, true)))
                                }
                            }
                            _ => {}
                        }
                    }
                }

                TyKind::Macro(Macro { attrs, doc })
            }

            (None, "use_extension") => {
                let expr = resolve_load_like(db, &mut args)?;
                let module_extension = match tcx.infer_expr(expr.file, expr.value).kind() {
                    TyKind::ModuleExtension(module_extension) => module_extension.clone(),
                    _ => return None,
                };
                TyKind::ModuleExtensionProxy(module_extension)
            }

            (None, "use_repo_rule") => {
                let expr = resolve_load_like(db, &mut args)?;
                let ty = tcx.infer_expr(expr.file, expr.value);
                return match ty.kind() {
                    TyKind::Rule(TyRule {
                        kind: RuleKind::Repository,
                        ..
                    }) => Some(ty),
                    _ => None,
                };
            }

            _ => return None,
        };

        Some(ret_kind.intern())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum BuiltinFunctionParam {
    Simple {
        name: Name,
        type_ref: TypeRef,
        doc: String,
        default_value: Option<String>,
        positional: bool,
        is_mandatory: bool,
    },
    ArgsList {
        name: Name,
        type_ref: TypeRef,
        doc: String,
    },
    KwargsDict {
        name: Name,
        type_ref: TypeRef,
        doc: String,
    },
}

impl BuiltinFunctionParam {
    pub(crate) fn type_ref(&self) -> Option<TypeRef> {
        Some(match self {
            BuiltinFunctionParam::Simple { type_ref, .. }
            | BuiltinFunctionParam::ArgsList { type_ref, .. }
            | BuiltinFunctionParam::KwargsDict { type_ref, .. } => type_ref.clone(),
        })
    }

    pub(crate) fn is_mandatory(&self) -> bool {
        match self {
            BuiltinFunctionParam::Simple { is_mandatory, .. } => *is_mandatory,
            _ => false,
        }
    }

    pub(crate) fn name(&self) -> Name {
        match self {
            BuiltinFunctionParam::Simple { name, .. }
            | BuiltinFunctionParam::ArgsList { name, .. }
            | BuiltinFunctionParam::KwargsDict { name, .. } => name.clone(),
        }
    }

    pub(crate) fn doc(&self) -> &str {
        match self {
            BuiltinFunctionParam::Simple { doc, .. }
            | BuiltinFunctionParam::ArgsList { doc, .. }
            | BuiltinFunctionParam::KwargsDict { doc, .. } => doc,
        }
    }
}

pub(crate) type BuiltinProvider = Interned<BuiltinProviderData>;

#[derive(Debug, PartialEq, Eq, Hash)]
pub(crate) struct BuiltinProviderData {
    pub(crate) name: Name,
    pub(crate) params: Vec<BuiltinFunctionParam>,
    pub(crate) fields: Vec<BuiltinField>,
    pub(crate) doc: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BuiltinProviders {
    pub(crate) providers: FxHashMap<String, BuiltinProvider>,
}

#[salsa::input(debug)]
pub struct BuiltinDefs {
    #[returns(ref)]
    pub builtins: Builtins,
    #[returns(ref)]
    pub rules: Builtins,
}

#[salsa::tracked(returns(ref))]
pub(crate) fn builtin_providers_query(db: &dyn Db, defs: BuiltinDefs) -> BuiltinProviders {
    // Collect all known provider types.
    let builtins = defs.builtins(db);
    let mut providers = FxHashMap::default();
    let known_provider_tys: FxHashMap<String, &Type> = builtins
        .r#type
        .iter()
        .filter(|ty| KNOWN_PROVIDER_TYPES.contains(&ty.name.as_str()))
        .map(|ty| (ty.name.clone(), ty))
        .collect();

    for value in builtins
        .global
        .iter()
        .chain(builtins.r#type.iter().flat_map(|ty| ty.field.iter()))
    {
        if let Some(ty) = known_provider_tys.get(value.name.as_str()) {
            providers.insert(
                ty.name.clone(),
                builtin_provider(ty, value.callable.as_ref()),
            );
        }
    }

    for (name, ty) in &known_provider_tys {
        if !providers.contains_key(name.as_str()) {
            providers.insert(ty.name.clone(), builtin_provider(ty, None));
        }
    }

    BuiltinProviders { providers }
}

pub(crate) fn builtin_globals(db: &dyn Db, dialect: Dialect) -> &BuiltinGlobals {
    let defs = db.get_builtin_defs(&dialect);
    builtin_globals_query(db, defs)
}

#[salsa::tracked(returns(ref))]
pub(crate) fn builtin_globals_query(db: &dyn Db, defs: BuiltinDefs) -> BuiltinGlobals {
    let builtins = defs.builtins(db);
    let rules = defs.rules(db);
    let providers = builtin_providers_query(db, defs);

    let bzl_globals = APIGlobals::from_values(
        providers,
        env::make_bzl_builtins()
            .global
            .iter()
            .chain(env::make_build_builtins().global.iter())
            .chain(builtins.global.iter())
            .chain(rules.global.iter()),
    );
    let bzlmod_globals =
        APIGlobals::from_values(providers, env::make_module_bazel_builtins().global.iter());
    let repo_globals = APIGlobals::from_values(providers, env::make_repo_builtins().global.iter());
    let workspace_globals =
        APIGlobals::from_values(providers, env::make_workspace_builtins().global.iter());
    let cquery_globals =
        APIGlobals::from_values(providers, env::make_cquery_builtins().global.iter());
    let vendor_globals =
        APIGlobals::from_values(providers, env::make_vendor_builtins().global.iter());

    BuiltinGlobals {
        bzl_globals,
        bzlmod_globals,
        repo_globals,
        workspace_globals,
        cquery_globals,
        vendor_globals,
    }
}

pub(crate) fn builtin_types(db: &dyn Db, dialect: Dialect) -> &BuiltinTypes {
    let defs = db.get_builtin_defs(&dialect);
    builtin_types_query(db, defs)
}

#[salsa::tracked(returns(ref))]
pub(crate) fn builtin_types_query(db: &dyn Db, defs: BuiltinDefs) -> BuiltinTypes {
    let mut types = FxHashMap::default();
    let builtins = defs.builtins(db);
    let rules = defs.rules(db);
    let mut missing_module_members = env::make_missing_module_members();
    let providers = &builtin_providers_query(db, defs).providers;

    // Add all builtin providers.
    types.extend(providers.iter().map(|(name, provider)| {
        (
            name.clone(),
            TyKind::ProviderInstance(Provider::Builtin(provider.clone())).intern(),
        )
    }));

    for type_ in builtins.r#type.iter() {
        // Skip deny-listed types, which are handled directly by `intrinsics.rs`, and provider types,
        // which are handled above.
        if type_.name.is_empty()
            || BUILTINS_TYPES_DENY_LIST.contains(&type_.name.as_str())
            || KNOWN_PROVIDER_TYPES.contains(&type_.name.as_str())
        {
            continue;
        }

        // Collect fields and methods.
        let mut fields = Vec::new();
        let mut methods = Vec::new();
        let mut seen_methods = HashSet::new();
        let workspace_builtins = make_workspace_builtins();

        // Special handling for the "native" type, which includes all native rules.
        if type_.name == "native" {
            // We also add symbols that are normally only available from `WORKSPACE` files, like
            // `register_execution_platforms` and `register_toolchains`. This is technically
            // incorrect if bzlmod is enabled, so we should revisit this approach in the future.
            for rule in rules.global.iter().chain(
                workspace_builtins
                    .global
                    .iter()
                    .filter(|global| !["workspace"].contains(&global.name.as_str())),
            ) {
                if let Some(callable) = &rule.callable {
                    if seen_methods.contains(&rule.name.as_str()) {
                        continue;
                    }

                    seen_methods.insert(rule.name.as_str());
                    methods.push(builtin_function(
                        &rule.name,
                        callable,
                        &rule.doc,
                        Some(&type_.name),
                    ));
                }
            }
        }

        for field in type_.field.iter().chain(
            missing_module_members
                .remove(&type_.name)
                .unwrap_or_default()
                .iter(),
        ) {
            if let Some(callable) = &field.callable {
                // Filter out duplicates.
                if !seen_methods.contains(&field.name.as_str()) {
                    match providers.get(field.name.as_str()) {
                        Some(provider) => {
                            fields.push(BuiltinField {
                                name: provider.name.clone(),
                                type_ref: TypeRef::Provider(provider.clone()),
                                doc: normalize_doc_text(&field.doc),
                            });
                        }
                        None => {
                            methods.push(builtin_function(
                                &field.name,
                                callable,
                                &field.doc,
                                Some(&type_.name),
                            ));
                        }
                    }
                }
            } else {
                let type_ref = match providers.get(field.name.as_str()) {
                    Some(provider) => TypeRef::Provider(provider.clone()),
                    None => maybe_field_type_ref_override(&type_.name, &field.name)
                        .unwrap_or_else(|| parse_type_ref(&field.r#type)),
                };

                fields.push(BuiltinField {
                    name: Name::from_str(&field.name),
                    type_ref,
                    doc: normalize_doc_text(&field.doc),
                });
            }
        }

        let indexable_by = match type_.name.as_str() {
            "ToolchainContext" => Some(("string", "ToolchainInfo")),
            // TODO(withered-magic): Audit Bazel docs for other indexable builtin types.
            _ => None,
        }
        .map(|(expected_index_ty, return_ty)| {
            (
                TypeRef::Name(Name::new_inline(expected_index_ty), None),
                TypeRef::Name(Name::new_inline(return_ty), None),
            )
        });

        types.insert(
            type_.name.clone(),
            TyKind::BuiltinType(
                Interned::new(BuiltinTypeData {
                    name: Name::from_str(&type_.name),
                    fields,
                    methods,
                    doc: normalize_doc_text(&type_.doc),
                    indexable_by,
                }),
                None,
            )
            .intern(),
        );
    }

    BuiltinTypes { types }
}

fn builtin_function(
    name: &str,
    callable: &Callable,
    doc: &str,
    parent_name: Option<&str>,
) -> BuiltinFunction {
    // Apply overrides for function return types known to be incorrect. For now, this
    // consists only of the `Label()` constructor.
    let ret_type_ref = match name {
        "Label" => "Label",
        _ => callable.return_type.as_str(),
    };

    Interned::new(BuiltinFunctionData {
        name: Name::from_str(name),
        parent_type: parent_name.map(|parent_name| parent_name.to_string()),
        params: callable.param.iter().map(builtin_param).collect(),
        ret_type_ref: parse_type_ref(ret_type_ref),
        doc: if doc.is_empty() {
            DEFAULT_DOC.to_string()
        } else {
            normalize_doc_text(doc)
        },
    })
}

fn builtin_param(param: &Param) -> BuiltinFunctionParam {
    let name = Name::from_str(param.name.trim_start_matches('*'));
    if param.is_star_arg {
        BuiltinFunctionParam::ArgsList {
            name,
            type_ref: maybe_strip_iterable_or_dict(parse_type_ref(&param.r#type)),
            doc: normalize_doc_text(&param.doc),
        }
    } else if param.is_star_star_arg {
        BuiltinFunctionParam::KwargsDict {
            name,
            type_ref: maybe_strip_iterable_or_dict(parse_type_ref(&param.r#type)),
            doc: normalize_doc_text(&param.doc),
        }
    } else {
        BuiltinFunctionParam::Simple {
            name,
            type_ref: parse_type_ref(&param.r#type),
            doc: normalize_doc_text(&param.doc),
            default_value: if !param.default_value.is_empty() {
                Some(param.default_value.clone())
            } else {
                None
            },
            positional: true,
            is_mandatory: param.is_mandatory,
        }
    }
}

fn builtin_provider(ty: &Type, callable: Option<&Callable>) -> BuiltinProvider {
    let params = match callable {
        Some(callable) => callable.param.iter().map(builtin_param).collect(),
        None => ty
            .field
            .iter()
            .filter(|field| field.callable.is_none())
            .map(|field| BuiltinFunctionParam::Simple {
                name: Name::from_str(&field.name),
                type_ref: parse_type_ref(&field.r#type),
                doc: normalize_doc_text(&field.doc),
                default_value: None,
                positional: false,
                is_mandatory: false,
            })
            .collect(),
    };

    let provider_fields = ty
        .field
        .iter()
        .map(|field| BuiltinField {
            name: Name::from_str(&field.name),
            type_ref: maybe_field_type_ref_override(&ty.name, &field.name)
                .unwrap_or_else(|| parse_type_ref(&field.r#type)),
            doc: normalize_doc_text(&field.doc),
        })
        .collect();
    Interned::new(BuiltinProviderData {
        name: Name::from_str(&ty.name),
        params,
        fields: provider_fields,
        doc: normalize_doc_text(&ty.doc),
    })
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CommonAttributes {
    pub(crate) build: Vec<(Name, Attribute)>,
    pub(crate) repository: Vec<(Name, Attribute)>,
}

impl CommonAttributes {
    pub(crate) fn get(&self, kind: RuleKind, index: usize) -> (&Name, &Attribute) {
        let (ref name, ref attr) = match kind {
            RuleKind::Build => &self.build,
            RuleKind::Repository => &self.repository,
        }[index];
        (name, attr)
    }
}

#[salsa::tracked(returns(ref))]
pub(crate) fn common_attributes_query(_db: &dyn Db) -> CommonAttributes {
    let map_attrs = |attrs: Vec<attr::Attribute>| {
        attrs
            .into_iter()
            .map(|attr| {
                use AttributeKind::*;

                (
                    Name::from_str(&attr.name),
                    Attribute {
                        kind: match attr.r#type {
                            attr::AttributeKind::Bool => Bool,
                            attr::AttributeKind::Int => Int,
                            attr::AttributeKind::IntList => IntList,
                            attr::AttributeKind::Label => Label,
                            attr::AttributeKind::LabelKeyedStringDict => LabelKeyedStringDict,
                            attr::AttributeKind::LabelList => LabelList,
                            attr::AttributeKind::Output => Output,
                            attr::AttributeKind::OutputList => OutputList,
                            attr::AttributeKind::String => String,
                            attr::AttributeKind::StringDict => StringDict,
                            attr::AttributeKind::StringList => StringList,
                            attr::AttributeKind::StringListDict => StringListDict,
                            attr::AttributeKind::StringKeyedLabelDict => StringKeyedLabelDict,
                        },
                        doc: Some(Arc::<str>::from(
                            normalize_doc_text(&attr.doc).into_boxed_str(),
                        )),
                        mandatory: attr.is_mandatory,
                        default_value: Some(Either::Right(Arc::<str>::from(
                            attr.default_value.into_boxed_str(),
                        ))),
                    },
                )
            })
            .collect()
    };

    let common = attr::make_common_attributes();
    CommonAttributes {
        build: map_attrs(common.build),
        repository: map_attrs(common.repository),
    }
}

/// Normalizes text from the generated Bazel documentation.
fn normalize_doc_text(text: &str) -> String {
    normalize_doc(text, false)
}

fn normalize_doc(text: &str, is_type: bool) -> String {
    // The main thing we need to normalize is that many Bazel types in
    // builtins file are wrapped with HTML tags, e.g. `<a>None</a>`.
    // We fix this by removing any text between angle brackets.
    let mut s = String::new();
    let mut in_tag = false;
    let chars = text.chars();
    let mut tag = String::new();

    for ch in chars {
        match (ch, in_tag) {
            ('<', _) => in_tag = true,
            ('>', _) => {
                match tag.as_str() {
                    "p" => s.push_str("\n\n"),
                    "code" | "/code" if !is_type => s.push('`'),
                    _ => {}
                }
                in_tag = false;
                tag.clear();
            }
            (_, true) => tag.push(ch),
            (_, false) => s.push(ch),
        }
    }

    s.to_string()
}

fn maybe_strip_iterable_or_dict(type_ref: TypeRef) -> TypeRef {
    match type_ref {
        TypeRef::Name(name, Some(args)) => match (args.len(), name.as_str()) {
            (1, "Iterable" | "Sequence" | "list") => args[0].clone(),
            (2, "dict") => args[1].clone(),
            _ => TypeRef::Name(name, Some(args)),
        },
        _ => type_ref,
    }
}

fn parse_type_ref(text: &str) -> TypeRef {
    let text = normalize_doc(text, true);
    let mut type_refs = text
        .split("; or ")
        .map(|part| {
            let mut parts = part.split(" of ");
            match (
                parts.next(),
                parts.next().map(|element| {
                    if let Some(stripped) = element.strip_suffix('s') {
                        stripped
                    } else {
                        element
                    }
                }),
            ) {
                (Some("Iterable" | "iterable"), element) => {
                    type_ref_with_single_arg("Iterable", element)
                }
                (Some("Sequence" | "sequence"), element) => {
                    type_ref_with_single_arg("Sequence", element)
                }
                (Some("List" | "list"), element) => type_ref_with_single_arg("list", element),
                (Some("Dict" | "dict" | "Dictionary"), element) => TypeRef::Name(
                    Name::new_inline("dict"),
                    Some(
                        vec![
                            TypeRef::from_str_opt("string"),
                            element.map_or(TypeRef::Unknown, parse_type_ref),
                        ]
                        .into_boxed_slice(),
                    ),
                ),
                (Some("String"), _) => TypeRef::from_str_opt("string"),
                (Some("Boolean" | "boolean"), _) => TypeRef::from_str_opt("bool"),
                (Some("label"), _) => TypeRef::from_str_opt("Label"),
                // Quick hack to normalize `NoneType`.
                (Some("NoneType"), _) => TypeRef::from_str_opt("None"),
                (Some(name), _) => TypeRef::from_str_opt(name),
                _ => TypeRef::Unknown,
            }
        })
        .collect::<Vec<_>>();

    if type_refs.is_empty() {
        TypeRef::Unknown
    } else if type_refs.len() == 1 {
        type_refs.pop().unwrap()
    } else {
        TypeRef::Union(type_refs)
    }
}

fn type_ref_with_single_arg(name: &str, element: Option<&str>) -> TypeRef {
    TypeRef::Name(
        Name::from_str(name),
        Some(vec![element.map_or(TypeRef::Unknown, parse_type_ref)].into_boxed_slice()),
    )
}

fn maybe_field_type_ref_override(typ: &str, field: &str) -> Option<TypeRef> {
    let type_ref = match (typ, field) {
        ("ctx", "executable" | "file" | "outputs") => TypeRef::Name(
            Name::new_inline("struct"),
            Some(vec![TypeRef::Name(Name::new_inline("File"), None)].into_boxed_slice()),
        ),

        ("ctx", "files") => TypeRef::Name(
            Name::new_inline("struct"),
            Some(
                vec![TypeRef::Name(
                    Name::new_inline("list"),
                    Some(vec![TypeRef::Name(Name::new_inline("File"), None)].into_boxed_slice()),
                )]
                .into_boxed_slice(),
            ),
        ),
        _ => return None,
    };

    Some(type_ref)
}

fn attrs_from_dict_literal(lit: &DictLiteral, allow_none: bool) -> RuleAttributes {
    RuleAttributes {
        attrs: lit
            .known_keys
            .iter()
            .filter_map(|(name, ty)| match ty.kind() {
                TyKind::Attribute(Some(attr)) => {
                    Some((Name::from_str(name.as_ref()), Some(attr.clone())))
                }
                TyKind::None if allow_none => Some((Name::from_str(name.as_ref()), None)),
                _ => None,
            })
            .collect::<Vec<_>>(),
        expr: lit.expr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_doc_text() {
        assert_eq!(normalize_doc_text("int").as_str(), "int");
        assert_eq!(normalize_doc_text("<a>int</a>").as_str(), "int");
        assert_eq!(
            normalize_doc_text("<a>int</a>; or <a>string</a>").as_str(),
            "int; or string"
        )
    }
}

impl_internable!(BuiltinTypeData, BuiltinFunctionData, BuiltinProviderData);
