//! Check selected implementations with annotations borrowed from trusted stubs.

use std::collections::hash_map::Entry;
use std::panic::AssertUnwindSafe;

use ruff_db::diagnostic::Annotation;
use ruff_db::diagnostic::Diagnostic;
use ruff_db::diagnostic::DiagnosticId;
use ruff_db::diagnostic::Severity;
use ruff_db::diagnostic::Span;
use ruff_python_ast::name::Name;
use ruff_python_ast::Expr;
use ruff_python_ast::HasNodeIndex;
use ruff_python_ast::NodeIndex;
use ruff_python_ast::Parameter;
use ruff_python_ast::Stmt;
use ruff_python_ast::StmtFunctionDef;
use ruff_python_ast::UnaryOp;
use ruff_text_size::Ranged;
use ruff_text_size::TextRange;
use rustc_hash::FxHashMap;
use salsa::Setter;
use starpls_common::File;
use starpls_hir::Db as _;
use starpls_hir::ProviderContract;
use starpls_hir::StubValidation;
use ty_python_core::definition::BindingsOwner;
use ty_python_core::definition::Definition;
use ty_python_core::definition::DefinitionKind;
use ty_python_core::definition::DefinitionNodeKey;
use ty_python_core::global_scope;
use ty_python_core::place_table;
use ty_python_core::semantic_index;
use ty_python_core::use_def_map;
use ty_python_core::ProgramFile;
use ty_python_semantic::provided::ProvidedBindingValue;
use ty_python_semantic::types::CallableTypeKind;
use ty_python_semantic::types::ParameterKind;
use ty_python_semantic::types::Signature;
use ty_python_semantic::types::Type;
use ty_python_semantic::types::TypeDefinition;
use ty_python_semantic::types::TypedDictOpenness;
use ty_python_semantic::HasType;
use ty_python_semantic::SemanticModel;

use super::diagnostics::INCOMPLETE_STUB_VALIDATION;
use super::diagnostics::INVALID_STUB_IMPLEMENTATION;
use super::interface::export_definitions;
use crate::Analysis;
use crate::Cancellable;
use crate::Database;

type Reports = FxHashMap<File, Vec<Diagnostic>>;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Function {
    file: File,
    key: DefinitionNodeKey,
}

struct ValueContract {
    source: File,
    stub: File,
    name: Name,
    range: TextRange,
    initializer_checked: bool,
}

struct Contract {
    source: Function,
    stub: Function,
    name: Name,
    range: TextRange,
    provider: Option<ProviderContract>,
}

impl Analysis {
    /// Validate selected registered implementations without changing caller contracts.
    /// Syntax correspondence is scoped to this operation and rebuilt after every edit.
    pub fn validate_stubs(
        &mut self,
        selected: impl Fn(&std::path::Path) -> bool,
    ) -> Cancellable<Vec<(File, Vec<Diagnostic>)>> {
        let Self { db } = self;
        let environment = db.environment();
        let interfaces = environment.type_interfaces(db).clone();
        let previous = environment.stub_validation(db).clone();
        let sources: Vec<_> = interfaces
            .values()
            .map(|(source, _)| *source)
            .filter(|source| source.api_context() != Some(starpls_bazel::APIContext::Build))
            .filter(|source| selected(source.path(db)))
            .collect();
        if sources.is_empty() {
            return Ok(Vec::new());
        }
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let mut reports = Reports::default();
            environment.set_type_interfaces(db).to(interfaces
                .iter()
                .filter(|(_, (source, _))| {
                    source.api_context() == Some(starpls_bazel::APIContext::Build)
                })
                .map(|(path, pair)| (*path, *pair))
                .collect());
            environment
                .set_stub_validation(db)
                .to(StubValidation::default());
            let mut contracts = Vec::new();
            let mut values = Vec::new();
            for source in &sources {
                let Some(&(_, stub)) = interfaces.get(&source.source) else {
                    continue;
                };
                reports.entry(*source).or_default();
                reports.entry(stub).or_default();
                discover(
                    db,
                    *source,
                    stub,
                    &selected,
                    &mut contracts,
                    &mut values,
                    &mut reports,
                );
            }
            let mut owners = FxHashMap::default();
            for contract in &contracts {
                match owners.entry(contract.source) {
                    Entry::Vacant(entry) => {
                        entry.insert(Ok(contract.stub));
                    }
                    Entry::Occupied(mut entry) => {
                        if *entry.get() != Ok(contract.stub) {
                            *entry.get_mut() = Err("the same function has multiple stub contracts");
                        }
                    }
                }
            }
            let mut validation = StubValidation::default();
            for (&source, owner) in &mut owners {
                if let Ok(stub) = *owner {
                    let provider = contracts
                        .iter()
                        .find(|contract| contract.source == source && contract.stub == stub)
                        .and_then(|contract| contract.provider.as_ref());
                    if let Err(reason) = annotations(db, source, stub, provider, &mut validation) {
                        *owner = Err(reason);
                    }
                }
            }
            for contract in &contracts {
                let Contract {
                    source,
                    stub,
                    name,
                    range,
                    provider: _,
                } = contract;
                reports.entry(source.file).or_default();
                if let Err(reason) = owners[source] {
                    let model = SemanticModel::new(db, db.starlark_program_file(source.file));
                    if let Err(ContractError::Incompatible(message)) = compare_function(
                        db,
                        source.file,
                        name,
                        model.definition_type(function_definition(db, *source)),
                        model.definition_type(function_definition(db, *stub)),
                    ) {
                        report(&mut reports, source.file, *range, false, message);
                    }
                    report(
                        &mut reports,
                        source.file,
                        *range,
                        true,
                        format!("Cannot validate `{name}`: {reason}"),
                    );
                }
            }
            for value in &mut values {
                value.initializer_checked = value_annotation(db, value, &mut validation).is_some();
            }
            validation
                .files
                .extend(reports.keys().map(|file| file.source));
            environment.set_stub_validation(db).to(validation);
            // Infer exported values from source with the borrowed signatures installed.
            // Keeping redirection disabled here avoids proving a reexport against itself.
            for value in values {
                let ValueContract {
                    source,
                    stub,
                    name,
                    range,
                    initializer_checked,
                } = value;
                if initializer_checked {
                    // Ordinary assignment diagnostics check the initializer. Its contextual
                    // binding type would only repeat the borrowed contract here.
                    continue;
                }
                let actual = ProvidedBindingValue::Export {
                    file: db.starlark_program_file(source),
                    name: name.clone(),
                }
                .resolve_type(db);
                let expected = ProvidedBindingValue::Export {
                    file: db.starlark_program_file(stub),
                    name: name.clone(),
                }
                .resolve_type(db);
                if let (Some(actual), Some(expected)) = (actual, expected) {
                    let environment = ty_python_semantic::ProgramEnvironment::from_file(
                        db.starlark_program_file(stub),
                    );
                    let result = if matches!(expected, Type::ClassLiteral(_))
                        && super::interface::provider_definition(db, expected, &environment)
                            .is_some()
                    {
                        compare_provider(db, source, stub, &name, actual, expected, None)
                    } else if matches!(actual, Type::NominalInstance(_))
                        && actual.provided_data(db, &environment).is_some_and(|data| {
                            data.downcast_ref::<super::factory::ProviderData>()
                                .is_some()
                        })
                    {
                        let origin =
                            super::interface::provider_implementation(db, source, expected);
                        let data = actual
                            .provided_data(db, &environment)
                            .and_then(|data| data.downcast_ref::<super::factory::ProviderData>())
                            .expect("provider instance metadata checked above");
                        if origin.is_some_and(|(_, origin)| origin == data.origin) {
                            Err(ContractError::Incomplete(format!("Cannot prove `{name}`: original provider instances do not retain field-value evidence")))
                        } else {
                            compare(db, stub, &name, actual, expected)
                        }
                    } else if actual.provided_data(db, &environment).is_some_and(|data| {
                        data.downcast_ref::<super::factory::ProviderData>()
                            .is_some()
                    }) {
                        match callable_signature(db, &environment, expected) {
                            Some(signature) => compare_provider(db, source, stub, &name, actual, signature.return_type(), Some(signature)),
                            None => Err(ContractError::Incomplete(format!("Cannot validate `{name}`: raw constructor has no single callable contract"))),
                        }
                    } else {
                        compare(db, stub, &name, actual, expected)
                    };
                    if let Err(error) = result {
                        let error = if ty_python_semantic::types::any_over_type(
                            db,
                            &environment,
                            expected,
                            expected.is_fully_static(db, &environment),
                            |ty| matches!(ty, Type::TypedDict(_)),
                        ) {
                            ContractError::Incomplete(format!(
                                "Cannot prove `{name}`: inferred type `{}` does not establish the dictionary fields of `{}`",
                                actual.display(db, &environment),
                                expected.display(db, &environment),
                            ))
                        } else {
                            error
                        };
                        error.report(&mut reports, stub, range);
                    }
                } else {
                    report(
                        &mut reports,
                        stub,
                        range,
                        true,
                        format!("Cannot resolve the contract for `{name}`"),
                    );
                }
            }
            environment.set_type_interfaces(db).to(interfaces.clone());
            for contract in contracts {
                let Contract {
                    source,
                    stub,
                    name,
                    range,
                    provider,
                } = contract;
                if owners[&source].is_err() {
                    continue;
                }
                let model = SemanticModel::new(db, db.starlark_program_file(source.file));
                let actual = model.definition_type(function_definition(db, source));
                let expected = model.definition_type(function_definition(db, stub));
                let result = if let Some(ProviderContract {
                    stub: file,
                    class,
                    allowed_fields: _,
                }) = provider
                {
                    compare_initializer(db, source, stub, &name, file, class)
                } else {
                    compare_function_body(db, source, &name, actual, expected)
                };
                if let Err(error) = result {
                    error.report(&mut reports, source.file, range);
                }
            }
            let mut reports: Vec<_> = reports
                .into_iter()
                .map(|(file, diagnostics)| {
                    let mut checked: Vec<_> = starpls_hir::diagnostics_for_file(db, file)
                        .take(128)
                        .collect();
                    checked.extend(super::diagnostics::check_with_diagnostics(
                        db,
                        file,
                        diagnostics,
                    ));
                    (file, checked)
                })
                .collect();
            reports.sort_by(|(left, _), (right, _)| left.path(db).cmp(right.path(db)));
            reports
        }));
        environment.set_type_interfaces(db).to(interfaces);
        environment.set_stub_validation(db).to(previous);
        salsa::Cancelled::catch(AssertUnwindSafe(|| match result {
            Ok(reports) => reports,
            Err(payload) => std::panic::resume_unwind(payload),
        }))
    }
}

fn discover(
    db: &Database,
    source: File,
    stub: File,
    selected: &impl Fn(&std::path::Path) -> bool,
    contracts: &mut Vec<Contract>,
    values: &mut Vec<ValueContract>,
    reports: &mut Reports,
) {
    let stub_program = db.starlark_program_file(stub);
    let source_program = db.starlark_program_file(source);
    let table = place_table(db, global_scope(db, stub_program));
    let model = SemanticModel::new(db, source_program);
    for symbol in table.symbols() {
        let name = symbol.name();
        let definitions = export_definitions(db, stub_program, name);
        let Some(declaration) = definitions.first() else {
            continue;
        };
        let parsed = ruff_db::parsed::parsed_module(db, stub_program.python_file(db)).load(db);
        let range = declaration.kind(db).target_range(&parsed);
        let actual_definitions = export_definitions(db, source_program, name);
        if name.starts_with('_') {
            // Private helper types belong only to the stub. A function declaration
            // can also describe a private implementation when that name exists.
            if !super::interface::is_function_contract(db, *declaration, source_program) {
                continue;
            }
        }
        if actual_definitions.is_empty() {
            report(
                reports,
                stub,
                range,
                false,
                format!("Implementation does not export `{name}`"),
            );
            continue;
        }
        if !definitely_bound(db, source_program, name) {
            report(
                reports,
                stub,
                range,
                true,
                format!("Cannot validate `{name}`: the implementation may leave it undefined"),
            );
            continue;
        }
        let expected = ProvidedBindingValue::Export {
            file: stub_program,
            name: name.clone(),
        }
        .resolve_type(db);
        let actual = ProvidedBindingValue::Export {
            file: source_program,
            name: name.clone(),
        }
        .resolve_type(db);
        let (Some(actual), Some(expected)) = (actual, expected) else {
            report(
                reports,
                stub,
                range,
                true,
                format!("Cannot resolve the contract for `{name}`"),
            );
            continue;
        };
        if matches!(expected, Type::ClassLiteral(_))
            && super::interface::provider_definition(db, expected, &model.program_environment())
                .is_some()
        {
            match provider_initializer(db, source, stub, name, actual, expected) {
                Ok(Some(contract)) => {
                    if selected(contract.source.file.path(db)) {
                        contracts.push(contract);
                    }
                }
                Ok(None) => {}
                Err(error) => error.report(reports, stub, range),
            }
        }
        let raw_provider = matches!(actual, Type::Callable(_))
            && actual
                .provided_data(db, &model.program_environment())
                .is_some_and(|data| {
                    data.downcast_ref::<super::factory::ProviderData>()
                        .is_some()
                });
        if raw_provider {
            values.push(ValueContract {
                source,
                stub,
                name: name.clone(),
                range,
                initializer_checked: false,
            });
        } else if let Some(TypeDefinition::Function(stub_definition)) =
            expected.definition(db, &model.program_environment())
        {
            let Some(TypeDefinition::Function(source_definition)) =
                actual.definition(db, &model.program_environment())
            else {
                if let Err(ContractError::Incompatible(message)) =
                    compare_function(db, stub, name, actual, expected)
                {
                    report(reports, stub, range, false, message);
                }
                report(
                    reports,
                    stub,
                    range,
                    true,
                    format!("Cannot validate `{name}`: no single implementation function"),
                );
                continue;
            };
            let (Some(source_function), Some(stub_function)) = (
                function(db, source_definition),
                function(db, stub_definition),
            ) else {
                report(
                    reports,
                    stub,
                    range,
                    true,
                    format!("Cannot validate `{name}`: function source is unavailable"),
                );
                continue;
            };
            if !selected(source_function.file.path(db)) {
                continue;
            }
            let parsed =
                ruff_db::parsed::parsed_module(db, source_definition.python_file(db)).load(db);
            contracts.push(Contract {
                source: source_function,
                stub: stub_function,
                name: name.clone(),
                range: source_definition.kind(db).target_range(&parsed),
                provider: None,
            });
        } else {
            values.push(ValueContract {
                source,
                stub,
                name: name.clone(),
                range,
                initializer_checked: false,
            });
        }
    }
}

/// Borrow context only after checking independent evidence and absence of shared values.
fn value_annotation(
    db: &Database,
    value: &ValueContract,
    validation: &mut StubValidation,
) -> Option<()> {
    let ValueContract {
        source,
        stub,
        name,
        range: _,
        initializer_checked: _,
    } = value;
    let source_program = db.starlark_program_file(*source);
    let table = place_table(db, global_scope(db, source_program));
    let symbol = table.symbol_by_name(name)?;
    if symbol.is_used() || symbol.is_reassigned() {
        return None;
    }
    let definitions = export_definitions(db, source_program, name);
    let [definition] = definitions.as_slice() else {
        return None;
    };
    let DefinitionKind::Assignment(assignment) = definition.kind(db) else {
        return None;
    };
    if assignment.owner() != BindingsOwner::Definition {
        return None;
    }
    let parsed = ruff_db::parsed::parsed_module(db, source_program.python_file(db)).load(db);
    let target = assignment.target(&parsed);
    if starpls_hir::Source::new(db)
        .type_comment_annotation(*source, target.node_index().load())
        .is_some()
    {
        return None;
    }
    let model = SemanticModel::new(db, source_program);
    let expected = ProvidedBindingValue::Export {
        file: db.starlark_program_file(*stub),
        name: name.clone(),
    }
    .resolve_type(db)?;
    if !expected.is_fully_static(db, &model.program_environment()) {
        return None;
    }
    // Literal initialization rejects hidden keys that an open structural contract permits.
    if ty_python_semantic::types::any_over_type(
        db,
        &model.program_environment(),
        expected,
        true,
        |ty| {
            let Type::TypedDict(dictionary) = ty else {
                return false;
            };
            matches!(dictionary.openness(db), TypedDictOpenness::ImplicitlyOpen)
        },
    ) {
        return None;
    }
    if !has_fresh_literal_evidence(&model, assignment.value(&parsed)) {
        return None;
    }
    let stub_program = db.starlark_program_file(*stub);
    let declarations = export_definitions(db, stub_program, name);
    let [declaration] = declarations.as_slice() else {
        return None;
    };
    let parsed = ruff_db::parsed::parsed_module(db, stub_program.python_file(db)).load(db);
    let statement = parsed.suite().iter().find_map(|statement| {
        let Stmt::AnnAssign(statement) = statement else {
            return None;
        };
        (semantic_index(db, stub_program).try_definition(statement) == Some(*declaration))
            .then_some(statement)
    })?;
    validation.annotations.insert(
        (source.source, target.node_index().load()),
        (*stub, statement.node_index().load()),
    );
    Some(())
}

/// Literal containers have fresh storage, including when empty; their children supply
/// the independent value evidence before contextual checking introduces the stub shape.
fn has_fresh_literal_evidence(model: &SemanticModel<'_>, expression: &Expr) -> bool {
    match expression {
        Expr::List(list) => list
            .elts
            .iter()
            .all(|item| has_fresh_literal_evidence(model, item)),
        Expr::Tuple(tuple) => tuple
            .elts
            .iter()
            .all(|item| has_fresh_literal_evidence(model, item)),
        Expr::Dict(dict) => dict.items.iter().all(|item| {
            item.key
                .as_ref()
                .is_some_and(|key| has_fresh_literal_evidence(model, key))
                && has_fresh_literal_evidence(model, &item.value)
        }),
        Expr::UnaryOp(unary) => {
            matches!(unary.op, UnaryOp::UAdd | UnaryOp::USub)
                && unary.operand.is_number_literal_expr()
                && expression_has_static_evidence(model, expression)
        }
        Expr::StringLiteral(_)
        | Expr::NumberLiteral(_)
        | Expr::BooleanLiteral(_)
        | Expr::NoneLiteral(_) => expression_has_static_evidence(model, expression),
        _ => false,
    }
}

fn definitely_bound(db: &Database, file: ProgramFile<'_>, name: &str) -> bool {
    let scope = global_scope(db, file);
    let Some(symbol) = place_table(db, scope).symbol_id(name) else {
        return false;
    };
    use_def_map(db, scope)
        .end_of_scope_symbol_bindings(symbol)
        .all(|binding| binding.binding.definition().is_some())
}

fn provider_initializer(
    db: &Database,
    source: File,
    stub: File,
    name: &str,
    actual: Type<'_>,
    expected: Type<'_>,
) -> Result<Option<Contract>, ContractError> {
    let model = SemanticModel::new(db, db.starlark_program_file(source));
    let environment = model.program_environment();
    let Some(data) = actual
        .provided_data(db, &environment)
        .and_then(|data| data.downcast_ref::<super::factory::ProviderData>())
    else {
        return Ok(None);
    };
    let super::factory::ProviderInitializer::Expression(expression) = data.initializer else {
        return Ok(None);
    };
    let incomplete =
        |reason: &str| ContractError::Incomplete(format!("Cannot validate `{name}`: {reason}"));
    let allowed = data
        .fields
        .clone()
        .ok_or_else(|| incomplete("initializer has an unrestricted field schema"))?;
    let (program, _) = super::interface::provider_implementation(db, source, expected)
        .ok_or_else(|| incomplete("provider source is unavailable"))?;
    let parsed = ruff_db::parsed::parsed_module(db, program.python_file(db)).load(db);
    let expression =
        ruff_python_ast::find_node::covering_node(parsed.syntax().into(), expression.range())
            .node()
            .as_expr_ref()
            .ok_or_else(|| incomplete("initializer source is unavailable"))?;
    let model = SemanticModel::new(db, program);
    let ty = expression
        .inferred_type(&model)
        .ok_or_else(|| incomplete("initializer type is unavailable"))?;
    let Some(TypeDefinition::Function(definition)) = ty.definition(db, &environment) else {
        return Err(incomplete("initializer is not a single source function"));
    };
    let source =
        function(db, definition).ok_or_else(|| incomplete("initializer source is unavailable"))?;
    let Some(TypeDefinition::StaticClass(class)) = expected.definition(db, &environment) else {
        return Err(incomplete("provider contract is not a class"));
    };
    let DefinitionKind::Class(class_kind) = class.kind(db) else {
        unreachable!();
    };
    let parsed = ruff_db::parsed::parsed_module(db, class.python_file(db)).load(db);
    let class_node = class_kind.node(&parsed);
    let constructor = class_node
        .body
        .iter()
        .find_map(|statement| {
            let ruff_python_ast::Stmt::FunctionDef(function) = statement else {
                return None;
            };
            (function.name.as_str() == "__init__").then_some(function)
        })
        .ok_or_else(|| incomplete("provider class has no constructor declaration"))?;
    let constructor = Function {
        file: stub,
        key: constructor.into(),
    };
    Ok(Some(Contract {
        source,
        stub: constructor,
        name: name.into(),
        range: definition
            .kind(db)
            .target_range(&ruff_db::parsed::parsed_module(db, definition.python_file(db)).load(db)),
        provider: Some(ProviderContract {
            stub,
            class: class_node.node_index().load(),
            allowed_fields: allowed,
        }),
    }))
}

fn function(db: &Database, definition: Definition<'_>) -> Option<Function> {
    let DefinitionKind::Function(kind) = definition.kind(db) else {
        return None;
    };
    let parsed = ruff_db::parsed::parsed_module(db, definition.python_file(db)).load(db);
    Some(Function {
        file: db.starlark_file(definition.program_file(db))?,
        key: kind.node(&parsed).into(),
    })
}

fn function_definition(db: &Database, function: Function) -> Definition<'_> {
    let Function { file, key } = function;
    let [definition] = semantic_index(db, db.starlark_program_file(file)).definitions(key) else {
        unreachable!("a function syntax node has one definition")
    };
    *definition
}

pub(super) fn provider_return_type<'db>(
    db: &'db Database,
    definition: Definition<'db>,
) -> Option<ty_python_semantic::provided::ProvidedReturnType<'db>> {
    let DefinitionKind::Function(function) = definition.kind(db) else {
        return None;
    };
    let parsed = ruff_db::parsed::parsed_module(db, definition.python_file(db)).load(db);
    let node = function.node(&parsed);
    let ProviderContract {
        stub,
        class: owner,
        allowed_fields: allowed,
    } = db
        .environment()
        .stub_validation(db)
        .provider_returns
        .get(&(definition.file(db), node.node_index().load()))?;
    let program = db.starlark_program_file(*stub);
    let parsed = ruff_db::parsed::parsed_module(db, program.python_file(db)).load(db);
    let ruff_python_ast::AnyRootNodeRef::Stmt(ruff_python_ast::Stmt::ClassDef(class)) =
        parsed.get_by_index(*owner)
    else {
        return None;
    };
    let [definition] = semantic_index(db, program).definitions(class) else {
        return None;
    };
    let model = SemanticModel::new(db, program);
    let fields = super::interface::provider_fields(
        db,
        model.definition_type(*definition),
        &model.program_environment(),
    )?;
    let mut schema: ty_python_semantic::types::TypedDictSchema = fields
        .into_iter()
        .map(|field| {
            (
                field.name,
                ty_python_semantic::types::TypedDictFieldBuilder::new(field.ty)
                    .required(true)
                    .build(),
            )
        })
        .collect();
    let object =
        ty_python_semantic::types::KnownClass::Object.to_instance(db, &model.program_environment());
    for name in allowed {
        schema.entry(name.clone()).or_insert_with(|| {
            ty_python_semantic::types::TypedDictFieldBuilder::new(object)
                .required(false)
                .build()
        });
    }
    Some(ty_python_semantic::provided::ProvidedReturnType {
        ty: Type::TypedDict(ty_python_semantic::types::TypedDictType::from_schema_items(
            db, schema,
        )),
        source: Some(ruff_db::files::FileRange::new(
            stub.source,
            class.name.range,
        )),
    })
}

fn annotations(
    db: &Database,
    source: Function,
    stub: Function,
    provider: Option<&ProviderContract>,
    validation: &mut StubValidation,
) -> Result<(), &'static str> {
    let source_definition = function_definition(db, source);
    let stub_definition = function_definition(db, stub);
    let DefinitionKind::Function(source_kind) = source_definition.kind(db) else {
        unreachable!()
    };
    let DefinitionKind::Function(stub_kind) = stub_definition.kind(db) else {
        unreachable!()
    };
    let source_parsed =
        ruff_db::parsed::parsed_module(db, source_definition.python_file(db)).load(db);
    let stub_parsed = ruff_db::parsed::parsed_module(db, stub_definition.python_file(db)).load(db);
    let source_node = source_kind.node(&source_parsed);
    let stub_node = stub_kind.node(&stub_parsed);
    if source_node.type_params.is_some()
        || stub_node.type_params.is_some()
        || source_kind.has_decorators()
        || stub_kind.has_decorators()
    {
        return Err("generic or decorated functions require a dedicated implementation contract");
    }
    let pairs = parameter_pairs_with_receiver(source_node, stub_node, provider.is_some())?;
    if let Some(provider) = provider {
        validation.provider_returns.insert(
            (source.file.source, source_node.node_index().load()),
            provider.clone(),
        );
    } else if stub_node.returns.is_some() {
        validation.annotations.insert(
            (source.file.source, source_node.node_index().load()),
            (stub.file, stub_node.node_index().load()),
        );
    }
    for (parameter, annotation) in pairs {
        if annotation.annotation.is_none() {
            continue;
        }
        validation.annotations.insert(
            (source.file.source, parameter.node_index().load()),
            (stub.file, annotation.node_index().load()),
        );
    }
    Ok(())
}

pub(crate) fn parameter_pairs<'a>(
    source: &'a StmtFunctionDef,
    stub: &'a StmtFunctionDef,
) -> Result<Vec<(&'a Parameter, &'a Parameter)>, &'static str> {
    parameter_pairs_with_receiver(source, stub, false)
}

fn parameter_pairs_with_receiver<'a>(
    source: &'a StmtFunctionDef,
    stub: &'a StmtFunctionDef,
    receiver: bool,
) -> Result<Vec<(&'a Parameter, &'a Parameter)>, &'static str> {
    let source = &source.parameters;
    let stub = &stub.parameters;
    let skip = usize::from(receiver);
    if source.posonlyargs.len() != stub.posonlyargs.len()
        || source.args.len() + skip != stub.args.len()
        || source.kwonlyargs.len() != stub.kwonlyargs.len()
        || source.vararg.is_some() != stub.vararg.is_some()
        || source.kwarg.is_some() != stub.kwarg.is_some()
    {
        return Err("parameter layouts differ");
    }
    let mut pairs: Vec<_> = source
        .posonlyargs
        .iter()
        .chain(&source.args)
        .zip(stub.posonlyargs.iter().chain(stub.args.iter().skip(skip)))
        .map(|(source, stub)| (&source.parameter, &stub.parameter))
        .collect();
    for parameter in &source.kwonlyargs {
        let Some(annotation) = stub
            .kwonlyargs
            .iter()
            .find(|stub| stub.parameter.name.id == parameter.parameter.name.id)
        else {
            return Err("keyword-only parameter names differ");
        };
        pairs.push((&parameter.parameter, &annotation.parameter));
    }
    if let (Some(source), Some(stub)) = (&source.vararg, &stub.vararg) {
        pairs.push((source, stub));
    }
    if let (Some(source), Some(stub)) = (&source.kwarg, &stub.kwarg) {
        pairs.push((source, stub));
    }
    Ok(pairs)
}

enum ContractError {
    Incompatible(String),
    Incomplete(String),
}

fn callable_signature<'db>(
    db: &'db Database,
    environment: &ty_python_semantic::ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Option<Signature<'db>> {
    let mut signatures = Vec::new();
    ty.map_callable_signatures(
        db,
        environment,
        CallableTypeKind::FunctionLike,
        |signature| {
            signatures.push(signature.clone());
            signature
        },
    )?;
    let [signature] = signatures.as_slice() else {
        return None;
    };
    Some(signature.clone())
}

fn compare_provider<'db>(
    db: &'db Database,
    source: File,
    file: File,
    name: &str,
    actual: Type<'db>,
    expected: Type<'db>,
    raw_signature: Option<Signature<'db>>,
) -> Result<(), ContractError> {
    let environment =
        ty_python_semantic::ProgramEnvironment::from_file(db.starlark_program_file(file));
    let incomplete = || {
        ContractError::Incomplete(format!(
            "Cannot validate `{name}`: no unique provider declaration"
        ))
    };
    let data = actual
        .provided_data(db, &environment)
        .and_then(|data| data.downcast_ref::<super::factory::ProviderData>())
        .ok_or_else(|| {
            ContractError::Incomplete(format!(
                "Cannot validate `{name}`: original provider schema is unavailable ({})",
                actual.display(db, &environment)
            ))
        })?;
    let (_, origin) =
        super::interface::provider_implementation(db, source, expected).ok_or_else(|| {
            ContractError::Incomplete(format!(
                "Cannot validate `{name}`: source export has no unique provider origin"
            ))
        })?;
    if raw_signature.is_some()
        && !super::interface::provider_export_origin(
            db,
            source,
            name,
            super::interface::ProviderPart::Raw,
        )
        .is_some_and(|(_, raw_origin)| raw_origin == data.origin)
    {
        return Err(ContractError::Incomplete(format!(
            "Cannot validate `{name}`: raw constructor has no unique provider origin"
        )));
    }
    if origin != data.origin {
        return Err(ContractError::Incomplete(format!(
            "Cannot validate `{name}`: source export does not directly identify its provider declaration"
        )));
    }
    let fields =
        super::interface::provider_fields(db, expected, &environment).ok_or_else(incomplete)?;
    if let Some(allowed) = &data.fields {
        for field in &fields {
            if !allowed.contains(&field.name) {
                return Err(ContractError::Incompatible(format!(
                    "Provider `{name}` does not allow field `{}`",
                    field.name
                )));
            }
        }
    }
    match data.initializer {
        super::factory::ProviderInitializer::None => {}
        super::factory::ProviderInitializer::Expression(_) => {
            if raw_signature.is_none() {
                return Ok(());
            }
        }
        super::factory::ProviderInitializer::Unknown => return Err(incomplete()),
    }
    let signature = raw_signature
        .or_else(|| callable_signature(db, &environment, expected))
        .ok_or_else(|| {
            ContractError::Incomplete(format!(
                "Cannot validate `{name}`: constructor has no single callable signature"
            ))
        })?;
    for parameter in signature.parameters().iter() {
        if matches!(parameter.kind(), ParameterKind::KeywordVariadic { name: _ })
            && data.fields.is_none()
        {
            continue;
        }
        let ParameterKind::KeywordOnly {
            name: parameter_name,
            default_type: _,
        } = parameter.kind()
        else {
            return Err(ContractError::Incompatible(format!(
                "Provider `{name}` accepts only keyword field arguments"
            )));
        };
        if data
            .fields
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(parameter_name))
        {
            return Err(ContractError::Incompatible(format!(
                "Provider `{name}` does not accept constructor argument `{parameter_name}`"
            )));
        }
    }
    for field in fields {
        let parameter = signature.parameters().iter().find(|parameter| matches!(parameter.kind(), ParameterKind::KeywordOnly { name, default_type: None } if name == &field.name));
        let Some(parameter) = parameter else {
            return Err(ContractError::Incompatible(format!(
                "Constructor for `{name}` can omit field `{}`",
                field.name
            )));
        };
        compare(
            db,
            file,
            &format!("{name}.{}", field.name),
            parameter.annotated_type(),
            field.ty,
        )?;
    }
    Ok(())
}

fn compare_initializer<'db>(
    db: &'db Database,
    source: Function,
    stub: Function,
    name: &str,
    class_file: File,
    class_node: NodeIndex,
) -> Result<(), ContractError> {
    let model = SemanticModel::new(db, db.starlark_program_file(source.file));
    let environment = model.program_environment();
    let definition = function_definition(db, source);
    let actual = model.definition_type(definition);
    let expected = model.definition_type(function_definition(db, stub));
    let inputs = |ty: Type<'db>, receiver: bool| {
        ty.map_callable_signatures(
            db,
            &environment,
            CallableTypeKind::FunctionLike,
            |signature| {
                let parameters = ty_python_semantic::types::Parameters::from_annotation(
                    db,
                    signature
                        .parameters()
                        .iter()
                        .skip(usize::from(receiver))
                        .cloned(),
                );
                signature
                    .with_parameters(parameters)
                    .with_return_type(Type::none(db, &environment))
            },
        )
        .unwrap_or(ty)
    };
    compare(
        db,
        source.file,
        name,
        inputs(actual, false),
        inputs(expected, true),
    )?;
    let DefinitionKind::Function(function) = definition.kind(db) else {
        unreachable!();
    };
    let parsed = ruff_db::parsed::parsed_module(db, definition.python_file(db)).load(db);
    let function = function.node(&parsed);
    if function.returns.is_some()
        || starpls_hir::Source::new(db)
            .type_comment_annotation(source.file, function.node_index().load())
            .is_some()
    {
        return Err(ContractError::Incomplete(format!("Cannot prove `{name}`: its initializer return annotation does not establish required field presence")));
    }
    let program = db.starlark_program_file(class_file);
    let parsed_class = ruff_db::parsed::parsed_module(db, program.python_file(db)).load(db);
    let ruff_python_ast::AnyRootNodeRef::Stmt(ruff_python_ast::Stmt::ClassDef(class)) =
        parsed_class.get_by_index(class_node)
    else {
        unreachable!("provider contracts originate in a class declaration");
    };
    let [class] = semantic_index(db, program).definitions(class) else {
        unreachable!("a class declaration has one definition");
    };
    let fields = super::interface::provider_fields(db, model.definition_type(*class), &environment)
        .expect("a provider contract is a static class");
    for field in fields {
        // Contextual return checking can hide gradual evidence in structural
        // contracts. Restrict proof to nominal values and their containers;
        // ordinary Ty diagnostics still check every initializer below.
        if !field.ty.is_fully_static(db, &environment)
            || ty_python_semantic::types::any_over_type(db, &environment, field.ty, false, |ty| {
                !matches!(
                    ty,
                    Type::NominalInstance(_)
                        | Type::ClassLiteral(_)
                        | Type::GenericAlias(_)
                        | Type::Union(_)
                        | Type::LiteralValue(_)
                        | Type::Never
                )
            })
        {
            return Err(ContractError::Incomplete(format!(
                "Cannot prove `{name}`: field `{}` requires a structural or unresolved contract",
                field.name
            )));
        }
    }
    if !has_static_evidence(&model, &function.body) {
        return Err(ContractError::Incomplete(format!("Cannot prove `{name}`: its initializer contains dynamic or unavailable expression types")));
    }
    Ok(())
}

/// Contextual checking of structural returns can hide gradual child types.
fn has_static_evidence(model: &SemanticModel<'_>, body: &[ruff_python_ast::Stmt]) -> bool {
    use ruff_python_ast::visitor::Visitor;
    use ruff_python_ast::visitor::{self};

    struct Evidence<'a, 'db> {
        model: &'a SemanticModel<'db>,
        complete: bool,
    }
    impl<'a> Visitor<'a> for Evidence<'_, '_> {
        fn visit_expr(&mut self, expression: &'a ruff_python_ast::Expr) {
            if !expression_has_static_evidence(self.model, expression) {
                self.complete = false;
            }
            visitor::walk_expr(self, expression);
        }
        fn visit_stmt(&mut self, statement: &'a ruff_python_ast::Stmt) {
            if matches!(
                statement,
                ruff_python_ast::Stmt::FunctionDef(_) | ruff_python_ast::Stmt::ClassDef(_)
            ) {
                self.complete = false;
                return;
            }
            visitor::walk_stmt(self, statement);
        }
    }
    let mut evidence = Evidence {
        model,
        complete: true,
    };
    evidence.visit_body(body);
    evidence.complete
}

fn expression_has_static_evidence(model: &SemanticModel<'_>, expression: &Expr) -> bool {
    let environment = model.program_environment();
    expression.inferred_type(model).is_some_and(|ty| {
        let ty = ty
            .map_callable_signatures(
                model.db(),
                &environment,
                CallableTypeKind::FunctionLike,
                std::convert::identity,
            )
            .unwrap_or(ty);
        ty.is_fully_static(model.db(), &environment)
    })
}

impl ContractError {
    fn report(self, reports: &mut Reports, file: File, range: TextRange) {
        match self {
            Self::Incompatible(message) => report(reports, file, range, false, message),
            Self::Incomplete(message) => report(reports, file, range, true, message),
        }
    }
}

fn compare_function_body<'db>(
    db: &'db Database,
    source: Function,
    name: &str,
    actual: Type<'db>,
    expected: Type<'db>,
) -> Result<(), ContractError> {
    compare_function(db, source.file, name, actual, expected)?;
    let model = SemanticModel::new(db, db.starlark_program_file(source.file));
    let definition = function_definition(db, source);
    let DefinitionKind::Function(function) = definition.kind(db) else {
        unreachable!("function contracts originate in a function declaration");
    };
    let parsed = ruff_db::parsed::parsed_module(db, definition.python_file(db)).load(db);
    if !has_static_evidence(&model, &function.node(&parsed).body) {
        return Err(ContractError::Incomplete(format!(
            "Cannot prove `{name}`: its body contains dynamic or unavailable expression types"
        )));
    }
    Ok(())
}

fn compare_function<'db>(
    db: &'db Database,
    file: File,
    name: &str,
    actual: Type<'db>,
    expected: Type<'db>,
) -> Result<(), ContractError> {
    let model = SemanticModel::new(db, db.starlark_program_file(file));
    let environment = model.program_environment();
    // A stub function declares a callable contract, not a particular function object.
    let signature = |ty: Type<'db>| {
        ty.map_callable_signatures(
            db,
            &environment,
            CallableTypeKind::FunctionLike,
            std::convert::identity,
        )
        .unwrap_or(ty)
    };
    compare(db, file, name, signature(actual), signature(expected))
}

fn compare<'db>(
    db: &'db Database,
    file: File,
    name: &str,
    actual: Type<'db>,
    expected: Type<'db>,
) -> Result<(), ContractError> {
    let model = SemanticModel::new(db, db.starlark_program_file(file));
    let environment = model.program_environment();
    if !actual.is_assignable_to(db, &environment, expected) {
        Err(ContractError::Incompatible(format!(
            "Implementation of `{name}` has type `{}`, which is not assignable to `{}`",
            actual.display(db, &environment),
            expected.display(db, &environment)
        )))
    } else if !actual.is_subtype_of(db, &environment, expected) {
        Err(ContractError::Incomplete(format!(
            "Cannot prove `{name}`: compatibility depends on dynamic or unresolved types"
        )))
    } else {
        Ok(())
    }
}

fn report(reports: &mut Reports, file: File, range: TextRange, incomplete: bool, message: String) {
    let lint = if incomplete {
        &INCOMPLETE_STUB_VALIDATION
    } else {
        &INVALID_STUB_IMPLEMENTATION
    };
    let mut diagnostic = Diagnostic::new(DiagnosticId::Lint(lint.name()), Severity::Error, message);
    diagnostic.annotate(Annotation::primary(
        Span::from(file.source).with_range(range),
    ));
    reports.entry(file).or_default().push(diagnostic);
}

#[cfg(test)]
mod tests {
    use starpls_hir::Db as _;
    use starpls_hir::Fixture;

    use crate::Analysis;

    fn validate(source: &str, stub: &str) -> Vec<String> {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let source = fixture.add_file(&mut analysis.db, "source.bzl", source);
        let stub = fixture.add_file(&mut analysis.db, "source.bzli", stub);
        loader.add_files_from_fixture(&fixture);
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                Default::default(),
            )
            .unwrap();
        analysis.set_type_interfaces([(source, stub)]).unwrap();
        let reports = analysis.validate_stubs(|_| true).unwrap();
        let diagnostics: Vec<_> = reports
            .into_iter()
            .flat_map(|(_, diagnostics)| diagnostics)
            .collect();
        diagnostics
            .into_iter()
            .map(|diagnostic| diagnostic.id().as_str().to_owned())
            .collect()
    }

    #[test]
    fn provider_contracts_check_storage_and_constructor_inputs() {
        let stub = "class Info:\n    value: Final[str]\n    def __init__(self, *, value: str) -> None: ...\n";
        for source in [
            "Info = provider(fields=['value'])\n",
            "Info = provider(fields=['value', 'hidden'])\n",
            "Info = provider()\n",
            "_Info = provider(fields=['value'])\nInfo = _Info\n",
        ] {
            let diagnostics = validate(source, stub);
            assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}");
        }
        let open = stub.replace("value: str) -> None", "value: str, **kwargs: Any) -> None");
        assert!(validate("Info = provider()\n", &open).is_empty());
        for (source, stub, expected) in [
            (
                "Info = provider(fields=['other'])\n",
                stub.to_owned(),
                "invalid-stub-implementation",
            ),
            (
                "names = ['value']\nInfo = provider(fields=names)\n",
                stub.to_owned(),
                "incomplete-stub-validation",
            ),
            (
                "Info = provider(fields=['value'])\n",
                stub.replace("*, value: str", "value: str"),
                "invalid-stub-implementation",
            ),
            (
                "Info = provider(fields=['value'])\n",
                stub.replace("*, value: str", "*, value: str = ..."),
                "invalid-stub-implementation",
            ),
            (
                "Info = provider(fields=['value'])\n",
                stub.replace("*, value: str", "*, value: int"),
                "invalid-stub-implementation",
            ),
        ] {
            let diagnostics = validate(source, &stub);
            assert!(
                diagnostics.iter().any(|id| id == expected),
                "{source}\n{stub}: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn provider_instance_exports_require_storage_evidence() {
        let stub = "class Info:\n    value: Final[str]\n    def __init__(self, *, value: str) -> None: ...\nitem: Info\n";
        for (construction, expected) in [
            ("Info(value='ok')", "incomplete-stub-validation"),
            ("Other(value='ok')", "invalid-stub-implementation"),
            ("Info(value=42)", "invalid-argument-type"),
        ] {
            let source = format!("Info = provider(fields=['value'])\nOther = provider(fields=['value'])\nitem = {construction}\n");
            let diagnostics = validate(&source, stub);
            assert!(
                diagnostics.iter().any(|id| id == expected),
                "{source}: {diagnostics:?}"
            );
            if construction == "Info(value='ok')" {
                assert!(
                    !diagnostics
                        .iter()
                        .any(|id| id == "invalid-stub-implementation"),
                    "{diagnostics:?}"
                );
            }
        }
    }

    #[test]
    fn provider_initializers_and_raw_constructors_have_separate_contracts() {
        let stub = "class Info:\n    value: Final[str]\n    def __init__(self, value: str) -> None: ...\ndef raw(*, value: str) -> Info: ...\n";
        let source = "def _init(value):\n    return {'value': value}\nInfo, raw = provider(fields=['value'], init=_init)\n";
        let diagnostics = validate(source, stub);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        for source in [
            source.replace("{'value': value}", "{'value': 42}"),
            source.replace("{'value': value}", "{}"),
            source.replace(
                "return {'value': value}",
                "if value:\n        return {'value': value}",
            ),
        ] {
            let diagnostics = validate(&source, stub);
            assert!(!diagnostics.is_empty(), "{source}");
            assert!(
                diagnostics
                    .iter()
                    .any(|id| id != "incomplete-stub-validation"),
                "{source}: {diagnostics:?}"
            );
        }
        let dynamic = source.replace("_init(value)", "_init(value: Any)");
        let diagnostics = validate(&dynamic, stub);
        assert!(
            diagnostics
                .iter()
                .any(|id| id == "incomplete-stub-validation"),
            "{diagnostics:?}"
        );
        for raw in [
            "def raw(value: str) -> Info: ...",
            "def raw(*, value: str = ...) -> Info: ...",
            "def raw(*, value: int) -> Info: ...",
        ] {
            let stub = stub.replace("def raw(*, value: str) -> Info: ...", raw);
            let diagnostics = validate(source, &stub);
            assert!(
                diagnostics
                    .iter()
                    .any(|id| id == "invalid-stub-implementation"),
                "{stub}: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn provider_initializer_proof_tracks_gradual_evidence() {
        let stub = "class Info:\n    value: Final[list[int]]\n    def __init__(self, xs: list[int]) -> None: ...\n";
        for source in [
            "def _init(xs):\n    return {'value': xs}\nInfo, _ = provider(fields=['value'], init=_init)\n",
            "def _typed(xs: list[int]) -> list[int]:\n    return xs\ndef _init(xs):\n    return {'value': _typed(xs)}\nInfo, _ = provider(fields=['value'], init=_init)\n",
        ] {
            let diagnostics = validate(source, stub);
            assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}");
        }
        let diagnostics = validate(
            "def _init(files):\n    return {'files': depset(files)}\nFilesInfo, _ = provider(fields=['files'], init=_init)\n",
            "class FilesInfo:\n    files: Final[depset[File]]\n    def __init__(self, files: list[File]) -> None: ...\n",
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        for source in [
            "def _init(xs: list[Any]):\n    return {'value': xs}\nInfo, _ = provider(fields=['value'], init=_init)\n",
            "def _unknown() -> Any: ...\ndef _init(xs):\n    return {'value': [_unknown()]}\nInfo, _ = provider(fields=['value'], init=_init)\n",
            "def _unknown() -> Any: ...\ndef _init(xs):\n    return {**_unknown(), 'value': xs}\nInfo, _ = provider(fields=['value'], init=_init)\n",
            "def _mutate(xs) -> None:\n    xs.append('bad')\ndef _init(xs):\n    _mutate(xs)\n    return {'value': xs}\nInfo, _ = provider(fields=['value'], init=_init)\n",
            "def _init(xs) -> dict[str, list[int]]:\n    return {}\nInfo, _ = provider(fields=['value'], init=_init)\n",
            "def _init(xs): # type: (list[int]) -> dict[str, list[int]]\n    return {}\nInfo, _ = provider(fields=['value'], init=_init)\n",
        ] {
            let diagnostics = validate(source, stub);
            assert!(diagnostics.iter().any(|id| id == "incomplete-stub-validation"), "{source}: {diagnostics:?}");
        }
        for (field, returned) in [
            ("Callable[[int], int]", "_helper"),
            ("list[Callable[[int], int]]", "[_helper]"),
        ] {
            let stub = format!(
                "class Info:\n    value: Final[{field}]\n    def __init__(self) -> None: ...\n"
            );
            let source = format!("def _helper(value):\n    return value\ndef _init():\n    return {{'value': {returned}}}\nInfo, _ = provider(fields=['value'], init=_init)\n");
            let diagnostics = validate(&source, &stub);
            assert!(
                diagnostics
                    .iter()
                    .any(|id| id == "incomplete-stub-validation"),
                "{source}: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn borrowed_annotations_check_bodies_and_source_signatures() {
        for (source, expected) in [
            ("def compute(value):\n    return value + 1\n", None),
            ("compute = 1\n", Some("invalid-stub-implementation")),
            (
                "def compute(value):\n    return 'bad'\n",
                Some("invalid-return-type"),
            ),
            (
                "def compute(value):\n    return value + 'bad'\n",
                Some("unsupported-operator"),
            ),
            (
                "def compute(value='bad'):\n    return value\n",
                Some("invalid-parameter-default"),
            ),
            (
                "def compute(renamed):\n    return renamed\n",
                Some("invalid-stub-implementation"),
            ),
            (
                "def compute(value: string) -> string:\n    return value\n",
                Some("invalid-stub-implementation"),
            ),
            (
                "def compute(*args, **kwargs):\n    return 1\n",
                Some("incomplete-stub-validation"),
            ),
        ] {
            let diagnostics = validate(source, "def compute(value: int) -> int: ...\n");
            if let Some(expected) = expected {
                assert!(
                    diagnostics.iter().any(|id| id == expected),
                    "{source}: {diagnostics:?}"
                );
            } else {
                assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}");
            }
        }
        let diagnostics = validate(
            "compute = lambda value: 1\n",
            "def compute(value: int) -> int: ...\n",
        );
        assert!(
            diagnostics
                .iter()
                .any(|id| id == "incomplete-stub-validation"),
            "{diagnostics:?}"
        );
        assert!(
            !diagnostics
                .iter()
                .any(|id| id == "invalid-stub-implementation"),
            "{diagnostics:?}"
        );
        assert!(validate(
            "def compute(*args, **kwargs):\n    return len(args) + len(kwargs)\n",
            "def compute(*args: int, **kwargs: string) -> int: ...\n"
        )
        .is_empty());
    }

    #[test]
    fn private_function_contracts_check_bodies_and_callers() {
        let stub = "class _Row(TypedDict):\n    value: int\ndef _compute(value: int) -> int: ...\ndef public(value: int) -> int: ...\n";
        for (implementation, expected) in [
            ("def _compute(value): return value + 1\n", None),
            (
                "def _compute(value): return 'bad'\n",
                Some("invalid-return-type"),
            ),
            ("_compute = 1\n", Some("invalid-stub-implementation")),
        ] {
            let source =
                format!("{implementation}def public(value):\n    return _compute(value)\n");
            let diagnostics = validate(&source, stub);
            if let Some(expected) = expected {
                assert!(
                    diagnostics.iter().any(|id| id == expected),
                    "{source}: {diagnostics:?}"
                );
                assert!(
                    diagnostics.iter().all(|id| id != "unused-definition"),
                    "{source}: {diagnostics:?}"
                );
            } else {
                assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}");
            }
        }
    }

    #[test]
    fn private_contract_usage_follows_edits_and_validation_lifetime() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let source = fixture.add_file(&mut analysis.db, "source.bzl", "");
        let stub = fixture.add_file(&mut analysis.db, "source.bzli", "");
        loader.add_files_from_fixture(&fixture);
        analysis.set_type_interfaces([(source, stub)]).unwrap();
        let previous = analysis
            .db
            .environment()
            .stub_validation(&analysis.db)
            .clone();
        for (prefix, name, expected_unused) in [
            ("", "_compute", 0),
            ("# moved\n", "_compute", 0),
            ("", "_other", 1),
            ("\n", "_compute", 0),
        ] {
            analysis.update_file(
                source,
                format!("def {name}(value): return value + 1\nresult = {name}(1)\n"),
            );
            analysis.update_file(
                stub,
                format!("{prefix}def _compute(value: int) -> int: ...\n"),
            );
            let reports = analysis.validate_stubs(|_| true).unwrap();
            assert_eq!(
                analysis.db.environment().stub_validation(&analysis.db),
                &previous
            );
            let diagnostics: Vec<_> = reports
                .into_iter()
                .flat_map(|(_, diagnostics)| diagnostics)
                .collect();
            assert_eq!(diagnostics.len(), expected_unused, "{diagnostics:?}");
            assert!(diagnostics
                .iter()
                .all(|diagnostic| diagnostic.id().as_str() == "unused-definition"));
            let diagnostics = analysis.snapshot().diagnostics(stub).unwrap();
            assert_eq!(diagnostics.len(), expected_unused, "{diagnostics:?}");
            assert!(diagnostics
                .iter()
                .all(|diagnostic| diagnostic.id().as_str() == "unused-definition"));
        }
    }

    #[test]
    fn keyword_parameters_match_names_across_source_locations() {
        let stub = "def compute(*, label: str, count: int) -> str: ...\n";
        for (source, expected) in [
            (
                "# Source and stub have different offsets and parameter orders.\ndef compute(*, count, label):\n    return label * count\n",
                None,
            ),
            (
                "def compute(*, count, label):\n    return count + label\n",
                Some("unsupported-operator"),
            ),
            (
                "def compute(*, count, renamed):\n    return renamed * count\n",
                Some("incomplete-stub-validation"),
            ),
        ] {
            let diagnostics = validate(source, stub);
            if let Some(expected) = expected {
                assert!(
                    diagnostics.iter().any(|id| id == expected),
                    "{source}: {diagnostics:?}"
                );
            } else {
                assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}");
            }
        }
    }

    #[test]
    fn typed_dictionary_returns_validate_source_values() {
        let stub = "class _Row(TypedDict):\n    name: str\n    count: NotRequired[int]\ndef make() -> _Row: ...\n";
        for (source, expected) in [
            ("def make(): return {'name': 'ok'}\n", None),
            ("def make(): return {'name': 'ok', 'count': 1}\n", None),
            ("def make(): return {}\n", Some("missing-typed-dict-key")),
            (
                "def make(): return {'name': 1}\n",
                Some("invalid-argument-type"),
            ),
            (
                "def make(): return {'name': 'ok', 'count': 'bad'}\n",
                Some("invalid-argument-type"),
            ),
            (
                "def opaque(): pass\ndef make(): return opaque()\n",
                Some("incomplete-stub-validation"),
            ),
            (
                "def opaque(): pass\ndef make(): return {'name': opaque()}\n",
                Some("incomplete-stub-validation"),
            ),
        ] {
            let diagnostics = validate(source, stub);
            if let Some(expected) = expected {
                assert!(
                    diagnostics.iter().any(|id| id == expected),
                    "{source}: {diagnostics:?}"
                );
            } else {
                assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}");
            }
        }
        let diagnostics = validate(
            "def opaque(): pass\ndef make(): return [{'name': opaque()}]\n",
            &stub.replace("-> _Row", "-> list[_Row]"),
        );
        assert!(
            diagnostics
                .iter()
                .any(|id| id == "incomplete-stub-validation"),
            "{diagnostics:?}"
        );
        let diagnostics = validate(
            "def identity(value): return value\ndef make(): return {'callback': identity}\n",
            "class _Row(TypedDict):\n    callback: Callable[[int], int]\ndef make() -> _Row: ...\n",
        );
        assert!(
            diagnostics
                .iter()
                .any(|id| id == "incomplete-stub-validation"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn typed_dictionary_variables_check_fresh_initializers() {
        let schema = "class _Row(TypedDict, closed=True):\n    name: str\n    tags: NotRequired[list[str]]\n";
        for (source, annotation) in [
            ("ROWS = [{'name': 'ok'}]\n", "list[_Row]"),
            (
                "ROWS = {'first': {'name': 'ok', 'tags': []}}\n",
                "dict[str, _Row]",
            ),
            ("ROWS = {}\n", "dict[str, _Row]"),
            ("ROWS = []\n", "list[_Row]"),
        ] {
            let diagnostics = validate(source, &format!("{schema}ROWS: {annotation}\n"));
            assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}");
        }
        for (source, expected) in [
            ("ROWS = [{}]\n", "missing-typed-dict-key"),
            ("ROWS = [{'name': 1}]\n", "invalid-argument-type"),
            (
                "ROWS = [{'name': 'ok', 'name': 1}]\n",
                "invalid-argument-type",
            ),
            (
                "ROWS = [{'name': 'ok', 'tags': [1]}]\n",
                "invalid-argument-type",
            ),
            ("ROWS = [{'name': 'ok'}, 1]\n", "invalid-assignment"),
        ] {
            let diagnostics = validate(source, &format!("{schema}ROWS: list[_Row]\n"));
            assert!(
                diagnostics.iter().any(|id| id == expected),
                "{source}: {diagnostics:?}"
            );
            assert!(
                !diagnostics
                    .iter()
                    .any(|id| id == "incomplete-stub-validation"),
                "{source}: {diagnostics:?}"
            );
        }
        assert!(validate("value = -1\n", "value: int\n").is_empty());
        let diagnostics = validate("other = 1\n", &format!("{schema}ROWS: list[_Row]\n"));
        assert_eq!(diagnostics, ["invalid-stub-implementation"]);
    }

    #[test]
    fn typed_dictionary_variable_proofs_reject_shared_or_mutable_values() {
        let stub = "class _Row(TypedDict, closed=True):\n    name: str\nROWS: list[_Row]\n";
        for source in [
            "def opaque(): pass\nROWS = [{'name': opaque()}]\n",
            "SHARED = {'name': 'ok'}\nROWS = [SHARED]\n",
            "SHARED = {'name': 'ok'}\nROWS = [dict(SHARED)]\n",
            "ROWS = [{'name': 'ok'}]\nALIAS = ROWS\n",
            "ALIAS, ROWS = (0, [{'name': 'ok'}])\n",
            "ROWS = [{'name': 'ok'}]\nROWS = [{'name': 'again'}]\n",
            "ROWS = [{'name': 'ok'}]\nROWS[0]['name'] = 1\n",
            "ROWS = [{'name': 'ok'}]\ndef corrupt(rows): rows[0]['name'] = 1\ncorrupt(ROWS)\n",
            "ROWS = [{'name': 'ok'}]\ndef corrupt(): ROWS[0]['name'] = 1\n",
            "def corrupt(): ROWS[0]['name'] = 1\nROWS = [{'name': 'ok'}]\n",
        ] {
            let diagnostics = validate(source, stub);
            assert!(
                diagnostics
                    .iter()
                    .any(|id| id == "incomplete-stub-validation"),
                "{source}: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn variable_initializer_validation_tracks_edits_without_changing_source_analysis() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let source = fixture.add_file(&mut analysis.db, "source.bzl", "ROWS = [{'name': 'ok'}]\n");
        let stub = fixture.add_file(&mut analysis.db, "source.bzli", "");
        loader.add_files_from_fixture(&fixture);
        analysis.set_type_interfaces([(source, stub)]).unwrap();
        for field_type in ["str", "int", "str"] {
            analysis.update_file(
                stub,
                format!("class _Row(TypedDict, closed=True):\n    name: {field_type}\nROWS: list[_Row]\n"),
            );
            assert!(analysis.snapshot().diagnostics(source).unwrap().is_empty());
            let reports = analysis.validate_stubs(|_| true).unwrap();
            let ids: Vec<_> = reports
                .iter()
                .flat_map(|(_, diagnostics)| diagnostics)
                .map(|diagnostic| diagnostic.id().as_str())
                .collect();
            assert_eq!(ids.is_empty(), field_type == "str", "{reports:?}");
            assert!(!ids.contains(&"incomplete-stub-validation"), "{reports:?}");
            assert!(analysis.snapshot().diagnostics(source).unwrap().is_empty());
        }
    }

    #[test]
    fn variable_initializers_require_static_contracts() {
        for (source, stub) in [
            ("value = 1\n", "value: Any\n"),
            ("ROWS = []\n", "ROWS: list[Any]\n"),
            (
                "ROWS = [{'name': 'ok'}]\n",
                "class _Row(TypedDict, closed=True):\n    name: Any\nROWS: list[_Row]\n",
            ),
        ] {
            assert_eq!(
                validate(source, stub),
                ["incomplete-stub-validation"],
                "{source}"
            );
        }
    }

    #[test]
    fn open_typed_dictionary_variables_require_independent_evidence() {
        let stub = "class _Row(TypedDict):\n    name: str\nROWS: list[_Row]\n";
        for source in [
            "ROWS = [{'name': 'ok'}]\n",
            "ROWS = [{'name': 'ok', 'private': 1}]\n",
        ] {
            assert_eq!(
                validate(source, stub),
                ["incomplete-stub-validation"],
                "{source}"
            );
        }
    }

    #[test]
    fn nested_open_dictionary_contracts_require_independent_evidence() {
        for (source, stub) in [
            (
                "ROWS = {'inner': {'name': 'ok', 'private': 1}}\n",
                "class _Inner(TypedDict):\n    name: str\nclass _Outer(TypedDict, closed=True):\n    inner: _Inner\nROWS: _Outer\n",
            ),
            (
                "ROWS = [{'name': 'ok', 'private': 1}]\n",
                "class _Row(TypedDict):\n    name: str\nclass _Rows(Protocol):\n    def __getitem__(self, index: int) -> _Row: ...\nROWS: _Rows\n",
            ),
        ] {
            assert_eq!(validate(source, stub), ["incomplete-stub-validation"], "{stub}");
        }
    }

    #[test]
    fn closed_interface_classes_require_dictionary_bases() {
        for stub in [
            "class _Value(closed=True):\n    value: int\n",
            "class _Value(Protocol, closed=True):\n    value: int\n",
        ] {
            let diagnostics = validate("", stub);
            assert_eq!(diagnostics, ["invalid-provider-interface"], "{stub}");
            let (mut analysis, loader) = Analysis::new_for_test();
            let mut fixture = Fixture::new(&mut analysis.db);
            let file = fixture.add_file(&mut analysis.db, "source.bzli", stub);
            loader.add_files_from_fixture(&fixture);
            let diagnostics = super::super::interface::diagnostics(&analysis.db, file);
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
            assert_eq!(
                diagnostics[0].headline_message(),
                "Only TypedDict declarations accept class keywords"
            );
        }
    }

    #[test]
    fn variable_annotations_keep_source_precedence() {
        for source in ["value: str = 'ok'\n", "value = 'ok' # type: str\n"] {
            assert_eq!(
                validate(source, "value: int\n"),
                ["invalid-stub-implementation"],
                "{source}"
            );
        }
    }

    #[test]
    fn variable_contracts_and_unknown_evidence() {
        for (source, stub, expected) in [
            ("value = 1\n", "value: int\n", None),
            ("def helper(value):\n    return value\nvalue = helper(1)\n", "def helper(value: int) -> int: ...\nvalue: int\n", None),
            ("value = 'bad'\n", "value: int\n", Some("invalid-assignment")),
            ("other = 1\n", "value: int\n", Some("invalid-stub-implementation")),
            ("def helper(value):\n    return value\nvalue = helper(1)\n", "value: int\n", Some("incomplete-stub-validation")),
            ("def helper(value):\n    return value\ndef compute(value):\n    return helper(value)\n", "def compute(value: int) -> int: ...\n", Some("unsound-return-statement")),
            ("def helper(): pass\ndef make():\n    return helper()\n", "class _Builder(Protocol):\n    def build(self) -> str: ...\ndef make() -> _Builder: ...\n", Some("unsound-return-statement")),
            ("def helper(): pass\ndef make(): return [helper()]\n", "def make() -> list[int]: ...\n", Some("incomplete-stub-validation")),
            ("def helper(): pass\ndef make(): return {'name': helper()}\n", "def make() -> dict[str, int]: ...\n", Some("incomplete-stub-validation")),
            ("def identity(value): return value\ndef make(): return identity\n", "def make() -> Callable[[int], int]: ...\n", Some("incomplete-stub-validation")),
            ("def identity(value): return value\nCALLBACKS = [identity]\ndef make(): return CALLBACKS\n", "def make() -> list[Callable[[int], int]]: ...\n", Some("incomplete-stub-validation")),
            ("def helper() -> int: return 1\ndef make(): return [helper()]\n", "def make() -> list[int]: ...\n", None),
            ("def make(): return {'name': 1}\n", "def make() -> dict[str, int]: ...\n", None),
            ("def identity(value: int) -> int: return value\ndef make(): return identity\n", "def make() -> Callable[[int], int]: ...\n", None),
        ] {
            let diagnostics = validate(source, stub);
            if let Some(expected) = expected { assert!(diagnostics.iter().any(|id| id == expected), "{source}: {diagnostics:?}"); }
            else { assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}"); }
        }
    }

    #[test]
    fn reexports_find_the_body_and_restore_caller_contracts() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let body = fixture.add_file(
            &mut analysis.db,
            "body.bzl",
            "def compute(value):\n    return 'bad'\n",
        );
        let source = fixture.add_file(
            &mut analysis.db,
            "source.bzl",
            "load('body.bzl', _compute='compute')\ncompute = _compute\n",
        );
        let stub = fixture.add_file(
            &mut analysis.db,
            "source.bzli",
            "def compute(value: int) -> int: ...\n",
        );
        let caller = fixture.add_file(
            &mut analysis.db,
            "caller.bzl",
            "load('source.bzl', 'compute')\nvalue = compute('bad')\n",
        );
        loader.add_files_from_fixture(&fixture);
        analysis.set_type_interfaces([(source, stub)]).unwrap();
        for (body_text, valid) in [
            ("def compute(value):\n    return 'bad'\n", false),
            ("def compute(value):\n    return value\n", true),
        ] {
            analysis.update_file(body, body_text.to_owned());
            let reports = analysis.validate_stubs(|_| true).unwrap();
            let diagnostics: Vec<_> = reports
                .iter()
                .flat_map(|(_, diagnostics)| diagnostics)
                .collect();
            assert_eq!(diagnostics.is_empty(), valid, "{diagnostics:?}");
            if !valid {
                assert!(
                    diagnostics
                        .iter()
                        .any(|diagnostic| diagnostic.id().as_str() == "invalid-return-type"),
                    "{diagnostics:?}"
                );
            }
            assert!(analysis
                .db
                .environment()
                .stub_validation(&analysis.db)
                .annotations
                .is_empty());
            let diagnostics = analysis.snapshot().diagnostics(caller).unwrap();
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
            assert_eq!(diagnostics[0].id().as_str(), "invalid-argument-type");
        }
    }

    #[test]
    fn absent_annotations_remain_unproved() {
        let diagnostics = validate(
            "def compute(value):\n    return value\n",
            "def compute(value): ...\n",
        );
        assert!(
            diagnostics
                .iter()
                .any(|id| id == "incomplete-stub-validation"),
            "{diagnostics:?}"
        );
        let diagnostics = validate(
            "def helper(value):\n    return value\nvalue = [helper(1)]\n",
            "value: list[int]\n",
        );
        assert!(
            diagnostics
                .iter()
                .any(|id| id == "incomplete-stub-validation"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn stub_annotations_keep_their_nominal_scope() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let source = fixture.add_file(
            &mut analysis.db,
            "source.bzl",
            "Info = provider(fields=[])\ndef consume(value):\n    return value\n",
        );
        let stub = fixture.add_file(
            &mut analysis.db,
            "source.bzli",
            "load('source.bzl', Original='Info')\ndef consume(value: Original) -> Original: ...\n",
        );
        loader.add_files_from_fixture(&fixture);
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                Default::default(),
            )
            .unwrap();
        analysis.set_type_interfaces([(source, stub)]).unwrap();
        let reports = analysis.validate_stubs(|_| true).unwrap();
        assert!(
            reports
                .iter()
                .all(|(_, diagnostics)| diagnostics.is_empty()),
            "{reports:?}"
        );
    }

    #[test]
    fn discovery_failures_and_excluded_bodies_preserve_contracts() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let body = fixture.add_file(
            &mut analysis.db,
            "body.bzl",
            "def compute(value):\n    return 'bad'\n",
        );
        let source = fixture.add_file(
            &mut analysis.db,
            "source.bzl",
            "load('body.bzl', 'compute')\n",
        );
        let stub = fixture.add_file(
            &mut analysis.db,
            "source.bzli",
            "def compute(value: int) -> int: ...\n",
        );
        let caller = fixture.add_file(
            &mut analysis.db,
            "caller.bzl",
            "load('source.bzl', 'compute')\nvalue = compute(1)\n",
        );
        loader.add_files_from_fixture(&fixture);
        analysis.set_type_interfaces([(source, stub)]).unwrap();
        let reports = analysis.validate_stubs(|_| true).unwrap();
        assert!(
            reports
                .iter()
                .flat_map(|(_, diagnostics)| diagnostics)
                .any(|diagnostic| diagnostic.id().as_str() == "invalid-stub-implementation"),
            "{reports:?}"
        );
        analysis.update_file(
            source,
            "load('body.bzl', _compute='compute')\ncompute = _compute\n".to_owned(),
        );
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            analysis.validate_stubs(|path| {
                assert_ne!(
                    path.file_name().unwrap(),
                    "body.bzl",
                    "interrupt correspondence discovery"
                );
                true
            })
        }));
        assert!(panic.is_err());
        assert!(analysis.snapshot().diagnostics(caller).unwrap().is_empty());
        assert!(analysis
            .db
            .environment()
            .stub_validation(&analysis.db)
            .annotations
            .is_empty());
        analysis.update_file(
            source,
            "load('missing.bzl', _compute='compute')\ncompute = _compute\n".to_owned(),
        );
        let reports = analysis.validate_stubs(|_| true).unwrap();
        assert!(
            reports
                .iter()
                .flat_map(|(_, diagnostics)| diagnostics)
                .any(|diagnostic| diagnostic.id().as_str() == "load-error"),
            "{reports:?}"
        );
        analysis.update_file(
            source,
            "load('body.bzl', _compute='compute')\ncompute = _compute\n".to_owned(),
        );
        let reports = analysis
            .validate_stubs(|path| path.file_name().unwrap() != "body.bzl")
            .unwrap();
        assert!(!reports.iter().any(|(file, _)| *file == body));
        assert!(
            reports
                .iter()
                .all(|(_, diagnostics)| diagnostics.is_empty()),
            "{reports:?}"
        );
        assert!(analysis.snapshot().diagnostics(caller).unwrap().is_empty());
        assert!(analysis
            .db
            .environment()
            .stub_validation(&analysis.db)
            .annotations
            .is_empty());
    }

    #[test]
    fn ambiguous_contracts_do_not_choose_one_annotation() {
        let diagnostics = validate("def implementation(value):\n    return value\nfirst = implementation\nsecond = implementation\n", "def first(value: int) -> int: ...\ndef second(value: string) -> string: ...\n");
        assert_eq!(
            diagnostics
                .iter()
                .filter(|id| *id == "incomplete-stub-validation")
                .count(),
            2,
            "{diagnostics:?}"
        );
    }
}
