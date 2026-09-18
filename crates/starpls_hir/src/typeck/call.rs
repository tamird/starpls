//! Starlark signatures and argument expansion for Ty's structural call matcher.
use std::collections::hash_map::Entry;

use rustc_hash::FxHashMap;
use starpls_common::File;
use starpls_common::InFile;
use ty_call_binding::MatchedArgument;
use ty_call_binding::Matcher;
use ty_call_binding::Parameter;

use crate::def::Argument;
use crate::def::Expr;
use crate::def::Literal;
use crate::def::Param;
use crate::def::StmtId;
use crate::module;
use crate::typeck::builtins::BuiltinFunctionParam;
use crate::typeck::intrinsics::IntrinsicFunctionParam;
use crate::typeck::resolve_type_ref_opt;
use crate::typeck::Attribute;
use crate::typeck::Provider;
use crate::typeck::Tuple;
use crate::typeck::Ty;
use crate::typeck::TyContext;
use crate::typeck::TyKind;
use crate::typeck::TypeRef;
use crate::Db;
use crate::ExprId;
use crate::Name;

pub(crate) enum ParameterType {
    Resolved(Ty),
    Declared {
        reference: Option<TypeRef>,
        scope: Option<InFile<StmtId>>,
    },
}

impl ParameterType {
    pub(crate) fn resolve(&self, cx: &mut TyContext<'_>) -> Ty {
        match self {
            Self::Resolved(ty) => ty.clone(),
            Self::Declared { reference, scope } => {
                resolve_type_ref_opt(cx, reference.clone(), *scope)
            }
        }
    }
}

pub(crate) struct CallParameter<'a> {
    pub(crate) shape: Parameter<'a>,
    pub(crate) ty: ParameterType,
    pub(crate) required: bool,
    pub(crate) deprecated: bool,
    pub(crate) display_index: usize,
}

impl CallParameter<'_> {
    pub(crate) fn name(&self) -> Option<&str> {
        match self.shape {
            Parameter::PositionalOnly(name) => name,
            Parameter::PositionalOrKeyword(name) => Some(name),
            Parameter::KeywordOnly(name) => Some(name),
            Parameter::Variadic => None,
            Parameter::KeywordVariadic => None,
        }
    }
}

pub(crate) struct Signature<'a> {
    pub(crate) parameters: Vec<CallParameter<'a>>,
    pub(crate) attributes: bool,
    allow_unknown: bool,
    disallowed: Vec<&'a Name>,
}

impl<'a> Signature<'a> {
    pub(crate) fn for_checking(db: &'a dyn Db, callee: &'a Ty) -> Option<Self> {
        match callee.kind() {
            TyKind::Provider(_) => None,
            TyKind::ProviderRawConstructor(_, _) => None,
            _ => Self::new(db, callee),
        }
    }

    pub(crate) fn new(db: &'a dyn Db, callee: &'a Ty) -> Option<Self> {
        let mut signature = Self {
            parameters: Vec::new(),
            attributes: false,
            allow_unknown: false,
            disallowed: Vec::new(),
        };
        match callee.kind() {
            TyKind::Function(def) => {
                let module = module(db, def.func().file);
                let mut keyword_only = false;
                for (display_index, id) in def.func().params.iter().enumerate() {
                    let param = &module[*id];
                    let shape = match param {
                        Param::Simple {
                            name,
                            default: _,
                            type_ref: _,
                            doc: _,
                        } => {
                            let name = name.as_str();
                            if keyword_only {
                                Parameter::KeywordOnly(name)
                            } else {
                                Parameter::PositionalOrKeyword(name)
                            }
                        }
                        Param::ArgsList {
                            name,
                            type_ref: _,
                            doc: _,
                        } => {
                            keyword_only = true;
                            if name.is_missing() {
                                continue;
                            }
                            Parameter::Variadic
                        }
                        Param::KwargsDict {
                            name: _,
                            type_ref: _,
                            doc: _,
                        } => Parameter::KeywordVariadic,
                    };
                    signature.parameters.push(CallParameter {
                        shape,
                        ty: ParameterType::Declared {
                            reference: param.type_ref(),
                            scope: def.stmt(),
                        },
                        required: !param.is_optional() && !param.name().is_missing(),
                        deprecated: false,
                        display_index,
                    });
                    if matches!(shape, Parameter::KeywordVariadic) {
                        break;
                    }
                }
            }
            TyKind::IntrinsicFunction(func, subst) => {
                for (display_index, param) in func.params.iter().enumerate() {
                    let (shape, ty, deprecated) = match param {
                        IntrinsicFunctionParam::Positional { ty, optional: _ } => {
                            (Parameter::PositionalOnly(None), ty.clone(), false)
                        }
                        IntrinsicFunctionParam::Keyword {
                            name,
                            ty,
                            deprecated,
                        } => (
                            Parameter::KeywordOnly(name.as_str()),
                            ty.clone(),
                            *deprecated,
                        ),
                        IntrinsicFunctionParam::ArgsList { ty } => {
                            (Parameter::Variadic, ty.clone(), false)
                        }
                        IntrinsicFunctionParam::KwargsDict => {
                            (Parameter::KeywordVariadic, Ty::any(), false)
                        }
                    };
                    signature.parameters.push(CallParameter {
                        shape,
                        ty: ParameterType::Resolved(ty.substitute(&subst.args)),
                        required: !param.is_optional(),
                        deprecated,
                        display_index,
                    });
                }
            }
            TyKind::BuiltinFunction(func) => signature.builtin(&func.params, false),
            TyKind::Rule(rule) => signature.attrs(rule.attrs(db)),
            TyKind::Tag(tag) => signature.attrs(
                tag.attrs
                    .iter()
                    .flatten()
                    .map(|data| (&data.name, &data.attr)),
            ),
            TyKind::Macro(makro) => {
                signature.attrs(makro.attrs());
                signature.disallowed.extend(makro.disallowed_attrs());
            }
            TyKind::Provider(provider) | TyKind::ProviderRawConstructor(_, provider) => {
                signature.allow_unknown = true;
                match provider {
                    Provider::Builtin(provider) => signature.builtin(&provider.params, true),
                    Provider::Custom(provider) => {
                        for (display_index, field) in provider
                            .fields
                            .iter()
                            .flat_map(|fields| fields.fields.iter())
                            .enumerate()
                        {
                            signature.parameters.push(CallParameter {
                                shape: Parameter::KeywordOnly(field.name.as_str()),
                                ty: ParameterType::Resolved(Ty::unknown()),
                                required: false,
                                deprecated: false,
                                display_index,
                            });
                        }
                    }
                }
            }
            _ => return None,
        }
        Some(signature)
    }

    fn builtin(&mut self, params: &'a [BuiltinFunctionParam], provider: bool) {
        for (display_index, param) in params.iter().enumerate() {
            let shape = match param {
                BuiltinFunctionParam::Simple {
                    name,
                    type_ref: _,
                    doc: _,
                    default_value: _,
                    positional,
                    is_mandatory: _,
                } => {
                    if *positional && !provider {
                        Parameter::PositionalOrKeyword(name.as_str())
                    } else {
                        Parameter::KeywordOnly(name.as_str())
                    }
                }
                BuiltinFunctionParam::ArgsList {
                    name: _,
                    type_ref: _,
                    doc: _,
                } => Parameter::Variadic,
                BuiltinFunctionParam::KwargsDict {
                    name: _,
                    type_ref: _,
                    doc: _,
                } => Parameter::KeywordVariadic,
            };
            self.parameters.push(CallParameter {
                shape,
                ty: ParameterType::Declared {
                    reference: param.type_ref(),
                    scope: None,
                },
                required: param.is_mandatory(),
                deprecated: false,
                display_index,
            });
            if matches!(shape, Parameter::KeywordVariadic) {
                break;
            }
        }
    }

    fn attrs(&mut self, attrs: impl Iterator<Item = (&'a Name, &'a Attribute)>) {
        self.attributes = true;
        self.allow_unknown = true;
        self.parameters
            .extend(
                attrs
                    .enumerate()
                    .map(|(display_index, (name, attr))| CallParameter {
                        shape: Parameter::KeywordOnly(name.as_str()),
                        ty: ParameterType::Resolved(attr.expected_ty()),
                        required: attr.mandatory,
                        deprecated: false,
                        display_index,
                    }),
            );
        self.parameters.push(CallParameter {
            shape: Parameter::KeywordVariadic,
            ty: ParameterType::Resolved(Ty::any()),
            required: false,
            deprecated: false,
            display_index: self.parameters.len(),
        });
    }
}

pub(crate) struct Value {
    pub(crate) expr: ExprId,
    /// Unknown expansions can satisfy a parameter without proving its value's type.
    pub(crate) ty: Option<Ty>,
}

enum ExpandedArgument {
    Positional(Value),
    Keyword(Name, Value),
    Variadic(Value),
    Keywords(Value),
}

pub(crate) struct CallBindings {
    pub(crate) arguments: Box<[MatchedArgument<Value>]>,
    pub(crate) missing: Vec<usize>,
    pub(crate) errors: Vec<(ExprId, String)>,
    pub(crate) active_parameter: Option<usize>,
}

/// Validate source order separately from parameter matching. Starlark permits one
/// `*args` after named arguments, followed by at most one `**kwargs`.
pub(crate) fn argument_order(args: &[Argument]) -> Vec<(usize, &'static str)> {
    let mut keyword = false;
    let mut variadic = false;
    let mut keywords = false;
    let mut errors = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        let error = match arg {
            Argument::Simple { expr: _ } => {
                if keywords {
                    Some("Positional argument cannot follow keyword argument unpacking")
                } else if variadic {
                    Some("Positional argument cannot follow iterable argument unpacking")
                } else if keyword {
                    Some("Positional argument cannot follow keyword arguments")
                } else {
                    None
                }
            }
            Argument::Keyword { name: _, expr: _ } => {
                keyword = true;
                if keywords {
                    Some("Keyword argument cannot follow keyword argument unpacking")
                } else if variadic {
                    Some("Keyword argument cannot follow iterable argument unpacking")
                } else {
                    None
                }
            }
            Argument::UnpackedList { expr: _ } => {
                let error = if keywords {
                    Some("Unpacked iterable argument cannot follow keyword argument unpacking")
                } else if variadic {
                    Some("Only one iterable argument unpacking is allowed")
                } else {
                    None
                };
                variadic = true;
                error
            }
            Argument::UnpackedDict { expr: _ } => {
                let error = keywords.then_some("Only one keyword argument unpacking is allowed");
                keywords = true;
                error
            }
        };
        if let Some(error) = error {
            errors.push((index, error));
        }
    }
    errors
}

pub(crate) fn argument_expr(arg: &Argument) -> ExprId {
    match *arg {
        Argument::Simple { expr } => expr,
        Argument::Keyword { name: _, expr } => expr,
        Argument::UnpackedList { expr } => expr,
        Argument::UnpackedDict { expr } => expr,
    }
}

fn expand(cx: &mut TyContext<'_>, file: File, arg: &Argument, ty: &Ty) -> Vec<ExpandedArgument> {
    let expr = argument_expr(arg);
    let value = || Value {
        expr,
        ty: Some(ty.clone()),
    };
    match arg {
        Argument::Simple { expr: _ } => vec![ExpandedArgument::Positional(value())],
        Argument::Keyword { name, expr: _ } => {
            vec![ExpandedArgument::Keyword(name.clone(), value())]
        }
        Argument::UnpackedList { expr: _ } => {
            let module = module(cx.db, file);
            let mut source = expr;
            while let Expr::Paren { expr } = module[source] {
                source = expr;
            }
            if let Expr::List { exprs } | Expr::Tuple { exprs } = &module[source] {
                return exprs
                    .iter()
                    .map(|expr| {
                        ExpandedArgument::Positional(Value {
                            expr: *expr,
                            ty: Some(cx.infer_expr(file, *expr)),
                        })
                    })
                    .collect();
            }
            if let TyKind::Tuple(Tuple::Simple(types)) = ty.kind() {
                return types
                    .iter()
                    .map(|ty| {
                        ExpandedArgument::Positional(Value {
                            expr,
                            ty: Some(ty.clone()),
                        })
                    })
                    .collect();
            }
            vec![ExpandedArgument::Variadic(Value { expr, ty: None })]
        }
        Argument::UnpackedDict { expr: _ } => {
            let module = module(cx.db, file);
            let mut source = expr;
            while let Expr::Paren { expr } = module[source] {
                source = expr;
            }
            if let Expr::Dict { entries } = &module[source] {
                // A direct literal is closed only when every key is a literal string.
                // Inferred known_keys can be partial or stale after dictionary mutation.
                let arguments = entries
                    .iter()
                    .map(|entry| {
                        let Expr::Literal {
                            literal: Literal::String(key),
                        } = &module[entry.key]
                        else {
                            return None;
                        };
                        Some(ExpandedArgument::Keyword(
                            Name::from_str(key),
                            Value {
                                expr: entry.value,
                                ty: Some(cx.infer_expr(file, entry.value)),
                            },
                        ))
                    })
                    .collect::<Option<Vec<_>>>();
                if let Some(arguments) = arguments {
                    return arguments;
                }
            }
            vec![ExpandedArgument::Keywords(Value { expr, ty: None })]
        }
    }
}

pub(crate) fn bind(
    cx: &mut TyContext<'_>,
    file: File,
    signature: &Signature<'_>,
    args: &[Argument],
    types: &[Ty],
    active_arg: Option<usize>,
) -> CallBindings {
    let expanded: Vec<_> = args
        .iter()
        .zip(types)
        .map(|(arg, ty)| expand(cx, file, arg, ty))
        .collect();
    let mut matcher = Matcher::new(
        signature.parameters.iter().map(|param| param.shape),
        args.len(),
    );
    for args in &expanded {
        for arg in args {
            if let ExpandedArgument::Keyword(name, _) = arg {
                matcher.reserve_keyword(name.as_str());
            }
        }
    }
    let mut errors = Vec::new();
    let mut keyword_names = FxHashMap::default();
    let mut keyword_expansions = Vec::new();
    for (argument_index, expanded) in expanded.into_iter().enumerate() {
        for arg in expanded {
            match arg {
                ExpandedArgument::Positional(value) => {
                    if let Some(index) = matcher.next_positional(argument_index) {
                        assign(
                            &mut matcher,
                            signature,
                            &mut errors,
                            argument_index,
                            index,
                            value,
                        );
                    } else if !signature.allow_unknown {
                        errors.push((value.expr, "Unexpected positional argument".to_string()));
                    }
                }
                ExpandedArgument::Keyword(name, value) => {
                    if signature.disallowed.contains(&&name) {
                        errors.push((
                            value.expr,
                            format!("Cannot set attribute \"{}\"", name.as_str()),
                        ));
                    }
                    let entry = keyword_names.entry(name);
                    let name = entry.key();
                    let selected = matcher.keyword(name.as_str());
                    let repeated = matches!(&entry, Entry::Occupied(_));
                    let name = name.as_str();
                    match selected {
                        Ok(index) => {
                            let expr = value.expr;
                            let duplicate = matcher.assign(argument_index, index, value);
                            if duplicate || repeated {
                                errors.push((
                                    expr,
                                    format!("Multiple arguments for parameter \"{}\"", name),
                                ));
                            }
                        }
                        Err(_) => {
                            if !signature.allow_unknown {
                                errors.push((
                                    value.expr,
                                    format!("Unexpected keyword argument \"{}\"", name),
                                ));
                            }
                        }
                    }
                    entry.or_insert(());
                }
                ExpandedArgument::Variadic(value) => {
                    while let Some(index) = matcher.positional_index() {
                        if matcher.keyword_reserved(index) {
                            break;
                        }
                        let index = matcher.next_positional(argument_index).unwrap();
                        assign(
                            &mut matcher,
                            signature,
                            &mut errors,
                            argument_index,
                            index,
                            Value {
                                expr: value.expr,
                                ty: None,
                            },
                        );
                    }
                    if let Some(index) = matcher.variadic_index() {
                        assign(
                            &mut matcher,
                            signature,
                            &mut errors,
                            argument_index,
                            index,
                            value,
                        );
                    }
                }
                ExpandedArgument::Keywords(value) => {
                    keyword_expansions.push((argument_index, value))
                }
            }
        }
    }
    // Unknown mappings can satisfy the remaining parameters, but cannot prove
    // conflicts with known arguments, including in malformed editor input.
    for (argument_index, value) in keyword_expansions {
        for (index, param) in signature.parameters.iter().enumerate() {
            let eligible = match param.shape {
                Parameter::PositionalOrKeyword(_) => !matcher.parameter_matched(index),
                Parameter::KeywordOnly(_) => !matcher.parameter_matched(index),
                Parameter::KeywordVariadic => true,
                Parameter::PositionalOnly(_) => false,
                Parameter::Variadic => false,
            };
            if eligible {
                matcher.assign(
                    argument_index,
                    index,
                    Value {
                        expr: value.expr,
                        ty: None,
                    },
                );
            }
        }
    }

    let active_parameter = active_arg
        .and_then(|arg| {
            if arg == args.len() {
                matcher
                    .positional_index()
                    .or_else(|| matcher.variadic_index())
            } else {
                matcher
                    .matches()
                    .get(arg)?
                    .parameters
                    .first()
                    .map(|matched| matched.index)
            }
        })
        .map(|index| signature.parameters[index].display_index);
    let missing = signature
        .parameters
        .iter()
        .enumerate()
        .filter_map(|(index, param)| {
            (param.required && !matcher.parameter_matched(index)).then_some(index)
        })
        .collect();
    CallBindings {
        arguments: matcher.into_matches(),
        missing,
        errors,
        active_parameter,
    }
}

fn assign(
    matcher: &mut Matcher<'_, Value>,
    signature: &Signature<'_>,
    errors: &mut Vec<(ExprId, String)>,
    argument: usize,
    parameter: usize,
    value: Value,
) {
    let expr = value.expr;
    if matcher.assign(argument, parameter, value) {
        let name = signature.parameters[parameter]
            .name()
            .unwrap_or("<positional>");
        errors.push((expr, format!("Multiple arguments for parameter \"{name}\"")));
    }
}
