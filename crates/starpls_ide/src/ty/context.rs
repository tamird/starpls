//! Bazel's optional implementation-parameter contract, supplied to Ty's body binding.

use ruff_python_ast::find_node::covering_node;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor;
use ruff_python_ast::visitor::Visitor;
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::ArgOrKeyword;
use ruff_python_ast::Expr;
use ruff_python_ast::ExprCall;
use ruff_python_ast::Stmt;
use ruff_text_size::Ranged;
use starpls_common::Dialect;
use starpls_hir::Db as _;
use ty_python_core::definition::Definition;
use ty_python_core::definition::DefinitionKind;
use ty_python_core::definition::ParameterDefinitionNodeKind;
use ty_python_semantic::provided::ProvidedClass;
use ty_python_semantic::provided::ProvidedField;
use ty_python_semantic::provided::ProvidedInstanceFields;
use ty_python_semantic::types::ide_support::resolved_call_signature;
use ty_python_semantic::types::ide_support::CallSignatureDetails;
use ty_python_semantic::types::Type;
use ty_python_semantic::HasDefinition;
use ty_python_semantic::HasType;
use ty_python_semantic::SemanticModel;

use super::factory;
use super::factory::Attribute;
use super::factory::AttributeUse;
use super::factory::Factory;
use crate::Database;

pub(super) fn parameter_type<'db>(
    db: &'db Database,
    definition: Definition<'db>,
) -> Option<Type<'db>> {
    if !db.environment().options(db).infer_ctx_attributes {
        return None;
    }
    let file = definition.program_file(db);
    if db.starlark_file(file)?.dialect != Dialect::Bazel {
        return None;
    }
    let DefinitionKind::Parameter(parameter) = definition.kind(db) else {
        return None;
    };
    let ParameterDefinitionNodeKind::Parameter(parameter) = parameter else {
        return None;
    };
    let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
    let parameter = parameter.node(&parsed);
    let function = covering_node(parsed.syntax().into(), parameter.range())
        .ancestors()
        .find_map(|node| match node {
            AnyNodeRef::StmtFunctionDef(function) => Some(function),
            _ => None,
        })?;
    if function.parameters.iter_non_variadic_params().count() != 1
        || function.parameters.vararg.is_some()
        || function.parameters.kwarg.is_some()
    {
        return None;
    }
    let model = SemanticModel::new(db, file);
    let implementation = function.definition(&model);
    let environment = model.program_environment();
    let mut calls = RegistrationCalls::default();
    for statement in parsed.suite() {
        calls.visit_stmt(statement);
    }
    let mut registration = None;
    for call in calls.calls {
        let Some(signature) = resolved_call_signature(&model, call) else {
            continue;
        };
        let Some(declaration) = signature.definition else {
            continue;
        };
        let Some(factory) = factory::declaration(db, declaration) else {
            continue;
        };
        let Factory::Rule { repository } = factory else {
            continue;
        };
        // An unknown expansion could register this callback a second time;
        // uncertainty here prevents a unique schema for the entire module.
        let Some(argument) = argument(call, &signature, "implementation").ok()? else {
            continue;
        };
        let ty = argument.inferred_type(&model)?;
        if !matches!(ty, Type::FunctionLiteral(_)) {
            return None;
        }
        let target = ty
            .definition(db, &environment)
            .and_then(|ty| ty.definition())?;
        if target != implementation {
            continue;
        }
        if registration.is_some() {
            // A callback used with more than one schema has no unique body contract.
            return None;
        }
        registration = Some((call, signature, declaration, repository));
    }
    let (call, signature, declaration, repository) = registration?;
    let declarations = declaration.program_file(db);
    let common = starpls_bazel::attr::make_common_attributes();
    let common = if repository {
        common.repository
    } else {
        common.build
    };
    let attribute_type = |kind| {
        factory::attribute_value_type(
            db,
            &environment,
            declarations,
            kind,
            if repository {
                AttributeUse::RepositoryContext
            } else {
                AttributeUse::BuildContext
            },
        )
    };
    let mut fields = common
        .iter()
        .map(|attribute| {
            Some((
                Name::new(&attribute.name),
                attribute_type(&attribute.r#type)?,
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    if let Some(attrs) = argument(call, &signature, "attrs").ok()? {
        let Expr::Dict(dictionary) = attrs else {
            // Mutable mapping aliases do not carry a proven current schema.
            return None;
        };
        for item in &dictionary.items {
            let name_type = item.key.as_ref()?.inferred_type(&model)?;
            let name = name_type.string_literal_value(db)?;
            let ty = item.value.inferred_type(&model)?;
            let attribute = ty
                .provided_data(db, &environment)?
                .downcast_ref::<Attribute>()?;
            let ty = attribute_type(&attribute.kind)?;
            if let Some((_, existing)) = fields.iter_mut().find(|(field, _)| field.as_str() == name)
            {
                *existing = ty;
            } else {
                fields.push((Name::new(name), ty));
            }
        }
    }
    let make_class = |name, base, fields: Box<[(Name, Type<'db>)]>| {
        let class = model.provided_class_at_call(
            call,
            ProvidedClass {
                name: Name::new(name),
                bases: vec![factory::native_class(db, declarations, base)?].into_boxed_slice(),
                class_members: Box::default(),
                instance_fields: ProvidedInstanceFields {
                    fields: fields
                        .into_vec()
                        .into_iter()
                        .map(|(name, ty)| ProvidedField {
                            name,
                            ty,
                            source: None,
                        })
                        .collect(),
                    has_dynamic_fields: false,
                    data: None,
                },
            },
        )?;
        class.to_instance_approximation(db, &environment)
    };
    let attrs = make_class("attributes", "struct", fields.into_boxed_slice())?;
    let name = if repository { "repository_ctx" } else { "ctx" };
    make_class(
        name,
        name,
        vec![(Name::new("attr"), attrs)].into_boxed_slice(),
    )
}

/// Select a definite source argument using Ty's existing binding. Expanded
/// arguments can supply or overwrite either field, so they leave this policy gradual.
fn argument<'a>(
    call: &'a ExprCall,
    signature: &CallSignatureDetails<'_>,
    name: &str,
) -> Result<Option<&'a Expr>, ()> {
    let parameter = signature
        .parameters
        .iter()
        .position(|parameter| parameter.name == name)
        .ok_or(())?;
    let mut selected = None;
    for (index, argument) in call.arguments.iter_source_order().enumerate() {
        let expression = match argument {
            ArgOrKeyword::Arg(expression) => {
                if matches!(expression, Expr::Starred(_)) {
                    return Err(());
                }
                expression
            }
            ArgOrKeyword::Keyword(keyword) => {
                keyword.arg.as_ref().ok_or(())?;
                &keyword.value
            }
        };
        if signature.argument_to_displayed_parameter_mapping.get(index) == Some(&Some(parameter)) {
            if selected.is_some() {
                return Err(());
            }
            selected = Some(expression);
        }
    }
    Ok(selected)
}

#[derive(Default)]
struct RegistrationCalls<'a> {
    calls: Vec<&'a ExprCall>,
}

impl<'a> Visitor<'a> for RegistrationCalls<'a> {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        // Only module-executed registrations establish this opt-in contract.
        // Inferring deferred bodies here could re-enter the parameter being supplied.
        if !matches!(statement, Stmt::FunctionDef(_) | Stmt::ClassDef(_)) {
            visitor::walk_stmt(self, statement);
        }
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        match expression {
            Expr::Lambda(_) => {}
            Expr::Call(call) => {
                // Skip calls that cannot supply an implementation argument.
                // The shared matcher still decides every candidate's meaning.
                if !call.arguments.args.is_empty()
                    || call.arguments.keywords.iter().any(|keyword| {
                        keyword
                            .arg
                            .as_ref()
                            .is_none_or(|name| name.as_str() == "implementation")
                    })
                {
                    self.calls.push(call);
                }
                visitor::walk_expr(self, expression);
            }
            _ => visitor::walk_expr(self, expression),
        }
    }
}
