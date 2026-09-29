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
use ruff_python_ast::ExprContext;
use ruff_python_ast::ExprName;
use ruff_python_ast::HasNodeIndex;
use ruff_python_ast::NodeIndex;
use ruff_python_ast::Parameter;
use ruff_python_ast::Stmt;
use ruff_python_ast::StmtFunctionDef;
use ruff_python_ast::UnaryOp;
use ruff_text_size::Ranged;
use ruff_text_size::TextRange;
use rustc_hash::FxHashMap;
use rustc_hash::FxHashSet;
use salsa::Setter;
use starpls_common::File;
use starpls_hir::Db as _;
use starpls_hir::ProviderContract;
use starpls_hir::StubValidation;
use starpls_hir::StubValidationPhase;
use starpls_hir::ValidationAnnotation;
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
use ty_python_semantic::types::ide_support::unreachable_ranges;
use ty_python_semantic::types::ide_support::UnreachableRange;
use ty_python_semantic::types::CallableTypeKind;
use ty_python_semantic::types::ParameterKind;
use ty_python_semantic::types::Signature;
use ty_python_semantic::types::Type;
use ty_python_semantic::types::TypeCheckResult;
use ty_python_semantic::types::TypeDefinition;
use ty_python_semantic::FunctionInferenceFacts;
use ty_python_semantic::FunctionInferenceMode;
use ty_python_semantic::HasType;
use ty_python_semantic::ProgramEnvironment;
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

pub(super) fn annotation(
    db: &dyn starpls_hir::Db,
    file: ruff_db::files::File,
    owner: NodeIndex,
) -> Option<ValidationAnnotation> {
    file_annotations(db, file).get(&owner).copied()
}

// The semantic index consumes every annotation in a file. A File key avoids
// reclaiming synthesized (file, node) argument keys between validation phases.
#[salsa::tracked(returns(ref))]
fn file_annotations(
    db: &dyn starpls_hir::Db,
    file: ruff_db::files::File,
) -> FxHashMap<NodeIndex, ValidationAnnotation> {
    db.environment()
        .stub_validation(db)
        .annotations
        .get(&file)
        .cloned()
        .unwrap_or_default()
}

#[salsa::tracked(returns(ref))]
fn provider_contract(
    db: &dyn starpls_hir::Db,
    file: ruff_db::files::File,
    owner: NodeIndex,
) -> Option<ProviderContract> {
    db.environment()
        .stub_validation(db)
        .provider_returns
        .get(&(file, owner))
        .cloned()
}

#[salsa::tracked(returns(copy))]
pub(super) fn is_validation_file(db: &dyn starpls_hir::Db, file: ruff_db::files::File) -> bool {
    db.environment().stub_validation(db).files.contains(&file)
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
            let mut validation = StubValidation::default();
            for source in &sources {
                let Some(&(_, stub)) = interfaces.get(&source.source) else {
                    continue;
                };
                reports.entry(*source).or_default();
                reports.entry(stub).or_default();
                let previous_count = contracts.len() + values.len();
                discover(
                    db,
                    *source,
                    stub,
                    &selected,
                    &mut contracts,
                    &mut values,
                    &mut reports,
                );
                if contracts.len() + values.len() > previous_count {
                    validation.files.insert(source.source);
                }
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
                .extend(contracts.iter().map(|contract| contract.source.file.source));
            environment.set_stub_validation(db).to(validation.clone());
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
            let mut body_checks = Vec::new();
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
                    let result = compare_function(db, source.file, &name, actual, expected);
                    if result.is_ok() {
                        let facts @ FunctionInferenceFacts {
                            return_type_correspondence,
                            has_cycle_recovery,
                            has_errors,
                            has_checking_failures,
                            has_unproved_requirements,
                        } = model
                            .function_inference_facts(function_definition(db, source))
                            .expect("function contracts originate in a function declaration");
                        if !has_errors {
                            if has_cycle_recovery
                                || has_checking_failures
                                || has_unproved_requirements
                                || return_type_correspondence != Some(true)
                            {
                                ContractError::Incomplete(format!(
                                    "Cannot prove `{name}`: {}",
                                    inference_failure_reasons(facts, true)
                                ))
                                .report(
                                    &mut reports,
                                    source.file,
                                    range,
                                );
                            } else {
                                body_checks.push((source, name.clone(), range));
                            }
                        }
                    }
                    result
                };
                if let Err(error) = result {
                    error.report(&mut reports, source.file, range);
                }
            }
            validation.phase = StubValidationPhase::Conservative;
            environment.set_stub_validation(db).to(validation.clone());
            for (source, name, range) in body_checks {
                if let Err(error) = compare_function_body(db, source, &name) {
                    error.report(&mut reports, source.file, range);
                }
            }
            validation.phase = StubValidationPhase::Ordinary;
            environment.set_stub_validation(db).to(validation);
            let mut reports: Vec<_> = reports
                .into_iter()
                .map(|(file, diagnostics)| {
                    let mut checked: Vec<_> = starpls_hir::diagnostics_for_file(db, file)
                        .take(128)
                        .collect();
                    let TypeCheckResult {
                        diagnostics: source_diagnostics,
                        has_suppressed_inference_failures,
                        has_unproved_requirements,
                    } = super::diagnostics::check_with_status(db, file);
                    checked.extend(source_diagnostics);
                    let has_errors = diagnostics
                        .iter()
                        .chain(&checked)
                        .any(|diagnostic| diagnostic.severity() == Severity::Error);
                    if (has_suppressed_inference_failures
                        || (has_unproved_requirements && !has_errors))
                        && !diagnostics.iter().chain(&checked).any(|diagnostic| {
                            diagnostic.id() == DiagnosticId::Lint(INCOMPLETE_STUB_VALIDATION.name())
                        })
                    {
                        let mut diagnostic = Diagnostic::new(
                            DiagnosticId::Lint(INCOMPLETE_STUB_VALIDATION.name()),
                            Severity::Error,
                            "Cannot validate implementation: type checking has unresolved or suppressed obligations",
                        );
                        diagnostic.annotate(Annotation::primary(Span::from(file.source)));
                        checked.push(diagnostic);
                    }
                    // These outcomes include suppressed checking obligations. Source lint
                    // suppressions cannot establish that a contract was validated.
                    checked.extend(diagnostics);
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
            // Private helper types belong only to the stub. Explicit function and
            // value declarations can describe matching implementation bindings.
            if !super::interface::is_implementation_contract(db, *declaration, source_program) {
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

/// Borrow context after checking independent evidence and confined storage references.
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
    if !has_fresh_literal_evidence(db, *source, &model, assignment.value(&parsed)) {
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
    validation
        .annotations
        .entry(source.source)
        .or_default()
        .insert(
            target.node_index().load(),
            ValidationAnnotation::ValueContract {
                file: *stub,
                owner: statement.node_index().load(),
            },
        );
    Some(())
}

/// Fresh container spines can contain confined, independently inferred list leaves.
/// Starlark freezes module values before import; the shared proof covers only in-file uses.
fn has_fresh_literal_evidence(
    db: &Database,
    source: File,
    model: &SemanticModel<'_>,
    expression: &Expr,
) -> bool {
    if expression.is_name_expr() {
        return false;
    }
    let mut names = Vec::new();
    if !collect_fresh_literal_evidence(model, expression, &mut names) {
        return false;
    }
    let Some(definitions) = model.confined_name_definitions(&names) else {
        return false;
    };
    let parsed =
        ruff_db::parsed::parsed_module(db, db.starlark_program_file(source).python_file(db))
            .load(db);
    let mut checked = FxHashSet::default();
    for definition in definitions {
        if !checked.insert(definition) {
            continue;
        }
        let DefinitionKind::Assignment(assignment) = definition.kind(db) else {
            return false;
        };
        if assignment.owner() != BindingsOwner::Definition
            || starpls_hir::Source::new(db)
                .type_comment_annotation(source, assignment.target(&parsed).node_index().load())
                .is_some()
        {
            return false;
        }
        let Expr::List(list) = assignment.value(&parsed) else {
            return false;
        };
        if list.elts.is_empty()
            || !list
                .elts
                .iter()
                .all(|item| has_static_scalar_evidence(model, item))
        {
            return false;
        }
    }
    true
}

/// Children provide independent evidence before the stub supplies a contextual shape.
fn collect_fresh_literal_evidence<'ast>(
    model: &SemanticModel<'_>,
    expression: &'ast Expr,
    names: &mut Vec<&'ast ExprName>,
) -> bool {
    match expression {
        Expr::List(list) => list
            .elts
            .iter()
            .all(|item| collect_fresh_literal_evidence(model, item, names)),
        Expr::Tuple(tuple) => tuple
            .elts
            .iter()
            .all(|item| collect_fresh_literal_evidence(model, item, names)),
        Expr::Dict(dict) => dict.items.iter().all(|item| {
            item.key
                .as_ref()
                .is_some_and(|key| collect_fresh_literal_evidence(model, key, names))
                && collect_fresh_literal_evidence(model, &item.value, names)
        }),
        Expr::Name(name) => {
            names.push(name);
            true
        }
        _ => has_static_scalar_evidence(model, expression),
    }
}

fn has_static_scalar_evidence(model: &SemanticModel<'_>, expression: &Expr) -> bool {
    match expression {
        Expr::UnaryOp(unary) => {
            matches!(unary.op, UnaryOp::UAdd | UnaryOp::USub)
                && unary.operand.is_number_literal_expr()
                && expression_has_static_evidence(model, expression)
        }
        Expr::StringLiteral(_) => expression_has_static_evidence(model, expression),
        Expr::NumberLiteral(_) => expression_has_static_evidence(model, expression),
        Expr::BooleanLiteral(_) => expression_has_static_evidence(model, expression),
        Expr::NoneLiteral(_) => expression_has_static_evidence(model, expression),
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
    } = provider_contract(db, definition.file(db), node.node_index().load()).as_ref()?;
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
        || source_kind.has_decorators()
        || stub_kind.has_decorators()
    {
        return Err("generic or decorated functions require a dedicated implementation contract");
    }
    if stub_node.type_params.is_some()
        && (provider.is_some()
            || source_node.returns.is_some()
            || source_node
                .parameters
                .iter()
                .any(|parameter| parameter.as_parameter().annotation.is_some())
            || stub_node.returns.is_none()
            || stub_node
                .parameters
                .iter()
                .any(|parameter| parameter.as_parameter().annotation.is_none()))
    {
        return Err(
            "generic contracts require a complete signature and an unannotated implementation",
        );
    }
    let pairs = parameter_pairs_with_receiver(source_node, stub_node, provider.is_some())?;
    if let Some(provider) = provider {
        validation.provider_returns.insert(
            (source.file.source, source_node.node_index().load()),
            provider.clone(),
        );
    } else if stub_node.returns.is_some() {
        validation
            .annotations
            .entry(source.file.source)
            .or_default()
            .insert(
                source_node.node_index().load(),
                ValidationAnnotation::Declaration {
                    file: stub.file,
                    owner: stub_node.node_index().load(),
                },
            );
    }
    for (parameter, annotation) in pairs {
        if annotation.annotation.is_none() {
            continue;
        }
        validation
            .annotations
            .entry(source.file.source)
            .or_default()
            .insert(
                parameter.node_index().load(),
                ValidationAnnotation::Declaration {
                    file: stub.file,
                    owner: annotation.node_index().load(),
                },
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
    compare_provider_constructor(
        db,
        file,
        name,
        expected,
        raw_signature,
        &fields,
        data.fields.as_deref(),
    )
}

fn compare_provider_constructor<'db>(
    db: &'db Database,
    file: File,
    name: &str,
    expected: Type<'db>,
    raw_signature: Option<Signature<'db>>,
    fields: &[ty_python_semantic::provided::ProvidedField<'db>],
    allowed: Option<&[Name]>,
) -> Result<(), ContractError> {
    let environment =
        ty_python_semantic::ProgramEnvironment::from_file(db.starlark_program_file(file));
    let signature = raw_signature
        .or_else(|| callable_signature(db, &environment, expected))
        .ok_or_else(|| {
            ContractError::Incomplete(format!(
                "Cannot validate `{name}`: constructor has no single callable signature"
            ))
        })?;
    for parameter in signature.parameters().iter() {
        if matches!(parameter.kind(), ParameterKind::KeywordVariadic { name: _ })
            && allowed.is_none()
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
        if allowed.is_some_and(|allowed| !allowed.contains(parameter_name)) {
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
        // Plain and raw constructors store each argument unchanged in its field,
        // so this compares declarations rather than independently inferred values.
        compare_declarations(
            db,
            file,
            &format!("{name}.{}", field.name),
            parameter.annotated_type(),
            field.ty,
        )?;
    }
    Ok(())
}

fn plain_provider_fields<'db>(
    db: &'db Database,
    ty: Type<'db>,
    environment: &ty_python_semantic::ProgramEnvironment<'db>,
) -> Option<Vec<ty_python_semantic::provided::ProvidedField<'db>>> {
    let Type::ClassLiteral(_) = ty else {
        return None;
    };
    let definition = super::interface::provider_definition(db, ty, environment)?;
    let allowed = super::interface::plain_provider_schema(db, definition)?;
    let fields = super::interface::provider_fields(db, ty, environment)?;
    if fields.iter().any(|field| !allowed.contains(&field.name)) {
        return None;
    }
    let file = db.starlark_file(definition.program_file(db))?;
    compare_provider_constructor(
        db,
        file,
        &definition.name(db)?,
        ty,
        None,
        &fields,
        Some(&allowed),
    )
    .ok()?;
    Some(fields)
}

pub(super) fn call_diagnostics<'db>(
    db: &'db Database,
    call: &ty_python_semantic::types::CheckedCall<'_, 'db>,
) -> Vec<Diagnostic> {
    use ty_python_semantic::types::CheckedArgument;

    if !is_validation_file(db, call.file().file(db))
        || db.environment().stub_validation(db).phase != StubValidationPhase::Ordinary
        || call.has_binding_errors()
    {
        return Vec::new();
    }
    let environment = ty_python_semantic::ProgramEnvironment::from_file(call.file());
    let check_argument =
        |subject: &str, name: &str, expected, has_default, missing, indeterminate| {
            let (range, reason) = match call.argument(name) {
                CheckedArgument::Value { ty, expression } => {
                    if ty.satisfies_declared_output(db, &environment, expected) {
                        return None;
                    }
                    (
                        expression.map_or(call.call().range(), Ranged::range),
                        format!(
                            "argument type `{}` does not preserve declared type `{}`",
                            ty.display(db, &environment),
                            expected.display(db, &environment),
                        ),
                    )
                }
                CheckedArgument::Omitted => {
                    if has_default {
                        return None;
                    }
                    (call.call().range(), String::from(missing))
                }
                CheckedArgument::Indeterminate => {
                    (call.call().range(), String::from(indeterminate))
                }
            };
            let mut diagnostic = Diagnostic::new(
                DiagnosticId::Lint(INCOMPLETE_STUB_VALIDATION.name()),
                Severity::Error,
                format!("Cannot prove {subject} `{name}`: {reason}"),
            );
            diagnostic.annotate(Annotation::primary(
                Span::from(call.file().file(db)).with_range(range),
            ));
            Some(diagnostic)
        };
    let selected_signature = || {
        let ty = call.expression_type(&call.call().func)?;
        let function = ty.as_function_literal()?;
        let signature = function.selected_contract_signature(db)?;
        let TypeDefinition::Function(definition) = ty.definition(db, &environment)? else {
            return None;
        };
        if call.declaration() != Some(definition) {
            return None;
        }
        let index = semantic_index(db, call.file());
        let scope = index.try_expression_scope_id(&ruff_python_ast::ExprRef::from(call.call()))?;
        (function_inference_mode(db, scope.to_scope_id(db, call.file()))
            == FunctionInferenceMode::OutputProof)
            .then_some(signature)
    };
    if let Some(signature) = selected_signature() {
        if call.arguments_satisfy_declared_parameters(db) {
            return Vec::new();
        }
        return signature
            .parameters()
            .iter()
            .filter_map(|parameter| {
                check_argument(
                    "argument",
                    parameter.name().expect("selected parameters have names"),
                    parameter.annotated_type(),
                    parameter.has_default(),
                    "no argument establishes the required input",
                    "argument matching does not identify one definite input value",
                )
            })
            .collect();
    }
    call.expression_type(&call.call().func)
        .and_then(|ty| plain_provider_fields(db, ty, &environment))
        .into_iter()
        .flatten()
        .filter_map(|field| {
            check_argument(
                "constructor field",
                &field.name,
                field.ty,
                false,
                "no argument establishes the required field",
                "argument matching does not identify one definite field value",
            )
        })
        .collect()
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
        if !has_nominal_evidence(db, &environment, field.ty) {
            return Err(ContractError::Incomplete(format!(
                "Cannot prove `{name}`: field `{}` requires a structural or unresolved contract",
                field.name
            )));
        }
    }
    let facts @ FunctionInferenceFacts {
        return_type_correspondence: _,
        has_cycle_recovery,
        has_errors,
        has_checking_failures,
        has_unproved_requirements,
    } = model
        .function_inference_facts(definition)
        .expect("provider initializers originate in a function declaration");
    if !has_errors && (has_cycle_recovery || has_checking_failures || has_unproved_requirements) {
        return Err(ContractError::Incomplete(format!(
            "Cannot prove initializer for `{name}`: {}",
            inference_failure_reasons(facts, false),
        )));
    }
    if !has_errors && !has_static_evidence(db, source.file, function, false) {
        return Err(ContractError::Incomplete(format!("Cannot prove `{name}`: its initializer contains dynamic or unavailable expression types")));
    }
    Ok(())
}

#[salsa::tracked(returns(copy))]
pub(super) fn function_inference_mode<'db>(
    db: &'db dyn super::interface::Db,
    scope: ty_python_core::scope::ScopeId<'db>,
) -> FunctionInferenceMode {
    let validation = db.environment().stub_validation(db);
    if validation.files.is_empty() {
        return FunctionInferenceMode::Default;
    }
    if !selected_execution_scope(db, scope) {
        return FunctionInferenceMode::Default;
    }
    match validation.phase {
        StubValidationPhase::Ordinary => FunctionInferenceMode::OutputProof,
        StubValidationPhase::Conservative => FunctionInferenceMode::Conservative,
    }
}

fn selected_execution_scope(
    db: &dyn super::interface::Db,
    scope: ty_python_core::scope::ScopeId<'_>,
) -> bool {
    let validation = db.environment().stub_validation(db);
    if validation.files.is_empty() {
        return false;
    }
    let Some(source) = db.starlark_file(scope.program_file(db)) else {
        return false;
    };
    if source.is_type_interface(db) || !validation.files.contains(&source.source) {
        return false;
    }
    let owns_function = |function: &StmtFunctionDef| {
        let owner = function.node_index().load();
        matches!(
            validation
                .annotations
                .get(&source.source)
                .and_then(|annotations| annotations.get(&owner)),
            Some(ValidationAnnotation::Declaration { file: _, owner: _ })
        ) || (validation.phase == StubValidationPhase::Ordinary
            && validation
                .provider_returns
                .contains_key(&(source.source, owner)))
    };
    // Selected named implementations include their nested expression scopes.
    // Module initialization includes eager comprehensions. Owned inline defaults
    // also require operation proof; unrelated module lambda bodies remain deferred.
    // Defaults and first iterables retain their semantic execution scope.
    let mut deferred_body = matches!(
        scope.node(db),
        ty_python_core::scope::NodeWithScopeKind::Lambda(_)
    );
    let scope = if matches!(
        scope.node(db),
        ty_python_core::scope::NodeWithScopeKind::Lambda(_)
            | ty_python_core::scope::NodeWithScopeKind::ListComprehension(_)
            | ty_python_core::scope::NodeWithScopeKind::DictComprehension(_)
    ) {
        let index = semantic_index(db, scope.program_file(db));
        let parsed = ruff_db::parsed::parsed_module(db, scope.python_file(db)).load(db);
        let owns_default = |lambda: &ruff_python_ast::ExprLambda| {
            if validation.phase != StubValidationPhase::Ordinary {
                return false;
            }
            let Some(ty_python_core::statement::Statement::Definition(definition)) =
                index.enclosing_lambda_statement(lambda.into())
            else {
                return false;
            };
            let DefinitionKind::Function(function) = definition.kind(db) else {
                return false;
            };
            let function = function.node(&parsed);
            owns_function(function)
                && function
                    .parameters
                    .iter_non_variadic_params()
                    .any(|parameter| {
                        parameter
                            .default
                            .as_deref()
                            .is_some_and(|default| default.range().contains_range(lambda.range()))
                    })
        };
        let mut owned_default = match scope.node(db) {
            ty_python_core::scope::NodeWithScopeKind::Lambda(lambda) => {
                owns_default(lambda.node(&parsed))
            }
            _ => false,
        };
        let Some((owner, _)) =
            index
                .ancestor_scopes(scope.file_scope_id(db))
                .skip(1)
                .find(|(_, ancestor)| {
                    if let ty_python_core::scope::NodeWithScopeKind::Lambda(lambda) =
                        ancestor.node()
                    {
                        deferred_body = true;
                        owned_default |= owns_default(lambda.node(&parsed));
                    }
                    matches!(
                        ancestor.node(),
                        ty_python_core::scope::NodeWithScopeKind::Function(_)
                            | ty_python_core::scope::NodeWithScopeKind::Class(_)
                            | ty_python_core::scope::NodeWithScopeKind::Module
                    )
                })
        else {
            return false;
        };
        if owned_default {
            return true;
        }
        owner.to_scope_id(db, scope.program_file(db))
    } else {
        scope
    };
    if matches!(
        scope.node(db),
        ty_python_core::scope::NodeWithScopeKind::Module
    ) {
        return validation.phase == StubValidationPhase::Ordinary && !deferred_body;
    }
    let ty_python_core::scope::NodeWithScopeKind::Function(function) = scope.node(db) else {
        return false;
    };
    let parsed = ruff_db::parsed::parsed_module(db, scope.python_file(db)).load(db);
    owns_function(function.node(&parsed))
}

fn conservative_function(db: &Database, source: File, owner: NodeIndex) -> bool {
    let validation = db.environment().stub_validation(db);
    validation.phase == StubValidationPhase::Conservative
        && matches!(
            validation
                .annotations
                .get(&source.source)
                .and_then(|annotations| annotations.get(&owner)),
            Some(ValidationAnnotation::Declaration { file: _, owner: _ })
        )
}

/// Check default and provider expression evidence and reject unchecked nested definitions.
fn has_static_evidence(
    db: &Database,
    source: File,
    function: &StmtFunctionDef,
    conservative_body: bool,
) -> bool {
    use ruff_python_ast::visitor::Visitor;
    use ruff_python_ast::visitor::{self};

    struct Evidence<'a, 'db> {
        db: &'db Database,
        model: &'a SemanticModel<'db>,
        unreachable: &'a [UnreachableRange],
        complete: bool,
        conservative: bool,
    }
    impl Evidence<'_, '_> {
        fn is_unreachable(&self, range: TextRange) -> bool {
            let index = self
                .unreachable
                .partition_point(|unreachable| unreachable.range.end() <= range.start());
            self.unreachable
                .get(index)
                .is_some_and(|unreachable| unreachable.range.contains_range(range))
        }
    }
    impl<'a> Visitor<'a> for Evidence<'_, '_> {
        fn visit_expr(&mut self, expression: &'a ruff_python_ast::Expr) {
            if self.conservative || self.is_unreachable(expression.range()) {
                return;
            }
            // Inference types the leaves of a binding pattern, not its container.
            let binding_pattern = match expression {
                Expr::Tuple(tuple) => tuple.ctx == ExprContext::Store,
                Expr::List(list) => list.ctx == ExprContext::Store,
                _ => false,
            };
            if !binding_pattern
                && !expression_has_static_evidence(self.model, expression)
                && !expression.inferred_type(self.model).is_some_and(|ty| {
                    plain_provider_fields(self.db, ty, &self.model.program_environment()).is_some()
                })
            {
                self.complete = false;
            }
            visitor::walk_expr(self, expression);
        }
        fn visit_stmt(&mut self, statement: &'a ruff_python_ast::Stmt) {
            if self.is_unreachable(statement.range()) {
                return;
            }
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
    let model = SemanticModel::new(db, db.starlark_program_file(source));
    let StmtFunctionDef {
        node_index: _,
        range: _,
        is_async: _,
        decorator_list: _,
        name: _,
        type_params: _,
        parameters,
        returns: _,
        body,
    } = function;
    let mut evidence = Evidence {
        db,
        model: &model,
        unreachable: unreachable_ranges(db, model.program_file()),
        complete: true,
        conservative: false,
    };
    for parameter in parameters.iter_non_variadic_params() {
        let ruff_python_ast::ParameterWithDefault {
            parameter: _,
            default,
            range: _,
            node_index: _,
        } = parameter;
        if let Some(default) = default {
            // Fresh literals establish their contents independently of gradual
            // parameter context; other defaults need evidence from every child.
            if !has_fresh_literal_evidence(db, source, &model, default) {
                evidence.visit_expr(default);
            }
        }
    }
    evidence.conservative =
        conservative_body && conservative_function(db, source, function.node_index().load());
    evidence.visit_body(body);
    evidence.complete
}

fn has_nominal_evidence<'db>(
    db: &'db dyn ty_python_semantic::Db,
    environment: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> bool {
    ty.is_fully_static(db, environment)
        && !ty_python_semantic::types::any_over_type(db, environment, ty, false, |ty| {
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

fn inference_failure_reasons(facts: FunctionInferenceFacts, require_return: bool) -> String {
    let FunctionInferenceFacts {
        return_type_correspondence,
        has_cycle_recovery,
        has_errors: _,
        has_checking_failures,
        has_unproved_requirements,
    } = facts;
    let mut reasons = Vec::new();
    if has_cycle_recovery {
        reasons.push("type inference used recursive recovery");
    }
    if has_checking_failures {
        reasons.push("checking diagnostics were reported or suppressed");
    }
    if has_unproved_requirements {
        reasons.push("some operation inputs could not be proved");
    }
    if require_return {
        match return_type_correspondence {
            Some(true) => {}
            Some(false) => {
                reasons.push("returned values were not proved to satisfy the declared result")
            }
            None => reasons.push("declared-result correspondence was unavailable"),
        }
    }
    reasons.join("; ")
}

fn compare_function_body(db: &Database, source: Function, name: &str) -> Result<(), ContractError> {
    let definition = function_definition(db, source);
    let DefinitionKind::Function(function) = definition.kind(db) else {
        unreachable!("function contracts originate in a function declaration");
    };
    let parsed = ruff_db::parsed::parsed_module(db, definition.python_file(db)).load(db);
    let function = function.node(&parsed);
    let model = SemanticModel::new(db, db.starlark_program_file(source.file));
    let facts @ FunctionInferenceFacts {
        return_type_correspondence: _,
        has_cycle_recovery,
        has_errors: _,
        has_checking_failures,
        has_unproved_requirements,
    } = model
        .function_inference_facts(definition)
        .expect("function contracts originate in a function declaration");
    if has_cycle_recovery || has_checking_failures || has_unproved_requirements {
        return Err(ContractError::Incomplete(format!(
            "Cannot prove `{name}`: {}",
            inference_failure_reasons(facts, false),
        )));
    }
    if !has_static_evidence(db, source.file, function, true) {
        return Err(ContractError::Incomplete(format!(
            "Cannot prove `{name}`: a default value or nested definition could not be checked"
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
    compare_declarations(db, file, name, signature(actual), signature(expected))
}

fn compare_declarations<'db>(
    db: &'db Database,
    file: File,
    name: &str,
    actual: Type<'db>,
    expected: Type<'db>,
) -> Result<(), ContractError> {
    let environment =
        ty_python_semantic::ProgramEnvironment::from_file(db.starlark_program_file(file));
    let error = match compare(db, file, name, actual, expected) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    // Equivalent gradual annotations describe the same declared contract. Unknown and
    // provisional components cannot establish correspondence; bodies are checked separately.
    if matches!(&error, ContractError::Incomplete(_))
        && actual.is_fully_static_except_any(db, &environment)
        && expected.is_fully_static_except_any(db, &environment)
        && actual.is_equivalent_to(db, &environment, expected)
    {
        return Ok(());
    }
    Err(error)
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
    use ruff_db::diagnostic::Severity;
    use starpls_hir::Db as _;
    use starpls_hir::Fixture;

    use crate::Analysis;

    fn validate(source: &str, stub: &str) -> Vec<String> {
        validation_diagnostics(source, stub)
            .into_iter()
            .map(|diagnostic| diagnostic.id().as_str().to_owned())
            .collect()
    }

    fn validation_diagnostics(source: &str, stub: &str) -> Vec<super::Diagnostic> {
        validation_diagnostics_with_rules(source, stub, &[])
    }

    fn validation_diagnostics_with_rules(
        source: &str,
        stub: &str,
        rules: &[(&str, Severity)],
    ) -> Vec<super::Diagnostic> {
        let (mut analysis, loader) = Analysis::new_for_test();
        let settings = std::sync::Arc::get_mut(&mut analysis.db.semantic).unwrap();
        for &(name, severity) in rules {
            settings.validation_rules.enable(
                crate::ty::diagnostics::registry().get(name).unwrap(),
                severity,
                ty_python_semantic::lint::LintSource::File,
            );
        }
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
        reports
            .into_iter()
            .flat_map(|(_, diagnostics)| diagnostics)
            .collect()
    }

    #[test]
    fn advisory_diagnostics_preserve_implementation_proof() {
        for (source, stub, expected) in [
            (
                "def make(value, empty):\n    if not empty: return value\n    return value\n",
                "def make(value: int, empty: tuple[()]) -> int: ...\n",
                vec!["redundant-condition"],
            ),
            (
                "def make(value, empty):\n    if not empty: # ty: ignore[redundant-condition]\n        return value\n    return value\n",
                "def make(value: int, empty: tuple[()]) -> int: ...\n",
                vec![],
            ),
            (
                "def make(): return 1\nVALUE = 1 if make else 2 # ty: ignore[redundant-condition]\n",
                "def make() -> int: ...\n",
                vec![],
            ),
            (
                "def plain(value): return value\ndef make(value):\n    if plain: return value\n    return value\n",
                "def plain(value: int) -> int: ...\ndef make(value: int) -> int: ...\n",
                vec!["redundant-condition"],
            ),
            (
                "def make(value):\n    len(value) # ty: ignore[invalid-argument-type]\n    return value\n",
                "def make(value: int) -> int: ...\n",
                vec!["incomplete-stub-validation"],
            ),
            (
                "len(1) # ty: ignore[invalid-argument-type]\ndef make(): return 1\n",
                "def make() -> int: ...\n",
                vec!["incomplete-stub-validation"],
            ),
        ] {
            assert_eq!(validate(source, stub), expected, "{source}");
        }

        for suppression in ["", " # ty: ignore[redundant-condition-strict]"] {
            let diagnostics = validation_diagnostics_with_rules(
                &format!("def make(value):\n    flag = True\n    if flag:{suppression}\n        pass\n    return value\n"),
                "def make(value: int) -> int: ...\n",
                &[("redundant-condition-strict", Severity::Warning)],
            );
            let actual: Vec<_> = diagnostics
                .iter()
                .map(|diagnostic| (diagnostic.id().as_str(), diagnostic.severity()))
                .collect();
            let expected = if suppression.is_empty() {
                vec![("redundant-condition-strict", Severity::Warning)]
            } else {
                vec![]
            };
            assert_eq!(actual, expected, "{suppression}");
        }

        let diagnostics = validation_diagnostics_with_rules(
            "def make(value):\n    len(value)\n    return value\n",
            "def make(value: int) -> int: ...\n",
            &[("invalid-argument-type", Severity::Warning)],
        );
        let actual: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| (diagnostic.id().as_str(), diagnostic.severity()))
            .collect();
        assert_eq!(
            actual,
            [
                ("invalid-argument-type", Severity::Warning),
                ("incomplete-stub-validation", Severity::Error),
            ]
        );
    }

    #[test]
    fn never_describes_empty_containers_in_interfaces() {
        let stub = "def empty() -> dict[Never, Never]: ...\n";
        assert_eq!(
            validate("def empty(): return {}\n", stub),
            Vec::<String>::new()
        );
        assert_eq!(
            validate("def empty(): return {\"key\": 1}\n", stub),
            ["invalid-return-type"]
        );
        assert_eq!(
            validate("def empty(): return {}\nprint(Never)\n", stub),
            ["unresolved-reference"]
        );
    }

    #[test]
    fn validation_annotations_survive_other_files() {
        use std::sync::atomic::Ordering;

        use ruff_db::files::FileRootKind;
        use ruff_db::system::SystemPath;
        use ruff_db::Db as _;
        use ruff_python_ast::HasNodeIndex;
        use salsa::Setter;

        let (mut analysis, loader) = Analysis::new_for_test();
        let root = analysis.db.files().try_add_root(
            &analysis.db,
            SystemPath::new("/annotation-project"),
            FileRootKind::Project,
        );
        assert_eq!(
            root.kind_at_time_of_creation(&analysis.db),
            FileRootKind::Project
        );
        let mut fixture = Fixture::new(&mut analysis.db);
        let functions = |count| {
            (0..count)
                .map(|index| format!("def function_{index}(value): return value\n"))
                .collect::<String>()
        };
        let source_text = functions(64);
        let source = fixture.add_file(
            &mut analysis.db,
            "/annotation-project/source.bzl",
            &source_text,
        );
        let others = (0..8)
            .map(|index| {
                fixture.add_file(
                    &mut analysis.db,
                    format!("/annotation-project/other_{index}.bzl"),
                    &functions(256),
                )
            })
            .collect::<Vec<_>>();
        loader.add_files_from_fixture(&fixture);
        let program = analysis.db.starlark_program_file(source);
        let owners = {
            let parsed =
                ruff_db::parsed::parsed_module(&analysis.db, program.python_file(&analysis.db))
                    .load(&analysis.db);
            parsed
                .syntax()
                .body
                .iter()
                .flat_map(|statement| statement.as_function_def_stmt().unwrap().parameters.iter())
                .map(|parameter| parameter.as_parameter().node_index().load())
                .collect::<Vec<_>>()
        };
        ty_python_core::semantic_index(&analysis.db, program);
        let environment = analysis.db.environment();
        let mut validation = environment.stub_validation(&analysis.db).clone();
        for (index, other) in others.into_iter().enumerate() {
            validation.phase = if index % 2 == 0 {
                starpls_hir::StubValidationPhase::Conservative
            } else {
                starpls_hir::StubValidationPhase::Ordinary
            };
            let revision = salsa::plumbing::current_revision(&analysis.db);
            environment
                .set_stub_validation(&mut analysis.db)
                .to(validation.clone());
            assert!(salsa::plumbing::current_revision(&analysis.db) > revision);
            let program = analysis.db.starlark_program_file(other);
            ty_python_core::semantic_index(&analysis.db, program);
        }
        // Equal selections can be revalidated without repairing retired query keys.
        for owner in owners {
            assert_eq!(super::annotation(&analysis.db, source.source, owner), None);
        }
        let program = analysis.db.starlark_program_file(source);
        analysis.db.executions.store(0, Ordering::Relaxed);
        ty_python_core::semantic_index(&analysis.db, program);
        assert_eq!(analysis.db.executions.load(Ordering::Relaxed), 0);

        analysis.update_file(source, format!("{source_text}extra = 1\n"));
        let program = analysis.db.starlark_program_file(source);
        analysis.db.executions.store(0, Ordering::Relaxed);
        ty_python_core::semantic_index(&analysis.db, program);
        assert!(analysis.db.executions.load(Ordering::Relaxed) > 0);
        assert!(ty_python_core::place_table(
            &analysis.db,
            ty_python_core::global_scope(&analysis.db, program),
        )
        .symbol_id("extra")
        .is_some());
    }

    #[test]
    fn validation_phase_changes_reuse_semantic_index() {
        use std::sync::atomic::Ordering;

        use salsa::Setter;
        use ty_python_semantic::FunctionInferenceMode;
        use ty_python_semantic::HasType;
        use ty_python_semantic::SemanticModel;

        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let dependency = fixture.add_file(
            &mut analysis.db,
            "dependency.bzl",
            "def forward(value): return value\n",
        );
        let source = fixture.add_file(
            &mut analysis.db,
            "source.bzl",
            "load('dependency.bzl', 'forward')\ndef selected(value): return value\ndef unselected(value): return value\n",
        );
        let stub = fixture.add_file(
            &mut analysis.db,
            "source.bzli",
            "def selected(value: int) -> int: ...\n",
        );
        loader.add_files_from_fixture(&fixture);
        analysis.set_type_interfaces([(source, stub)]).unwrap();
        let function = |db: &crate::Database, file, name| {
            let definitions = super::export_definitions(db, db.starlark_program_file(file), name);
            let [definition] = definitions.as_slice() else {
                panic!("expected one function");
            };
            super::function(db, *definition).unwrap()
        };
        let selected = function(&analysis.db, source, "selected");
        let declaration = function(&analysis.db, stub, "selected");
        let environment = analysis.db.environment();
        let previous = environment.stub_validation(&analysis.db).clone();
        let mut validation = starpls_hir::StubValidation::default();
        validation.files.insert(source.source);
        super::annotations(&analysis.db, selected, declaration, None, &mut validation).unwrap();
        // Source syntax is fixed throughout these phase changes, so its node and scope IDs
        // can be reused without traversing the syntax tree after each change.
        let scopes = {
            let db = &analysis.db;
            let program = db.starlark_program_file(source);
            let parsed = ruff_db::parsed::parsed_module(db, program.python_file(db)).load(db);
            let index = ty_python_core::semantic_index(db, program);
            let mut scopes = Vec::new();
            for statement in &parsed.syntax().body {
                let ruff_python_ast::Stmt::FunctionDef(function) = statement else {
                    continue;
                };
                scopes.push(
                    index.node_scope(ty_python_core::scope::NodeWithScopeRef::Function(function)),
                );
            }
            scopes
        };
        let returned_types = |db: &crate::Database| {
            let program = db.starlark_program_file(source);
            let parsed = ruff_db::parsed::parsed_module(db, program.python_file(db)).load(db);
            let model = SemanticModel::new(db, program);
            parsed
                .syntax()
                .body
                .iter()
                .filter_map(|statement| statement.as_function_def_stmt())
                .map(|function| {
                    let [statement] = function.body.as_slice() else {
                        panic!("expected one return statement");
                    };
                    let value = statement
                        .as_return_stmt()
                        .unwrap()
                        .value
                        .as_deref()
                        .unwrap();
                    value
                        .inferred_type(&model)
                        .unwrap()
                        .display(db, &model.program_environment())
                        .to_string()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(returned_types(&analysis.db), ["Unknown", "Unknown"]);
        ty_python_core::semantic_index(&analysis.db, analysis.db.starlark_program_file(dependency));
        let dependency_reused = |db: &crate::Database, phase: &str| {
            let program = db.starlark_program_file(dependency);
            db.executions.store(0, Ordering::Relaxed);
            ty_python_core::semantic_index(db, program);
            // The unselected parameter's annotation remains absent.
            assert_eq!(db.executions.load(Ordering::Relaxed), 1, "{phase}");
        };
        let modes = |db: &crate::Database| {
            let program = db.starlark_program_file(source);
            scopes
                .iter()
                .map(|scope| super::function_inference_mode(db, scope.to_scope_id(db, program)))
                .collect::<Vec<_>>()
        };
        environment
            .set_stub_validation(&mut analysis.db)
            .to(validation.clone());
        let program = analysis.db.starlark_program_file(source);
        analysis.db.executions.store(0, Ordering::Relaxed);
        ty_python_core::semantic_index(&analysis.db, program);
        assert!(analysis.db.executions.load(Ordering::Relaxed) > 1);
        dependency_reused(&analysis.db, "installation");
        assert_eq!(returned_types(&analysis.db), ["int", "Unknown"]);
        assert_eq!(
            modes(&analysis.db),
            [
                FunctionInferenceMode::OutputProof,
                FunctionInferenceMode::Default
            ]
        );
        for (phase, expected) in [
            (
                starpls_hir::StubValidationPhase::Conservative,
                FunctionInferenceMode::Conservative,
            ),
            (
                starpls_hir::StubValidationPhase::Ordinary,
                FunctionInferenceMode::OutputProof,
            ),
        ] {
            validation.phase = phase;
            environment
                .set_stub_validation(&mut analysis.db)
                .to(validation.clone());
            dependency_reused(&analysis.db, &format!("{phase:?}"));
            let db = &analysis.db;
            let program = db.starlark_program_file(source);
            db.executions.store(0, Ordering::Relaxed);
            ty_python_core::semantic_index(db, program);
            // Recheck the file's annotation map; equal answers preserve the index.
            assert_eq!(db.executions.load(Ordering::Relaxed), 1);
            assert_eq!(modes(db), [expected, FunctionInferenceMode::Default]);
        }
        environment
            .set_stub_validation(&mut analysis.db)
            .to(previous.clone());
        let program = analysis.db.starlark_program_file(source);
        analysis.db.executions.store(0, Ordering::Relaxed);
        ty_python_core::semantic_index(&analysis.db, program);
        assert!(analysis.db.executions.load(Ordering::Relaxed) > 1);
        dependency_reused(&analysis.db, "restoration");
        assert_eq!(environment.stub_validation(&analysis.db), &previous);
        assert_eq!(returned_types(&analysis.db), ["Unknown", "Unknown"]);
        assert_eq!(modes(&analysis.db), [FunctionInferenceMode::Default; 2]);
        let reports = analysis.validate_stubs(|_| true).unwrap();
        assert!(
            reports
                .iter()
                .all(|(_, diagnostics)| diagnostics.is_empty()),
            "{reports:?}"
        );
        dependency_reused(&analysis.db, "validation");
    }

    #[test]
    fn validation_checks_native_constructor_inputs() {
        for (source, stub) in [
            (
                "def convert(value): return int(value)\n",
                "def convert(value: str) -> int: ...\n",
            ),
            (
                "def convert(value): return str(value)\n",
                "def convert(value: object) -> str: ...\n",
            ),
            (
                "def convert(value): return [i for i in range(value)]\n",
                "def convert(value: int) -> list[int]: ...\n",
            ),
        ] {
            assert!(validate(source, stub).is_empty(), "{source}");
        }
    }

    #[test]
    fn validation_checks_selected_callee_evidence() {
        for (source, stub, expected) in [
            (
                "def make(): return attr.string()\n",
                "def make() -> Attribute: ...\n",
                None,
            ),
            (
                "def make(): return attr.string(mandatory='yes')\n",
                "def make() -> Attribute: ...\n",
                Some("invalid-argument-type"),
            ),
            (
                "def make(): return attr.string()\n",
                "def make() -> int: ...\n",
                Some("invalid-return-type"),
            ),
            (
                "def factory(unused: Any = None) -> Callable[[int], int]: return lambda value: 1\ndef make(): return factory()(1)\n",
                "def make() -> int: ...\n",
                None,
            ),
            (
                "def factory() -> Callable[..., None]: return lambda: None\ndef make(): return factory()()\n",
                "def make() -> None: ...\n",
                Some("incomplete-stub-validation"),
            ),
        ] {
            let diagnostics = validate(source, stub);
            match expected {
                Some(expected) => assert!(
                    diagnostics.iter().any(|id| id == expected),
                    "{source}: {diagnostics:?}",
                ),
                None => assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}"),
            }
        }
    }

    #[test]
    fn validation_checks_json_encoder_inputs() {
        for (source, stub, expected) in [
            (
                "def encode(value): return json.encode(value)\n",
                "def encode(value: object) -> str: ...\n",
                None,
            ),
            (
                "def encoder(): return json.encode\n",
                "def encoder() -> Callable[[object], str]: ...\n",
                None,
            ),
            (
                "def encode(): return json.encode()\n",
                "def encode() -> str: ...\n",
                Some("missing-argument"),
            ),
            (
                "def encode(value): return json.encode(value)\n",
                "def encode(value: object) -> int: ...\n",
                Some("invalid-return-type"),
            ),
        ] {
            let diagnostics = validate(source, stub);
            match expected {
                Some(expected) => assert!(
                    diagnostics.iter().any(|id| id == expected),
                    "{source}: {diagnostics:?}"
                ),
                None => assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}"),
            }
        }
    }

    #[test]
    fn validation_checks_rule_attribute_inputs() {
        for (attrs, expected) in [
            (None, None),
            (Some("{}"), None),
            (Some("attributes()"), None),
            (Some("descriptors"), None),
            (Some(r#"{"value": attr.string()}"#), None),
            (Some(r#"{"value": attr.label(default=None)}"#), None),
            (Some("None"), Some("invalid-argument-type")),
            (Some("{1: attr.string()}"), Some("invalid-argument-type")),
            (Some(r#"{"value": None}"#), Some("invalid-argument-type")),
            (Some(r#"{"value": 42}"#), Some("invalid-argument-type")),
            (Some("[]"), Some("invalid-argument-type")),
        ] {
            let attrs = attrs.map_or_else(String::new, |attrs| format!(", attrs={attrs}"));
            let source = format!(
                r#"def attributes():
    # type: () -> dict[str, Attribute]
    return {{"value": attr.string(mandatory=True)}}

descriptors = {{"text": attr.string(), "number": attr.int()}}
target = rule(implementation=lambda ctx: []{attrs})
def marker(): return 1
"#,
            );
            let diagnostics = validate(&source, "def marker() -> int: ...\n");
            match expected {
                Some(expected) => assert!(
                    diagnostics.iter().any(|id| id == expected),
                    "{source}: {diagnostics:?}"
                ),
                None => assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}"),
            }
        }
    }

    #[test]
    fn validation_distinguishes_rule_values_from_uses() {
        let preamble = r#"def attrs():
    # type: () -> dict[str, Attribute]
    return {"required": attr.string(mandatory=True)}

rule_value = rule(implementation=lambda ctx: [], attrs=attrs())
other_rule = rule(implementation=lambda ctx: [], attrs=attrs(), test=True)
def known(*, name: str) -> None: pass
"#;
        for (body, stub, expected) in [
            (
                "def make(): return rule_value",
                "def make() -> Callable[..., None]: ...",
                None,
            ),
            (
                "def make(flag): return rule_value if flag else other_rule",
                "def make(flag: bool) -> Callable[..., None]: ...",
                None,
            ),
            (
                "def make():\n    alias = rule_value\n    return alias",
                "def make() -> Callable[..., None]: ...",
                None,
            ),
            (
                "def make(): rule_value(name='target')",
                "def make() -> None: ...",
                Some("incomplete-stub-validation"),
            ),
            (
                "def make():\n    alias = rule_value\n    alias(name='target')",
                "def make() -> None: ...",
                Some("incomplete-stub-validation"),
            ),
            (
                "def make(): return rule_value",
                "class _Named(Protocol):\n    def __call__(self, *, name: str) -> None: ...\ndef make() -> _Named: ...",
                Some("unsound-return-statement"),
            ),
            (
                "def make(): return [rule_value]",
                "class _Named(Protocol):\n    def __call__(self, *, name: str) -> None: ...\ndef make() -> list[_Named]: ...",
                Some("incomplete-stub-validation"),
            ),
            (
                "def make(callback=lambda: rule_value(name='target')): return callback",
                "def make(callback: Callable[[], None] = ...) -> Callable[[], None]: ...",
                Some("incomplete-stub-validation"),
            ),
            (
                "def make(callback=lambda: None): return callback",
                "def make(callback: Callable[[], None] = ...) -> Callable[[], None]: ...",
                None,
            ),
            (
                "def make(callback=lambda: [rule_value]): return callback",
                "class _Named(Protocol):\n    def __call__(self, *, name: str) -> None: ...\ndef make(callback: Callable[[], list[_Named]] = ...) -> Callable[[], list[_Named]]: ...",
                Some("incomplete-stub-validation"),
            ),
            (
                "def make(callback=lambda: [known]): return callback",
                "class _Named(Protocol):\n    def __call__(self, *, name: str) -> None: ...\ndef make(callback: Callable[[], list[_Named]] = ...) -> Callable[[], list[_Named]]: ...",
                None,
            ),
        ] {
            let source = format!("{preamble}\n{body}\n");
            let diagnostics = validate(&source, stub);
            if let Some(expected) = expected {
                assert_eq!(diagnostics, [expected], "{body}");
            } else {
                assert!(diagnostics.is_empty(), "{body}: {diagnostics:?}");
            }
        }
    }

    #[test]
    fn validation_checks_generic_collection_initializers() {
        for value in [
            "type([])",
            "type({})",
            "type(select({'//conditions:default': []}))",
        ] {
            let source = format!("VALUE = {value}\n");
            let diagnostics = validate(&source, "VALUE: str\n");
            assert!(diagnostics.is_empty(), "{value}: {diagnostics:?}");
        }
    }

    #[test]
    fn validation_checks_reachable_code() {
        for (name, source, stub, expected) in [
            ("dead_unknown", "def make():\n    if False:\n        missing\n    return 1\n", "def make() -> int: ...\n", &[] as &[&str]),
            ("dead_suppressed", "def make():\n    if False:\n        missing # ty: ignore[unresolved-reference]\n    return 1\n", "def make() -> int: ...\n", &[]),
            ("dead_return", "def make():\n    if False:\n        return 'bad'\n    return 1\n", "def make() -> int: ...\n", &[]),
            ("live_return", "def make():\n    return 'bad'\n", "def make() -> int: ...\n", &["invalid-return-type"]),
            ("conditional", "def make(): return 1 if True else missing\n", "def make() -> int: ...\n", &[]),
            ("short_circuit", "def make(): return False and missing\n", "def make() -> bool: ...\n", &[]),
            ("after_return", "def make():\n    return 1\n    missing\n", "def make() -> int: ...\n", &[]),
            ("uncertain", "def make(flag):\n    if flag:\n        missing\n    return 1\n", "def make(flag: bool) -> int: ...\n", &["unresolved-reference"]),
            ("live_default", "def make(value=missing): return 1\n", "def make(value: object = ...) -> int: ...\n", &["unresolved-reference"]),
            ("live_suppressed", "def make():\n    missing # ty: ignore[unresolved-reference]\n    return 1\n", "def make() -> int: ...\n", &["incomplete-stub-validation"]),
            ("suppressed_default", "def make(value=missing): # ty: ignore[unresolved-reference]\n    return 1\n", "def make(value: object = ...) -> int: ...\n", &["incomplete-stub-validation"]),
        ] {
            let diagnostics = validation_diagnostics(source, stub);
            let ids: Vec<_> = diagnostics.iter().map(|diagnostic| diagnostic.id().as_str()).collect();
            assert_eq!(ids, expected, "{name}: {diagnostics:?}");
            let reason = match name {
                "live_suppressed" | "suppressed_default" => Some("diagnostics were reported or suppressed"),
                _ => None,
            };
            if let Some(reason) = reason {
                assert!(diagnostics[0].headline_message().contains(reason), "{name}: {diagnostics:?}");
            }
        }
    }

    #[test]
    fn dictionary_copies_preserve_nested_input_bounds() {
        for copy in [
            "dict(values)",
            "{key: value for key, value in values.items()}",
        ] {
            let source = format!(
                r#"
def make(values):
    copied = {copy}
    return len(copied)
"#
            );
            for value in ["int", "list[Any]"] {
                let stub = format!("def make(values: dict[str, {value}]) -> int: ...\n");
                let diagnostics = validate(&source, &stub);
                assert!(diagnostics.is_empty(), "{copy}, {value}: {diagnostics:?}");
            }
            assert_eq!(
                validate(
                    &format!(
                        r#"
def make(values):
    copied = {copy}
    copied["key"].append(1)
    return len(copied)
"#
                    ),
                    "def make(values: dict[str, list[Any]]) -> int: ...\n",
                ),
                ["incomplete-stub-validation"],
                "{copy}",
            );
            assert_eq!(
                validate(
                    &format!(
                        r#"
def _mutate(values: dict[str, list[Any]]) -> None:
    values["key"].append(1)
def make(values):
    copied = {copy}
    _mutate(copied)
    return len(copied)
"#
                    ),
                    "def make(values: dict[str, list[Any]]) -> int: ...\n",
                ),
                ["incomplete-stub-validation"],
                "{copy}",
            );
        }
        assert!(validate(
            "def make(): return dict()\n",
            "def make() -> dict[str, int]: ...\n",
        )
        .is_empty());
        assert_eq!(
            validate(
                "def make(): return len(dict(1))\n",
                "def make() -> int: ...\n",
            ),
            ["no-matching-overload"],
        );
    }

    #[test]
    fn binding_patterns_preserve_runtime_evidence() {
        assert!(validate(
            r#"
def make():
    [first, (second, third)] = (1, (2, 3))
    return first + second + third
"#,
            "def make() -> int: ...\n",
        )
        .is_empty());
        assert_eq!(
            validate(
                r#"
def make():
    values = [0]
    [values[(lambda item: item)(0)], result] = (1, 2)
    return result
"#,
                "def make() -> int: ...\n",
            ),
            ["incomplete-stub-validation"],
        );
    }

    #[test]
    fn reassigned_inputs_preserve_inferred_bounds() {
        for (annotation, body, expected) in [
            (
                "dict[str, list[Any]]",
                "values = dict(values)\n    return len(values)",
                vec![],
            ),
            (
                "list[Any]",
                "values = []\n    values.append(1)\n    return len(values)",
                vec![],
            ),
            (
                "dict[str, list[Any]]",
                "values = dict(values)\n    values['key'].append(1)\n    return len(values)",
                vec!["incomplete-stub-validation"],
            ),
        ] {
            assert_eq!(
                validate(
                    &format!("def collect(values):\n    {body}\n"),
                    &format!("def collect(values: {annotation}) -> int: ...\n")
                ),
                expected,
                "{body}",
            );
        }
    }

    #[test]
    fn module_storage_suppression_keeps_validation_incomplete() {
        let stub = r#"
class Info:
    run: Final[Callable[[list[Any]], int]]
    def __init__(self, *, run: Callable[[list[Any]], int]) -> None: ...
def make() -> Info: ...
"#;
        for suppression in ["", " # ty: ignore[incomplete-stub-validation]"] {
            let source = format!(
                r#"
Info = provider(fields=["run"])
def _only_strings(values: list[str]) -> int:
    return len(values[0])
_BAD = Info(run=_only_strings){suppression}
def make():
    return _BAD
"#
            );
            assert_eq!(validate(&source, stub), ["incomplete-stub-validation"]);
        }
        assert_eq!(
            validate(
                "def _unused():\n    return 1\ndef make(): return 1\n",
                "def make() -> int: ...\n",
            ),
            ["unused-definition"],
        );
        assert!(validate(
            "def _unused(): # ty: ignore[unused-definition]\n    return 1\ndef make(): return 1\n",
            "def make() -> int: ...\n",
        )
        .is_empty());
    }

    #[test]
    fn external_generic_contracts_check_identity_and_body() {
        assert!(validate(
            "def identity(value): return value\n",
            "def identity[T](value: T) -> T: ...\n"
        )
        .is_empty());
        assert!(validate(
            "def identity(value=None): return value\n",
            "def identity[T](value: T | None = ...) -> T | None: ...\n"
        )
        .is_empty());
        for (body, expected) in [
            ("return 1", vec!["invalid-return-type"]),
            ("return []", vec!["invalid-return-type"]),
            (
                "return value.missing()",
                vec!["unresolved-attribute", "unsound-return-statement"],
            ),
        ] {
            assert_eq!(
                validate(
                    &format!("def identity(value): {body}\n"),
                    "def identity[T](value: T) -> T: ...\n"
                ),
                expected,
                "{body}"
            );
        }
        for (source, stub) in [
            (
                "def identity(value) -> int: return 1\n",
                "def identity[T](value: T) -> T: ...\n",
            ),
            (
                "def identity(value): return value\n",
                "def identity[T](value: T): ...\n",
            ),
        ] {
            assert_eq!(validate(source, stub), ["incomplete-stub-validation"]);
        }
    }

    #[test]
    fn external_generic_contracts_validate_reset_helper() {
        let source = r#"
def _reset_on_attrs(attrs, *, self, attrs_to_reset, mutable_has_been_built):
    if mutable_has_been_built[0]:
        fail("reset_on_attrs() can only be called before build()")
    if not attrs:
        fail("reset_on_attrs() must be called with at least one attribute name")
    if attrs_to_reset:
        fail("reset_on_attrs() can only be called once")
    attrs_to_reset.extend(attrs)
    return self

_reset_on_attrs(("srcs",), self=1, attrs_to_reset=[], mutable_has_been_built=[False])
"#;
        let stub = r#"def _reset_on_attrs[T](
    attrs: tuple[str, ...],
    *,
    self: T,
    attrs_to_reset: list[str],
    mutable_has_been_built: list[bool],
) -> T: ...
"#;
        let diagnostics = validation_diagnostics(source, stub);
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
        assert_eq!(
            validate(&source.replace("return self", "return 42"), stub),
            ["invalid-return-type"]
        );
        assert_eq!(
            validate(
                &source.replace(
                    "attrs_to_reset.extend(attrs)",
                    "attrs_to_reset.extend([42])"
                ),
                stub
            ),
            ["invalid-argument-type"]
        );
    }

    #[test]
    fn type_predicate_has_static_implementation_evidence() {
        let version = "tuple[list[int], bool, list[tuple[int, int | str]]]";
        let mut failures = Vec::new();
        for (left, right, operator) in [
            ("object", "object", "=="),
            ("object", "object", "!="),
            ("object", "None", "=="),
            ("object", "None", "!="),
            ("None", "object", "=="),
            ("None", "object", "!="),
            ("str", "object", "=="),
            ("str", "object", "!="),
            ("bool", "object", "=="),
            ("bool", "object", "!="),
            ("list[int]", "object", "=="),
            ("list[int]", "object", "!="),
            (version, version, "<"),
            (version, version, "=="),
        ] {
            let source = format!("def compare(value, other): return value {operator} other\n");
            let donor = format!("def compare(value: {left}, other: {right}) -> bool: ...\n");
            let diagnostics = validate(&source, &donor);
            if !diagnostics.is_empty() {
                failures.push(format!("{left} {operator} {right}: {diagnostics:?}"));
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
        assert_eq!(
            validate(
                "def compare(value): return value == None\n",
                "def compare(value: object) -> Literal[False]: ...\n"
            ),
            ["invalid-return-type"]
        );
        for name in ["__eq__", "__ne__"] {
            assert_eq!(
                validate(
                    &format!("def extract(value): return value.{name}\n"),
                    "def extract(value: object) -> Callable[[object], bool]: ...\n"
                ),
                ["unresolved-attribute", "unsound-return-statement"],
                "{name}"
            );
        }
        let source = r#"def is_label(value):
    return type(value) == _LABEL_TYPE

_LABEL_TYPE = type(Label("//:bogus"))
"#;
        let stub = "def is_label(value: object) -> bool: ...\n";
        let diagnostics = validation_diagnostics(source, stub);
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
        assert_eq!(
            validate(
                &source.replace(
                    "type(value) == _LABEL_TYPE",
                    "(type(value) == _LABEL_TYPE, 42)[1]"
                ),
                stub
            ),
            ["invalid-return-type"]
        );

        let guard = "def is_label(value: object) -> TypeGuard[Label]: ...\n";
        let diagnostics = validation_diagnostics(source, guard);
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
        let diagnostics = validation_diagnostics(
            source,
            "def is_label(value: object) -> TypeGuard[str]: ...\n",
        );
        let [diagnostic] = diagnostics.as_slice() else {
            panic!("{diagnostics:?}");
        };
        assert_eq!(diagnostic.id().as_str(), "incomplete-stub-validation");
        assert_eq!(
            diagnostic.headline_message(),
            "Cannot prove `is_label`: returned values were not proved to satisfy the declared result"
        );
        for expression in ["True", "type(value) == 'string'"] {
            let diagnostics = validation_diagnostics(
                &format!("def is_label(value): return {expression}\n"),
                guard,
            );
            let [diagnostic] = diagnostics.as_slice() else {
                panic!("{diagnostics:?}");
            };
            assert_eq!(diagnostic.id().as_str(), "incomplete-stub-validation");
            let reason = if expression == "True" {
                "declared-result correspondence was unavailable"
            } else {
                "returned values were not proved to satisfy the declared result"
            };
            assert_eq!(
                diagnostic.headline_message(),
                format!("Cannot prove `is_label`: {reason}"),
                "{expression}"
            );
        }
        assert_eq!(
            validate(
                "def is_label(value, classifier): return classifier(value) == 'Label'\n",
                "def is_label(value: object, classifier: Callable[[object], str]) -> TypeGuard[Label]: ...\n"
            ),
            ["incomplete-stub-validation"]
        );
        assert_eq!(
            validate(
                &source.replace(
                    "_LABEL_TYPE = type(Label(\"//:bogus\"))",
                    "_LABEL_TYPE = type(\"\")"
                ),
                guard
            ),
            ["incomplete-stub-validation"]
        );
        let predicates = format!(
            r#"{source}
def is_string(value):
    return type(value) == _STRING_TYPE

_STRING_TYPE = type("")

def read(value):
    if is_label(value):
        return value.name
    if is_string(value):
        return value
    fail("expected Label or string")
"#
        );
        let contracts = format!(
            r#"{guard}def is_string(value: object) -> TypeGuard[str]: ...
def read(value: Label | str) -> str: ...
"#
        );
        let diagnostics = validation_diagnostics(&predicates, &contracts);
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
    }

    #[test]
    fn native_rules_without_contracts_remain_unproved() {
        assert_eq!(
            validate(
                "def namespace(): return native\n",
                "class _Required(Protocol):\n    @property\n    def sh_binary(self) -> object: ...\ndef namespace() -> _Required: ...\n",
            ),
            ["invalid-return-type"]
        );
        assert_eq!(
            validate(
                "def read(): return str(native.sh_binary)\n",
                "def read() -> str: ...\n",
            ),
            ["incomplete-stub-validation"]
        );
    }

    #[test]
    fn struct_signature_validation_requires_present_fields() {
        for field in ["run", "__repr__", "__class__", "__getattr__"] {
            for source in [
                "def make(): return struct()",
                "def make(data: dict[str, int]): return struct(**data)",
                "def make(value: struct[object]): return value\nmake(struct())",
                "def make(value: object): return value\nmake(struct())",
            ] {
                let parameters = if source.contains("data:") {
                    "data: dict[str, int]"
                } else if source.contains("value: struct") {
                    "value: struct[object]"
                } else if source.contains("value: object") {
                    "value: object"
                } else {
                    ""
                };
                let stub = format!("class _Required(Protocol):\n    @property\n    def {field}(self) -> object: ...\ndef make({parameters}) -> _Required: ...\n");
                assert_eq!(
                    validate(source, &stub),
                    ["invalid-return-type"],
                    "{field}: {source}"
                );
            }
        }
        for (annotation, expected) in [
            ("int", vec![]),
            ("NotRequired[int]", vec!["invalid-return-type"]),
        ] {
            let stub = format!("class _Row(TypedDict):\n    run: {annotation}\nclass _Required(Protocol):\n    @property\n    def run(self) -> int: ...\ndef make(row: _Row) -> _Required: ...\n");
            assert_eq!(
                validate("def make(row): return struct(**row)\n", &stub),
                expected
            );
        }
        let higher_order_source = "def use(factory): return factory(text='x')\nuse(struct)\ndef make(): return use(struct)\n";
        let higher_order_stub = "class _Factory(Protocol):\n    def __call__(self, **kwargs: str) -> struct[str]: ...\ndef use(factory: _Factory) -> struct[str]: ...\ndef make() -> struct[str]: ...\n";
        let diagnostics = validation_diagnostics(higher_order_source, higher_order_stub);
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
        assert!(validate(
            "def make(): return struct()",
            "class _Empty(Protocol): ...\ndef make() -> _Empty: ...\n"
        )
        .is_empty());
        for (source, parameters, required, expected) in [
            ("def make(): return struct(run=1)", "", "int", vec![]),
            ("def make(): return struct(**{'run': 1})", "", "int", vec![]),
            (
                "def make(): return struct(run='wrong')",
                "",
                "int",
                vec!["invalid-return-type"],
            ),
            (
                "def make(value: Any): return struct(run=value)",
                "value: Any",
                "object",
                vec![],
            ),
            (
                "def make(value: Any): return struct(run=value)",
                "value: Any",
                "int",
                vec!["unsound-return-statement"],
            ),
            (
                "def make(kwargs): return struct(run=_known, **kwargs)",
                "kwargs: Any",
                "_Run",
                vec!["incomplete-stub-validation"],
            ),
        ] {
            let source = if source.contains("_known") {
                format!("def _known(value: int) -> int: return value\n{source}\n")
            } else {
                source.to_owned()
            };
            let stub = format!("class _Run(Protocol):\n    def __call__(self, value: int) -> int: ...\nclass _Required(Protocol):\n    @property\n    def run(self) -> {required}: ...\ndef make({parameters}) -> _Required: ...\n");
            assert_eq!(validate(&source, &stub), expected, "{source}");
        }
    }

    #[test]
    fn struct_signature_typing_only_members_require_runtime_evidence() {
        let stub = "class _Getter(Protocol):\n    def __getattr__(self, name: str) -> int: ...\ndef make() -> _Getter: ...\n";
        assert_eq!(
            validate("def make(): return struct(value=1)", stub),
            ["invalid-return-type"]
        );
        let getter = r#"class _Getter(Protocol):
    @property
    def existing(self) -> int: ...
    @type_check_only
    def __getattr__(self, name: str) -> Any: ...
class _RequiredField(Protocol):
    @property
    def missing(self) -> object: ...
"#;
        for (source, declaration, expected) in [
            (
                "def make(): return struct(existing=1)",
                "def make() -> _Getter: ...",
                &[] as &[&str],
            ),
            (
                "def read(value): return value.existing",
                "def read(value: _Getter) -> int: ...",
                &[],
            ),
            (
                "def read(value): return value.missing",
                "def read(value: _Getter) -> object: ...",
                &[],
            ),
            (
                "def read(value): return value.missing",
                "def read(value: _Getter) -> int: ...",
                &["unsound-return-statement"],
            ),
            (
                "def extract(value): return value.__getattr__",
                "def extract(value: _Getter) -> Callable[[str], Any]: ...",
                &["incomplete-stub-validation"],
            ),
            (
                "def require(value): return value",
                "def require(value: _Getter) -> _RequiredField: ...",
                &["invalid-return-type"],
            ),
        ] {
            let stub = format!("{getter}{declaration}\n");
            assert_eq!(validate(source, &stub), expected, "{source}");
        }
        let getter = r#"class _Getter(Protocol):
    @type_check_only
    def __getattr__(self, name: str) -> int: ...
"#;
        for (source, declaration, expected) in [
            (
                "def read(value): return getattr(value, 'missing', None)",
                "def read(value: _Getter) -> int: ...",
                &["invalid-return-type"] as &[&str],
            ),
            (
                "def read(value): return getattr(value, 'missing', None)",
                "def read(value: _Getter) -> int | None: ...",
                &[],
            ),
            (
                "def read(value): return getattr(value, 'missing')",
                "def read(value: _Getter) -> int: ...",
                &[],
            ),
        ] {
            let stub = format!("{getter}{declaration}\n");
            assert_eq!(validate(source, &stub), expected, "{source}");
        }
        let stub = "def make(value: struct[int]) -> Callable[[str], int]: ...\n";
        assert!(!validate("def make(value): return value.__getattr__", stub).is_empty());
        let stub = "class _Required(Protocol):\n    @property\n    def __getattr__(self) -> int: ...\ndef make() -> _Required: ...\n";
        assert!(validate("def make(): return struct(__getattr__=1)", stub).is_empty());
        let stub = "class _Required(Protocol):\n    @property\n    def __len__(self) -> object: ...\ndef make() -> _Required: ...\n";
        assert_eq!(
            validate("def make(): return 'value'", stub),
            ["invalid-return-type"]
        );
        let stub = "def make() -> Callable[[], int]: ...\n";
        assert!(!validate("def make(): return 'value'.__len__", stub).is_empty());
        let stub = "class _Rows(Protocol):\n    def __getitem__(self, index: int) -> int: ...\ndef read(value: _Rows) -> int: ...\n";
        assert!(validate("def read(value): return value[0]", stub).is_empty());
        assert_eq!(
            validate("def read(value): return value['wrong']", stub),
            ["invalid-argument-type"]
        );
        let stub = "class _Rows(Protocol):\n    def __getitem__(self, index: int) -> int: ...\ndef extract(value: _Rows) -> Callable[[int], int]: ...\n";
        assert_eq!(
            validate("def extract(value): return value.__getitem__", stub),
            ["unresolved-attribute", "unsound-return-statement"]
        );
    }

    #[test]
    fn struct_signature_callback_runtime_member() {
        let source = "def known(value: int) -> int: return value\ndef extract(callback): return callback.__call__\nextract(known)\n";
        for annotation in ["_Callback", "Callable[[int], int]"] {
            let stub = format!("class _Callback(Protocol):\n    def __call__(self, value: int) -> int: ...\ndef extract(callback: {annotation}) -> Callable[[int], int]: ...\n");
            let diagnostics = validate(source, &stub);
            assert_eq!(
                diagnostics,
                ["unresolved-attribute", "unsound-return-statement"],
                "{annotation}"
            );
        }
        let stub = "def invoke(callback: Callable[[int], int]) -> int: ...\n";
        assert!(validate("def invoke(callback): return callback(1)", stub).is_empty());
        assert_eq!(
            validate("def invoke(callback): return callback('wrong')", stub),
            ["invalid-argument-type"]
        );
        let stub = "def extract() -> Callable[[int], int]: ...\n";
        assert_eq!(
            validate(
                "def known(value: int) -> int: return value\ndef extract(): return known.__call__",
                stub
            ),
            ["unresolved-attribute", "unsound-return-statement"]
        );
        let source = "def known(value: int) -> int: return value\ndef make(): return struct(__call__=known)\n";
        let stub = "class _Callback(Protocol):\n    def __call__(self, value: int) -> int: ...\nclass _Required(Protocol):\n    @property\n    def __call__(self) -> _Callback: ...\ndef make() -> _Required: ...\n";
        assert!(validate(source, stub).is_empty());
    }

    #[test]
    fn declared_callback_outputs_check_retained_constraints() {
        for (name, source, stub, expected) in [
            (
                "finite callback",
                "def make(): return lambda: 1\n",
                "def make() -> Callable[..., int]: ...\n",
                &[][..],
            ),
            (
                "stored omitted callback",
                "def make(row): return row['run']\n",
                r#"class _Row(TypedDict):
    run: Callable[..., None]
def make(row: _Row) -> Callable[..., None]: ...
"#,
                &[],
            ),
            (
                "invoke stored omitted callback",
                "def make(row): return row['run']()\n",
                r#"class _Row(TypedDict):
    run: Callable[..., None]
def make(row: _Row) -> None: ...
"#,
                &["incomplete-stub-validation"],
            ),
            (
                "invoke aliased omitted callback",
                "def make(row):\n    callback = row['run']\n    return callback()\n",
                r#"class _Row(TypedDict):
    run: Callable[..., None]
def make(row: _Row) -> None: ...
"#,
                &["incomplete-stub-validation"],
            ),
            (
                "tuple callback",
                "def make(): return (lambda: 1,)\n",
                "def make() -> tuple[Callable[..., int]]: ...\n",
                &[],
            ),
            (
                "readonly callback",
                "def make(): return struct(run=lambda: 1)\n",
                r#"class _Runner(Protocol):
    @property
    def run(self) -> Callable[..., int]: ...
def make() -> _Runner: ...
"#,
                &[],
            ),
            (
                "unconstrained result",
                "def _known(value: str) -> str: return value\ndef make(): return _known\n",
                "def make() -> Callable[..., Any]: ...\n",
                &[],
            ),
            (
                "opaque helper",
                "def _opaque(value): return value.missing()\ndef make(): return _opaque\n",
                "def make() -> Callable[..., Any]: ...\n",
                &[],
            ),
            (
                "unknown explicit input",
                "def _opaque(value) -> int: return 1\ndef make(): return _opaque\n",
                "def make() -> Callable[[Any], int]: ...\n",
                &["incomplete-stub-validation"],
            ),
            (
                "omitted unknown input",
                "def _opaque(value) -> int: return 1\ndef make(): return _opaque\n",
                "def make() -> Callable[..., int]: ...\n",
                &[],
            ),
            (
                "local unknown body",
                "def make(): return lambda value: value.missing()\n",
                "def make() -> Callable[..., Any]: ...\n",
                &["incomplete-stub-validation"],
            ),
            (
                "known helper error",
                "def _bad(value: Any) -> int: return 'bad'\ndef make(): return _bad\n",
                "def make() -> Callable[..., Any]: ...\n",
                &["invalid-return-type"],
            ),
            (
                "wrong result",
                "def make(): return lambda: 'bad'\n",
                "def make() -> Callable[..., int]: ...\n",
                &["invalid-return-type"],
            ),
            (
                "unknown callable presence",
                "def make(value): return value\n",
                "def make(value: Any) -> Callable[..., Any]: ...\n",
                &["incomplete-stub-validation"],
            ),
            (
                "mixed readonly domains",
                r#"def _narrow(values: list[str]) -> int: return len(values[0])
def make(): return struct(opaque=lambda: 1, checked=_narrow)
"#,
                r#"class _Runner(Protocol):
    @property
    def opaque(self) -> Callable[..., int]: ...
    @property
    def checked(self) -> Callable[[list[Any]], int]: ...
def make() -> _Runner: ...
"#,
                &["incomplete-stub-validation"],
            ),
            (
                "fresh callback storage",
                "def make(): return [lambda: 1]\n",
                "def make() -> list[Callable[..., int]]: ...\n",
                &[],
            ),
            (
                "retained callback storage",
                r#"def make():
    items: list[Callable[[], int]] = [lambda: 1]
    run = lambda: items[0]()
    return struct(items=items, run=run)
"#,
                r#"class _Result(Protocol):
    @property
    def items(self) -> list[Callable[..., int]]: ...
    @property
    def run(self) -> Callable[[], int]: ...
def make() -> _Result: ...
"#,
                &["incomplete-stub-validation"],
            ),
        ] {
            assert_eq!(validate(source, stub), expected, "{name}");
        }
    }

    #[test]
    fn provider_outputs_compare_declared_read_constraints() {
        for (name, helper, value, field, expected) in [
            (
                "finite callback",
                "def _known(value: str) -> str: return value\n",
                "_known",
                "Callable[..., str]",
                &[][..],
            ),
            (
                "unconstrained nested result",
                "def _opaque(value): return value.missing()\n",
                "(_opaque,)",
                "tuple[Callable[..., Any]]",
                &[],
            ),
            (
                "unknown explicit input",
                "def _opaque(value) -> int: return 1\n",
                "_opaque",
                "Callable[[Any], int]",
                &["incomplete-stub-validation"],
            ),
            (
                "concrete nested result",
                "def _opaque(value): return value.missing()\n",
                "(_opaque,)",
                "tuple[Callable[..., int]]",
                &["incomplete-stub-validation"],
            ),
        ] {
            let source = format!("Info = provider(fields=['run'])\n{helper}_VALUE = Info(run={value})\ndef make(): return _VALUE\n");
            let stub = format!(
                r#"class Info:
    run: Final[{field}]
    def __init__(self, *, run: {field}) -> None: ...
def make() -> Info: ...
"#
            );
            assert_eq!(validate(&source, &stub), expected, "{name}");
        }

        for key in ["Provider[object]", "Callable[..., object]"] {
            for (result, expected) in [("object", &[][..]), ("int", &["invalid-return-type"])] {
                let stub =
                    format!("def lookup(target: Target[None], key: {key}) -> {result}: ...\n");
                assert_eq!(
                    validate("def lookup(target, key): return target[key]\n", &stub),
                    expected,
                    "{key} -> {result}",
                );
            }
        }

        for (name, source, stub, expected) in [
            (
                "native provider key sequence",
                "def make(): return [config_common.FeatureFlagInfo]\n",
                "def make() -> Sequence[Provider[Any] | Callable[..., Any]]: ...\n",
                &[][..],
            ),
            (
                "provider result widening",
                "def make(value): return value\n",
                "def make(value: Provider[FeatureFlagInfo]) -> Provider[object]: ...\n",
                &[],
            ),
            (
                "provider result narrowing",
                "def make(value): return value\n",
                "def make(value: Provider[object]) -> Provider[FeatureFlagInfo]: ...\n",
                &["invalid-return-type"],
            ),
            (
                "unrelated provider result",
                "def make(value): return value\n",
                "def make(value: Provider[FeatureFlagInfo]) -> Provider[ToolchainInfo]: ...\n",
                &["invalid-return-type"],
            ),
        ] {
            assert_eq!(validate(source, stub), expected, "{name}");
        }
    }

    #[test]
    fn callback_keyword_variadic_context_checks_body() {
        let stub = r#"class _Collect(Protocol):
    def __call__(self, *, name: str, **values: int) -> int: ...
def make() -> _Collect: ...
"#;
        assert_eq!(
            validate(
                "def make(): return lambda *, name, **values: values['first'] + 1\n",
                stub,
            ),
            Vec::<String>::new(),
        );
        assert_eq!(
            validate(
                "def make(): return lambda *, name, **values: values['first'].upper()\n",
                stub,
            ),
            ["unresolved-attribute", "unsound-return-statement"],
        );
    }

    #[test]
    fn gradual_return_proofs_preserve_unrestricted_outputs() {
        assert_eq!(
            validate("def make(): return 1\n", "def make() -> Any: ...\n"),
            Vec::<String>::new(),
        );
        assert_eq!(
            validate(
                "def make(value): return value.missing()\n",
                "def make(value: Any) -> Any: ...\n",
            ),
            ["incomplete-stub-validation"],
        );
        assert_eq!(
            validate(
                "def make(): return struct(value=1)\n",
                "class _Row(Protocol):\n    @property\n    def value(self) -> int: ...\ndef make() -> _Row: ...\n",
            ),
            Vec::<String>::new(),
        );
        assert_eq!(
            validate(
                "def make(): return lambda value: len(value)\n",
                "def make() -> Callable[[str], int]: ...\n",
            ),
            Vec::<String>::new(),
        );
        assert_eq!(
            validate(
                r#"def _length(values: Sequence[object]) -> int:
    return len(values)
def make():
    return struct(run=_length)
"#,
                r#"class _Run(Protocol):
    def __call__(self, values: list[Any]) -> int: ...
class _Runner(Protocol):
    @property
    def run(self) -> _Run: ...
def make() -> _Runner: ...
"#,
            ),
            Vec::<String>::new(),
        );
    }

    #[test]
    fn gradual_output_correspondence_preserves_declared_results() {
        let source = r#"
def length(values: Sequence[object]) -> int:
    return len(values)
def only_strings(values: list[str]) -> int:
    return len(values[0])
def declared():
    return struct(run=length)
def forward():
    return declared()
def mixed(flag):
    return declared() if flag else struct(run=only_strings)
"#;
        let stub = r#"
class _Runner(Protocol):
    @property
    def run(self) -> Callable[[list[Any]], int]: ...
def declared() -> _Runner: ...
def forward() -> _Runner: ...
def mixed(flag: bool) -> _Runner: ...
"#;
        let diagnostics = validation_diagnostics(source, stub);
        let [diagnostic] = diagnostics.as_slice() else {
            panic!("only the mixed return should remain unproved: {diagnostics:?}");
        };
        assert_eq!(diagnostic.id().as_str(), "incomplete-stub-validation");
        let range = diagnostic
            .primary_span()
            .and_then(|span| span.range())
            .unwrap();
        assert_eq!(&source[range], "mixed");

        let source = r#"
def accept_any(*args: object, **kwargs: object) -> None:
    pass
def declared() -> Callable[..., None]:
    return accept_any
def invoke():
    declared()()
    return 1
"#;
        assert_eq!(
            validate(source, "def invoke() -> int: ...\n"),
            ["incomplete-stub-validation"]
        );
    }

    #[test]
    fn gradual_callback_returns_check_the_promised_input_domain() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let source = fixture.add_file(
            &mut analysis.db,
            "source.bzl",
            r#"def only_strings(values: list[str]) -> int:
    return len(values[0])
def make():
    return struct(run=only_strings)
"#,
        );
        let stub = fixture.add_file(
            &mut analysis.db,
            "source.bzli",
            r#"class _Run(Protocol):
    def __call__(self, values: list[Any]) -> int: ...
class _Runner(Protocol):
    @property
    def run(self) -> _Run: ...
def make() -> _Runner: ...
def only_strings(values: list[str]) -> int: ...
"#,
        );
        let caller_source =
            "load('source.bzl', 'make', 'only_strings')\nmake().run([1])\nonly_strings([1])\n";
        let caller = fixture.add_file(&mut analysis.db, "caller.bzl", caller_source);
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

        let diagnostics = analysis.snapshot().diagnostics(caller).unwrap();
        let [diagnostic] = diagnostics.as_slice() else {
            panic!("expected only the direct helper call to fail: {diagnostics:?}");
        };
        assert_eq!(diagnostic.id().as_str(), "invalid-argument-type");
        let Some(range) = diagnostic.primary_span().and_then(|span| span.range()) else {
            panic!("expected the direct helper argument range: {diagnostic:?}");
        };
        assert_eq!(&caller_source[range], "[1]");
        assert_eq!(
            usize::from(range.start()),
            caller_source.rfind("[1]").unwrap()
        );

        let reports = analysis.validate_stubs(|_| true).unwrap();
        let ids: Vec<_> = reports
            .iter()
            .flat_map(|(_, diagnostics)| diagnostics)
            .map(|diagnostic| diagnostic.id().as_str())
            .collect();
        assert_eq!(ids, ["incomplete-stub-validation"], "{reports:?}");
    }

    #[test]
    fn readonly_protocol_fields_check_named_callbacks() {
        let stub = r#"
class _Run(Protocol):
    def __call__(self, value: int) -> int: ...
class _Runner(Protocol):
    @property
    def run(self) -> _Run: ...
def make() -> _Runner: ...
"#;
        for (callback, compatible) in [
            ("def _known(value: int) -> int: return value", true),
            ("def _known(value: int) -> str: return 'wrong'", false),
            ("def _known() -> int: return 1", false),
            ("def _known(other: int) -> int: return other", false),
        ] {
            let source = format!("{callback}\ndef make(): return struct(run=_known)\n");
            let diagnostics = validate(&source, stub);
            let expected = if compatible {
                vec![]
            } else {
                vec!["invalid-return-type"]
            };
            assert_eq!(diagnostics, expected, "{callback}");
        }
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
    fn incomplete_contract_reports_survive_source_suppression() {
        let diagnostics = validate(
            "def make(value): # ty: ignore[incomplete-stub-validation]\n    return value.missing\n",
            "def make(value: Any) -> Any: ...\n",
        );
        assert_eq!(diagnostics, ["incomplete-stub-validation"]);
    }

    #[test]
    fn plain_provider_calls_check_stored_values() {
        let provider = "Info = provider(fields=['run'])\n";
        let declaration = r#"class Info:
    run: Final[Callable[[list[Any]], int]]
    def __init__(self, *, run: Callable[[list[Any]], int]) -> None: ...
"#;
        let narrow = r#"def _only_strings(values: list[str]) -> int:
    return len(values[0])
"#;
        let safe = r#"def _all_objects(values: Sequence[object]) -> int:
    return len(values)
"#;
        for (name, helper, source, contract, expected) in [
            (
                "forward",
                "",
                "def make(run):\n    return Info(run=run)\n",
                "def make(run: Callable[[list[Any]], int]) -> Info: ...\n",
                false,
            ),
            (
                "sequence",
                safe,
                "def make():\n    return Info(run=_all_objects)\n",
                "def make() -> Info: ...\n",
                false,
            ),
            (
                "constructor alias",
                "",
                "def make(run):\n    constructor = Info\n    return constructor(run=run)\n",
                "def make(run: Callable[[list[Any]], int]) -> Info: ...\n",
                false,
            ),
            (
                "direct",
                narrow,
                "def make():\n    return Info(run=_only_strings)\n",
                "def make() -> Info: ...\n",
                true,
            ),
            (
                "local result",
                narrow,
                "def make():\n    result = Info(run=_only_strings)\n    return result\n",
                "def make() -> Info: ...\n",
                true,
            ),
            (
                "annotated callback alias",
                narrow,
                "def make():\n    callback: Callable[[list[Any]], int] = _only_strings\n    return Info(run=callback)\n",
                "def make() -> Info: ...\n",
                true,
            ),
            (
                "nested lambda",
                narrow,
                "def make():\n    return lambda: Info(run=_only_strings)\n",
                "def make() -> Callable[[], Info]: ...\n",
                true,
            ),
            (
                "default",
                narrow,
                "def make(value=Info(run=_only_strings)):\n    return value\n",
                "def make(value: Info = ...) -> Info: ...\n",
                true,
            ),
            (
                "suppressed body",
                narrow,
                "def make():\n    return Info(run=_only_strings) # ty: ignore[incomplete-stub-validation]\n",
                "def make() -> Info: ...\n",
                true,
            ),
            (
                "suppressed default",
                narrow,
                "def make(value=Info(run=_only_strings)): # ty: ignore[incomplete-stub-validation]\n    return value\n",
                "def make(value: Info = ...) -> Info: ...\n",
                true,
            ),
        ] {
            let source = format!("{provider}{helper}{source}");
            let stub = format!("{declaration}{contract}");
            let diagnostics = validation_diagnostics(&source, &stub);
            let ids: Vec<_> = diagnostics
                .iter()
                .map(|diagnostic| diagnostic.id().as_str())
                .collect();
            assert_eq!(
                ids,
                if expected {
                    vec!["incomplete-stub-validation"]
                } else {
                    Vec::new()
                },
                "{name}: {diagnostics:#?}"
            );
        }
        let source = "Info = provider(fields={'run': 'Callback'})\ndef make(run):\n    return Info(run=run)\n";
        let stub = format!("{declaration}def make(run: Callable[[list[Any]], int]) -> Info: ...\n");
        let diagnostics = validation_diagnostics(source, &stub);
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");

        for provider in [
            "native_provider = provider\nprovider = native_provider\nInfo = provider(fields=['run'])\n",
            "native_provider = provider\nInfo = native_provider(fields=['run'])\n",
        ] {
            let source = format!("{provider}def make(run):\n    return Info(run=run)\n");
            let stub = format!("{declaration}def make(run: Callable[[list[Any]], int]) -> Info: ...\n");
            assert!(validate(&source, &stub).is_empty(), "{source}");
            let source = format!("{provider}{narrow}def make():\n    return Info(run=_only_strings)\n");
            let stub = format!("{declaration}def make() -> Info: ...\n");
            assert_eq!(validate(&source, &stub), ["incomplete-stub-validation"], "{source}");
        }

        let stub = r#"class Info:
    run: Final[Any]
    def __init__(self, *, run: Any) -> None: ...
def make() -> Info: ...
"#;
        let source = "Info = provider(fields=['run'])\ndef make():\n    return Info(run=42)\n";
        let diagnostics = validation_diagnostics(source, stub);
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");

        let stub = r#"class Info:
    run: Final[_Missing]
    def __init__(self, *, run: _Missing) -> None: ...
def make() -> Info: ...
"#;
        let diagnostics = validate(
            "Info = provider(fields=['run'])\ndef make():\n    return Info(run=42)\n",
            stub,
        );
        assert!(
            diagnostics
                .iter()
                .any(|id| id == "incomplete-stub-validation"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn provider_contracts_preserve_equivalent_gradual_storage() {
        let source = "Info = provider(fields=['value'])\n";
        let stub = r#"class Info:
    value: Final[Callable[..., Any]]
    def __init__(self, *, value: Callable[..., Any]) -> None: ...
"#;
        let diagnostics = validation_diagnostics(source, stub);
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
        let mismatched = r#"class Info:
    value: Final[Callable[[list[Any]], int]]
    def __init__(self, *, value: Callable[[list[str]], int]) -> None: ...
"#;
        assert_eq!(validate(source, mismatched), ["incomplete-stub-validation"]);
        let unknown = r#"class Info:
    value: Final[Any]
    def __init__(self, *, value) -> None: ...
"#;
        assert_eq!(validate(source, unknown), ["incomplete-stub-validation"]);

        let source = r#"def _init(value):
    return {"value": value}
Info, raw = provider(fields=["value"], init=_init)
"#;
        let stub = r#"class Info:
    value: Final[Callable[..., Any]]
    def __init__(self, value: Callable[..., Any]) -> None: ...
def raw(*, value: Callable[..., Any]) -> Info: ...
"#;
        let diagnostics = validation_diagnostics(source, stub);
        let [diagnostic] = diagnostics.as_slice() else {
            panic!("{diagnostics:#?}");
        };
        assert_eq!(diagnostic.id().as_str(), "incomplete-stub-validation");
        assert!(
            diagnostic.headline_message().contains("`Info`"),
            "{diagnostic:?}"
        );
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

        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let source = fixture.add_file(&mut analysis.db, "source.bzl",
            "def _init(): return {'value': 'ok'}\nInfo, _ = provider(fields=['value'], init=_init)\n");
        let stub = fixture.add_file(&mut analysis.db, "source.bzli", "");
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
        let previous = analysis
            .db
            .environment()
            .stub_validation(&analysis.db)
            .clone();
        for (field, expected) in [
            ("str", &[] as &[&str]),
            ("int", &["invalid-argument-type", "invalid-return-type"]),
            ("str", &[]),
        ] {
            analysis.update_file(
                stub,
                format!(
                    "class Info:\n    value: Final[{field}]\n    def __init__(self) -> None: ...\n"
                ),
            );
            let diagnostics: Vec<_> = analysis
                .validate_stubs(|_| true)
                .unwrap()
                .into_iter()
                .flat_map(|(_, diagnostics)| diagnostics)
                .map(|diagnostic| diagnostic.id().as_str().to_owned())
                .collect();
            assert_eq!(diagnostics, expected, "{field}");
            assert_eq!(
                analysis.db.environment().stub_validation(&analysis.db),
                &previous
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
    fn legacy_globals_keep_presence_and_gradual_members_distinct() {
        for key in ["PyInfo", "PyRuntimeInfo"] {
            let source = format!(
                "def make(): return getattr(getattr(native, 'legacy_globals', None), '{key}', None)\n"
            );
            let diagnostics = validate(&source, "def make() -> Callable[..., Any] | None: ...\n");
            assert!(diagnostics.is_empty(), "{key}: {diagnostics:?}");
        }
        let diagnostics = validate(
            "def make(): return native.legacy_globals.CcInfo\n",
            "def make() -> Callable[..., Any]: ...\n",
        );
        assert_eq!(diagnostics, ["incomplete-stub-validation"]);
    }

    #[test]
    fn java_internal_api_preserves_optional_members() {
        let helper = "def get_internal_java_common(): return java_common.internal_DO_NOT_USE()\n";
        let source = format!(
            r#"{helper}def enabled():
    return get_internal_java_common().google_legacy_api_enabled()
def maybe_factory():
    return getattr(java_common, "internal_DO_NOT_USE", None)
def merge():
    return java_common.merge
"#
        );
        let stub = r#"class _Internal(Protocol):
    @property
    def google_legacy_api_enabled(self) -> Callable[[], bool]: ...
def get_internal_java_common() -> _Internal: ...
def enabled() -> bool: ...
def maybe_factory() -> Callable[[], _Internal] | None: ...
def merge() -> Callable[..., Any]: ...
"#;
        let diagnostics = validate(&source, stub);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        for (source, stub, expected) in [
            (
                helper.replace(
                    "return java_common.internal_DO_NOT_USE()",
                    "return java_common.internal_DO_NOT_USE().google_legacy_api_enabled(True)",
                ),
                "def get_internal_java_common() -> bool: ...\n".to_owned(),
                &["too-many-positional-arguments"] as &[&str],
            ),
            (
                helper.replace(
                    "return java_common.internal_DO_NOT_USE()",
                    "return java_common.internal_DO_NOT_USE().google_legacy_api_enabled()",
                ),
                "def get_internal_java_common() -> str: ...\n".to_owned(),
                &["invalid-return-type"],
            ),
            (
                format!("{source}def unknown(): return java_common.unknown_member\n"),
                format!("{stub}def unknown() -> object: ...\n"),
                &["unresolved-attribute"],
            ),
            (
                helper
                    .replace("get_internal_java_common", "unknown")
                    .replace(
                        "return java_common.internal_DO_NOT_USE()",
                        "return java_common.internal_DO_NOT_USE().unknown_member",
                    ),
                "def unknown() -> int: ...\n".to_owned(),
                &["unsound-return-statement"],
            ),
        ] {
            let diagnostics = validate(&source, &stub);
            assert_eq!(diagnostics, expected, "{source}");
        }
        let diagnostics = validate(
            "def require_factory(): return java_common\n",
            r#"class _Internal(Protocol):
    @property
    def google_legacy_api_enabled(self) -> Callable[[], bool]: ...
class _RequiredFactory(Protocol):
    @property
    def internal_DO_NOT_USE(self) -> Callable[[], _Internal]: ...
def require_factory() -> _RequiredFactory: ...
"#,
        );
        assert_eq!(diagnostics, ["invalid-return-type"]);
    }

    #[test]
    fn mapping_interfaces_preserve_key_and_value_bounds() {
        let source = r#"def copy(values):
    return {key: value for key, value in values.items()}
def forward(values):
    return copy(values)
"#;
        let stub = r#"def copy(values: Mapping[str, object]) -> dict[str, object]: ...
def forward(values: dict[str, int]) -> dict[str, object]: ...
"#;
        let diagnostics = validate(source, stub);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let writer = source.replace(
            "    return {key:",
            "    values.update({\"key\": 0})\n    return {key:",
        );
        let diagnostics = validate(&writer, stub);
        assert!(
            diagnostics.iter().any(|id| id == "unresolved-attribute"),
            "{diagnostics:?}"
        );
        for (stub, expected) in [
            (
                stub.replace("dict[str, int]", "dict[int, int]"),
                "invalid-argument-type",
            ),
            (
                stub.replace("-> dict[str, object]", "-> dict[str, int]"),
                "invalid-return-type",
            ),
        ] {
            let diagnostics = validate(source, &stub);
            assert!(
                diagnostics.iter().any(|id| id == expected),
                "{diagnostics:?}"
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
                Some("invalid-return-type"),
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
        let source = r#"def target(*, name, value=0):
    pass
def forward(**kwargs):
    target(**kwargs)
"#;
        let stub = r#"class _Keywords(TypedDict, closed=True):
    name: str
    value: NotRequired[int]
def target(*, name: str, value: int = ...) -> None: ...
def forward(**kwargs: Unpack[_Keywords]) -> None: ...
"#;
        let diagnostics = validate(source, stub);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let diagnostics = validate(
            source,
            &stub.replace("name: str\n", "name: NotRequired[str]\n"),
        );
        assert!(
            diagnostics
                .iter()
                .any(|id| id == "incomplete-stub-validation"),
            "{diagnostics:?}",
        );
        let diagnostics = validate(
            source,
            &stub.replace("NotRequired[int]", "NotRequired[str]"),
        );
        assert!(
            diagnostics.iter().any(|id| id == "invalid-argument-type"),
            "{diagnostics:?}",
        );
        let source = r#"def apply(func, item):
    flag, payload = item
    if flag:
        return (True, {key: func(value) for key, value in payload.items()})
    else:
        return (False, func(payload))
"#;
        let stub = "def apply(func: Callable[[object], object], item: tuple[Literal[True], dict[str, object]] | tuple[Literal[False], object]) -> tuple[bool, object]: ...\n";
        for (stub, expected) in [
            (stub.to_owned(), None),
            (
                stub.replace("Literal[True], dict", "Literal[False], dict")
                    .replace("Literal[False], object", "Literal[True], object"),
                Some("unresolved-attribute"),
            ),
            (
                stub.replace("Callable[[object], object]", "Callable[[str], object]"),
                Some("invalid-argument-type"),
            ),
        ] {
            let diagnostics = validate(source, &stub);
            if let Some(expected) = expected {
                assert!(
                    diagnostics.iter().any(|id| id == expected),
                    "{stub}: {diagnostics:?}",
                );
            } else {
                assert!(diagnostics.is_empty(), "{stub}: {diagnostics:?}");
            }
        }
    }

    #[test]
    fn struct_keyword_fields_contextualize_callbacks() {
        let stub = r#"class _Reset(Protocol):
    def __call__(self, *attrs: str) -> _Builder: ...
class _Builder(Protocol):
    @property
    def reset(self) -> _Reset: ...
    @property
    def normalize(self) -> Callable[[str], str]: ...
def make() -> _Builder: ...
def reset(self: _Builder, state: list[str], attrs: tuple[str, ...]) -> _Builder: ...
"#;
        for constructor in ["struct", "constructor"] {
            let alias = if constructor == "constructor" {
                "    constructor = struct\n"
            } else {
                ""
            };
            let source = format!(
                r#"def make():
{alias}    state = []
    self = {constructor}(reset=lambda *attrs: reset(self, state, attrs), normalize=lambda text: text.lower())
    return self
def reset(self, state, attrs):
    state.extend(attrs)
    return self
"#
            );
            assert_eq!(validate(&source, stub), Vec::<String>::new(), "{source}");
        }

        let callback_stub = r#"class _Row(Protocol):
    @property
    def run(self) -> Callable[[str], str]: ...
def make() -> _Row: ...
"#;
        for (body, expected) in [
            ("1", vec!["invalid-return-type"]),
            (
                "value + 1",
                vec!["unsupported-operator", "unsound-return-statement"],
            ),
        ] {
            let source = format!(
                "def make():\n    result = struct(run=lambda value: {body})\n    return result\n"
            );
            assert_eq!(validate(&source, callback_stub), expected, "{source}");
        }
        assert_eq!(
            validate(
                "def struct(**fields): return fields\ndef make(): return struct(run=lambda value: value.lower())\n",
                callback_stub,
            ),
            ["unsound-return-statement"],
        );

        let optional_stub = r#"class _Run(Protocol):
    def __call__(self, value: int = ...) -> int: ...
class _Row(Protocol):
    @property
    def run(self) -> _Run: ...
def make() -> _Row: ...
"#;
        assert_eq!(
            validate(
                "def make(): return struct(run=lambda value='bad': value)\n",
                optional_stub
            ),
            ["invalid-return-type"],
        );
        assert_eq!(
            validate(
                "def make(): return struct(run=lambda value='bad': 1)\n",
                optional_stub
            ),
            Vec::<String>::new(),
        );
    }

    #[test]
    fn returned_callbacks_use_borrowed_context() {
        let stub = "def make() -> Callable[[str], str]: ...\n";
        assert_eq!(
            validate(
                "def make():\n    callback = lambda value: value.lower()\n    return callback\n",
                stub,
            ),
            Vec::<String>::new(),
        );
        assert_eq!(
            validate(
                "def make():\n    callback = lambda value: value + 1\n    return callback\n",
                stub,
            ),
            ["unsupported-operator", "unsound-return-statement"],
        );
        assert_eq!(
            validate(
                "def make():\n    callback = lambda value='bad': 1\n    callback()\n    return callback\n",
                "def make() -> Callable[[int], int]: ...\n",
            ),
            ["unsound-return-statement"],
        );
    }

    #[test]
    fn augmented_storage_checks_local_and_module_values() {
        for (input, expected) in [
            (
                "Callable[..., None]",
                &["incomplete-stub-validation"] as &[&str],
            ),
            ("Callable[[str], None]", &[]),
        ] {
            let source = "def make(values):\n    callbacks: list[Callable[[str], None]] = []\n    callbacks += values\n    return callbacks[0]\n";
            let stub = format!("def make(values: list[{input}]) -> Callable[[str], None]: ...\n");
            assert_eq!(validate(source, &stub), expected, "local {input}");

            let source = format!("def incoming() -> {input}: return lambda value: None\nvalues = [incoming()]\ncallbacks: list[Callable[[str], None]] = []\ncallbacks += values\ndef make(): return callbacks[0]\n");
            assert_eq!(
                validate(&source, "def make() -> Callable[[str], None]: ...\n"),
                expected,
                "module {input}"
            );
        }
    }

    #[test]
    fn borrowed_defaults_enter_the_implementation_body() {
        for (default, annotation, valid) in [
            ("narrow", "Callable[[Any], None]", false),
            ("narrow", "Callable[[str], None]", true),
            ("narrow", "Callable[..., None]", true),
            (
                "lambda value: narrow(value)",
                "Callable[[Any], None]",
                false,
            ),
            ("lambda value: narrow(value)", "Callable[[str], None]", true),
        ] {
            let source = format!(
                "def narrow(value: str) -> None: pass\ndef make(callback={default}): return callback\n"
            );
            let stub = format!("def make(callback: {annotation} = ...) -> {annotation}: ...\n");
            let diagnostics = validate(&source, &stub);
            let expected: &[&str] = if valid {
                &[]
            } else {
                &["incomplete-stub-validation"]
            };
            assert_eq!(diagnostics, expected, "{source}");
        }
        assert!(validate(
            "def narrow(value: str) -> None: pass\ndef make(callback=narrow): return 1\n",
            "def make(callback: Callable[[Any], None] = ...) -> int: ...\n",
        )
        .is_empty());
        for (body, valid) in [
            (
                "    if value == None:\n        value = 1\n    return value\n",
                true,
            ),
            ("    return value\n", false),
        ] {
            let source = format!("def make(value=None):\n{body}");
            let diagnostics = validate(&source, "def make(value: int = ...) -> int: ...\n");
            let expected: &[&str] = if valid { &[] } else { &["invalid-return-type"] };
            assert_eq!(diagnostics, expected, "{source}");
        }
        for source in [
            "def make(value: int = None) -> int: return value\n",
            "def make(value=None): # type: (int) -> int\n    return value\n",
        ] {
            let diagnostics = validate(source, "def make(value: int = ...) -> int: ...\n");
            assert!(
                diagnostics
                    .iter()
                    .any(|id| id == "invalid-parameter-default"),
                "{source}: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn parameter_defaults_require_independent_evidence() {
        for (source, stub, expected) in [
            (
                "def opaque(): return 'bad'\ndef compute(value=opaque()): return value\n",
                "def compute(value: int = ...) -> int: ...\n",
                Some("unsound-return-statement"),
            ),
            (
                "def opaque() -> Any: return 'bad'\ndef compute(value=[opaque()]): return value[0]\n",
                "def compute(value: list[int] = ...) -> int: ...\n",
                Some("incomplete-stub-validation"),
            ),
            (
                "def opaque(): return 'bad'\ndef needs_int(value: int) -> int: return value\ndef compute(*, value=needs_int(opaque())): return value\n",
                "def compute(*, value: int = ...) -> int: ...\n",
                Some("incomplete-stub-validation"),
            ),
            (
                "def compute(value='bad'): # ty: ignore[invalid-parameter-default]\n    return value\n",
                "def compute(value: int = ...) -> int: ...\n",
                Some("invalid-return-type"),
            ),
            (
                "def needs_int(value: int) -> int: return value\ndef compute(value=needs_int('bad')): # ty: ignore[invalid-argument-type]\n    return value\n",
                "def compute(value: int = ...) -> int: ...\n",
                Some("incomplete-stub-validation"),
            ),
            (
                "def compute(value=[]): return value\n",
                "def compute(value: list[int] = ...) -> list[int]: ...\n",
                None,
            ),
            (
                "def compute(value=[]): return 1\n",
                "def compute(value: list[Any] = ...) -> int: ...\n",
                None,
            ),
            (
                "def compute(value={'items': [1]}): return 1\n",
                "def compute(value: dict[str, list[Any]] = ...) -> int: ...\n",
                None,
            ),
        ] {
            let diagnostics = validate(source, stub);
            if let Some(expected) = expected {
                assert_eq!(diagnostics, [expected], "{source}");
            } else {
                assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}");
            }
        }
        let stub = "class Info:\n    value: Final[list[int]]\n    def __init__(self, xs: list[int] = ...) -> None: ...\n";
        assert_eq!(validate(
            "def needs_int(value: int) -> list[int]: return [value]\ndef _init(xs=needs_int('bad')): # ty: ignore[invalid-argument-type]\n    return {'value': xs}\nInfo, _ = provider(fields=['value'], init=_init)\n",
            stub,
        ), ["incomplete-stub-validation"]);
        for (default, expected) in [
            ("[]", None),
            ("[opaque()]", Some("incomplete-stub-validation")),
        ] {
            let source = format!("def opaque() -> Any: return 'bad'\ndef _init(xs={default}): return {{'value': xs}}\nInfo, _ = provider(fields=['value'], init=_init)\n");
            let diagnostics = validate(&source, stub);
            if let Some(expected) = expected {
                assert_eq!(diagnostics, [expected], "{source}");
            } else {
                assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}");
            }
        }
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
    fn private_value_contracts_check_implementation_types() {
        for (value, expected) in [
            ("1", &[] as &[&str]),
            ("'bad'", &["invalid-stub-implementation"]),
        ] {
            let source = format!("_value = {value}\npublic = _value\n");
            assert_eq!(validate(&source, "_value: int\n"), expected, "{source}");
        }
        assert!(validate(
            "def callback(value: object) -> object: return value\n_value = callback\npublic = _value\n",
            "_value: Callable[[object], object]\n",
        )
        .is_empty());
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
        for (implementation, declaration) in [
            (
                "def _compute(value): return value + 1\nresult = _compute(1)\n",
                "def _compute(value: int) -> int: ...\n",
            ),
            ("_compute = 1\nresult = _compute\n", "_compute: int\n"),
        ] {
            for (prefix, name, expected_unused) in [
                ("", "_compute", 0),
                ("# moved\n", "_compute", 0),
                ("", "_other", 1),
                ("\n", "_compute", 0),
            ] {
                analysis.update_file(source, implementation.replace("_compute", name));
                analysis.update_file(stub, format!("{prefix}{declaration}"));
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
                Some("unsound-return-statement"),
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
    fn implicitly_open_value_contracts_check_fresh_storage() {
        let stub = "class _Row(TypedDict):\n    name: str\nROWS: list[_Row]\n";
        for source in [
            "ROWS = [{'name': 'ok'}]\n",
            "ROWS = [{'name': 'ok', 'hidden': []}]\n",
            "SHARED = ['linux']\nROWS = [{'name': 'ok', 'hidden': SHARED}]\n",
        ] {
            let diagnostics = validate(source, stub);
            assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}");
        }
        for source in [
            "SHARED = ['linux']\nALIAS = SHARED\nROWS = [{'name': 'ok', 'hidden': SHARED}]\n",
            "def opaque(): pass\nROWS = [{'name': 'ok', 'hidden': opaque()}]\n",
            "ROWS = [{'name': 'ok'}]\nALIAS = ROWS\n",
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
    fn confined_list_leaves_keep_independent_element_types() {
        let stub = "class _Row(TypedDict):\n    tags: list[str]\nROWS: list[_Row]\n";
        let source = "tags = ['linux']\nROWS = [{'tags': tags}, {'tags': tags, 'hidden': tags}]\n";
        assert!(validate(source, stub).is_empty());
        let diagnostics = validate(&source.replace("['linux']", "[1]"), stub);
        assert!(
            diagnostics.iter().any(|id| id == "invalid-argument-type"),
            "{diagnostics:?}"
        );
        assert!(
            !diagnostics
                .iter()
                .any(|id| id == "incomplete-stub-validation"),
            "{diagnostics:?}"
        );

        let diagnostics = validate(
            "tags = ['linux']\nROWS = [{'tags': tags, 'numbers': tags}]\n",
            &stub.replace(
                "    tags: list[str]",
                "    tags: list[str]\n    numbers: list[int]",
            ),
        );
        assert!(
            diagnostics.iter().any(|id| id == "invalid-argument-type"),
            "{diagnostics:?}"
        );
        assert!(
            !diagnostics
                .iter()
                .any(|id| id == "incomplete-stub-validation"),
            "{diagnostics:?}"
        );
        assert!(validate(
            "tags = [-1, +2]\nROWS = [{'tags': tags}]\n",
            &stub.replace("list[str]", "list[int]"),
        )
        .is_empty());
    }

    #[test]
    fn named_list_leaves_require_fresh_unannotated_storage() {
        let stub = "class _Row(TypedDict):\n    tags: list[str]\nROWS: list[_Row]\n";
        for source in [
            "tags = []\nROWS = [{'tags': tags}]\n",
            "tags = [['linux']]\nROWS = [{'tags': tags}]\n",
            "def opaque(): return ['linux']\ntags = opaque()\nROWS = [{'tags': tags}]\n",
            "tags = ['linux'] # type: list[str]\nROWS = [{'tags': tags}]\n",
            "tags: list[str] = ['linux']\nROWS = [{'tags': tags}]\n",
            "tags, other = (['linux'], 1)\nROWS = [{'tags': tags}]\n",
            "row = {'tags': ['linux']}\nROWS = [row]\n",
            "tags = ['linux']\nalias = tags\nROWS = [{'tags': tags}]\n",
            "tags = ['linux']\ndef corrupt(value): value.append(1)\ncorrupt(tags)\nROWS = [{'tags': tags}]\n",
            "tags = ['linux']\nROWS = [{'tags': tags}]\ndef corrupt(value): value.append(1)\ncorrupt(tags)\n",
        ] {
            let diagnostics = validate(source, stub);
            assert!(
                diagnostics
                    .iter()
                    .any(|id| id == "incomplete-stub-validation"),
                "{source}: {diagnostics:?}"
            );
        }
        let diagnostics = validate(
            "tags = [1] # type: list[str]\nROWS = [{'tags': tags}]\n",
            stub,
        );
        assert!(
            diagnostics
                .iter()
                .any(|id| id == "incomplete-stub-validation"),
            "{diagnostics:?}"
        );
        assert!(
            diagnostics.iter().any(|id| id == "invalid-assignment"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn named_list_evidence_tracks_edits_without_changing_source_diagnostics() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let source = fixture.add_file(&mut analysis.db, "source.bzl", "");
        let stub = fixture.add_file(
            &mut analysis.db,
            "source.bzli",
            "class _Row(TypedDict):\n    tags: list[str]\nROWS: list[_Row]\n",
        );
        loader.add_files_from_fixture(&fixture);
        analysis.set_type_interfaces([(source, stub)]).unwrap();
        for (text, expected) in [
            (
                "tags = ['linux']\nROWS = [{'tags': tags}, {'tags': tags}]\n",
                None,
            ),
            (
                "tags = ['linux']\nROWS = [{'tags': tags}, {'tags': tags}]\nmethod = tags.append\n",
                Some("incomplete-stub-validation"),
            ),
            (
                "tags = ['linux']\nROWS = [{'tags': tags}, {'tags': tags}]\n",
                None,
            ),
            (
                "tags = [1]\nROWS = [{'tags': tags}, {'tags': tags}]\n",
                Some("invalid-argument-type"),
            ),
        ] {
            analysis.update_file(source, text.to_owned());
            assert!(analysis.snapshot().diagnostics(source).unwrap().is_empty());
            let reports = analysis.validate_stubs(|_| true).unwrap();
            let ids: Vec<_> = reports
                .iter()
                .flat_map(|(_, diagnostics)| diagnostics)
                .map(|diagnostic| diagnostic.id().as_str())
                .collect();
            if let Some(expected) = expected {
                assert!(ids.contains(&expected), "{reports:?}");
            } else {
                assert!(ids.is_empty(), "{reports:?}");
            }
            assert!(analysis.snapshot().diagnostics(source).unwrap().is_empty());
        }
    }

    #[test]
    fn readonly_extra_items_validate_fresh_dictionary_values() {
        let stub = "class _Row(TypedDict, extra_items=ReadOnly[object]):\n    name: str\nROWS: dict[str, _Row]\n";
        for extra_items in ["object", "ReadOnly[object]", "ReadOnly[Iterable[str]]"] {
            let diagnostics = validate(
                "ROWS = {'first': {'name': 'ok', 'platforms': ['linux']}}\n",
                &stub.replace("ReadOnly[object]", extra_items),
            );
            assert!(diagnostics.is_empty(), "{extra_items}: {diagnostics:?}");
        }
        for (source, expected) in [
            ("ROWS = {'first': {'name': 'ok'}}\n", None),
            (
                "ROWS = {'first': {'name': 'ok', 'platforms': ['linux']}}\n",
                None,
            ),
            (
                "ROWS = {'first': {'platforms': []}}\n",
                Some("missing-typed-dict-key"),
            ),
            (
                "ROWS = {'first': {'name': 1, 'platforms': []}}\n",
                Some("invalid-argument-type"),
            ),
            (
                "def opaque(): pass\nROWS = {'first': {'name': 'ok', 'platforms': opaque()}}\n",
                Some("incomplete-stub-validation"),
            ),
        ] {
            let diagnostics = validate(source, stub);
            if let Some(expected) = expected {
                assert!(
                    diagnostics.iter().any(|id| id == expected),
                    "{source}: {diagnostics:?}"
                );
                assert_eq!(
                    diagnostics
                        .iter()
                        .any(|id| id == "incomplete-stub-validation"),
                    expected == "incomplete-stub-validation",
                    "{source}: {diagnostics:?}"
                );
            } else {
                assert!(diagnostics.is_empty(), "{source}: {diagnostics:?}");
            }
        }
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
        let source = fixture.add_file(
            &mut analysis.db,
            "source.bzl",
            "ROWS = [{'name': 'ok', 'hidden': []}]\n",
        );
        let stub = fixture.add_file(&mut analysis.db, "source.bzli", "");
        loader.add_files_from_fixture(&fixture);
        analysis.set_type_interfaces([(source, stub)]).unwrap();
        for field_type in ["str", "int", "str"] {
            analysis.update_file(
                stub,
                format!("class _Row(TypedDict):\n    name: {field_type}\nROWS: list[_Row]\n"),
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
    fn nested_open_dictionary_contracts_use_literal_context() {
        for (source, stub) in [
            (
                "ROWS = {'inner': {'name': 'ok', 'private': 1}}\n",
                "class _Inner(TypedDict):\n    name: str\nclass _Outer(TypedDict, closed=True):\n    inner: _Inner\nROWS: _Outer\n",
            ),
            (
                "ROWS = [{'name': 'ok', 'private': 1}]\n",
                "class _Row(TypedDict):\n    name: str\nclass _Rows(Protocol):\n    def __getitem__(self, __index: int) -> _Row: ...\nROWS: _Rows\n",
            ),
        ] {
            let diagnostics = validate(source, stub);
            assert!(diagnostics.is_empty(), "{stub}: {diagnostics:?}");
        }

        let diagnostics = validate(
            "ROWS = [{'name': 'ok', 'private': 1}]\n",
            "class _Row(TypedDict):\n    name: str\nclass _Rows(Protocol):\n    def __getitem__(self, index: int) -> _Row: ...\nROWS: _Rows\n",
        );
        assert_eq!(diagnostics, ["invalid-assignment"]);
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
            ("def identity(value): return value\ndef make(): return identity\n", "def make() -> Callable[[int], int]: ...\n", Some("unsound-return-statement")),
            ("def identity(value): return value\nCALLBACKS = [identity]\ndef make(): return CALLBACKS\n", "def make() -> list[Callable[[int], int]]: ...\n", Some("invalid-return-type")),
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
        let previous = analysis
            .db
            .environment()
            .stub_validation(&analysis.db)
            .clone();
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
            assert_eq!(
                analysis.db.environment().stub_validation(&analysis.db),
                &previous
            );
            let diagnostics = analysis.snapshot().diagnostics(caller).unwrap();
            assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
            assert_eq!(diagnostics[0].id().as_str(), "invalid-argument-type");
        }
    }

    #[test]
    fn native_getattr_refines_literal_names() {
        for (expression, result, expected) in [
            ("getattr(value, 'field', 0)", "str | int", &[] as &[&str]),
            (
                "getattr(value, 'field', 0)",
                "str",
                &["invalid-return-type"],
            ),
            ("getattr(value, 'field')", "object", &[]),
            ("getattr(value, name, 0)", "object", &[]),
            ("getattr(value, name)", "object", &[]),
            ("getattr(value, name)", "str", &["unsound-return-statement"]),
            (
                "getattr(value, name)()",
                "object",
                &["incomplete-stub-validation"],
            ),
            ("getattr(value, 'field', None)", "str | None", &[]),
            ("getattr(value, 0)", "object", &["invalid-argument-type"]),
            (
                "getattr(x=value, name='field')",
                "object",
                &[
                    "positional-only-parameter-as-kwarg",
                    "positional-only-parameter-as-kwarg",
                ],
            ),
        ] {
            let source = format!("def make(value, name): return {expression}\n");
            let stub = format!("def make(value: struct[str], name: str) -> {result}: ...\n");
            assert_eq!(validate(&source, &stub), expected, "{expression}");
        }
        assert_eq!(
            validate(
                "def make(): return getattr(struct(field='value'), 'field')\n",
                "def make() -> str: ...\n"
            ),
            Vec::<String>::new(),
        );
        assert_eq!(
            validate(
                "def make(): return getattr(struct(field='value'), 'field')\n",
                "def make() -> int: ...\n"
            ),
            ["invalid-return-type"],
        );
        assert_eq!(
            validate(
                "def make(value, name): return getattr(value, name)\n",
                "def make(value: struct[object], name: str) -> object: ...\n"
            ),
            Vec::<String>::new(),
        );
        assert_eq!(
            validate(
                "def make(value): return getattr(value, 'relative')\n",
                "def make(value: Label) -> Callable[..., Label]: ...\n"
            ),
            Vec::<String>::new(),
        );
        assert_eq!(
            validate(
                "def make(value): return getattr(value, '__len__')\n",
                "def make(value: list[int]) -> Callable[..., int]: ...\n"
            ),
            ["incomplete-stub-validation"],
        );
        assert_eq!(
            validate(
                "def getattr(x, name): return 1\ndef make(value): return getattr(value, 'field')\n",
                "def getattr(x: object, name: str) -> int: ...\ndef make(value: struct[str]) -> int: ...\n"
            ),
            Vec::<String>::new(),
        );
    }

    #[test]
    fn selected_function_calls_check_declared_inputs() {
        let stub = r#"
def _id(callback: Callable[[list[Any]], int]) -> Callable[[list[Any]], int]: ...
def make(callback: Callable[[list[Any]], int]) -> Callable[[list[Any]], int]: ...
"#;
        for (prefix, helper, body, expected) in [
            ("", "callback", "_id(callback)", &[] as &[&str]),
            ("_alias = _id\n", "callback", "_alias(callback)", &[]),
            ("", "callback", "_id(lambda values: len(values))", &[]),
            (
                "",
                "callback",
                "_id(lambda values: values.append(1) or 1)",
                &["incomplete-stub-validation"],
            ),
            (
                "def _narrow(values: list[str]) -> int: return len(values)\n",
                "callback",
                "_id(_narrow)",
                &["incomplete-stub-validation"],
            ),
            (
                "def _narrow(values: list[str]) -> int: return len(values)\n",
                "callback",
                "_id(_narrow) # ty: ignore[incomplete-stub-validation]",
                &["incomplete-stub-validation"],
            ),
            (
                "",
                "callback",
                "_id(*[callback])",
                &["incomplete-stub-validation"],
            ),
            ("", "1", "_id(callback)", &["invalid-return-type"]),
        ] {
            let source = format!(
                "def _id(callback): return {helper}
{prefix}def make(callback): return {body}
"
            );
            assert_eq!(validate(&source, stub), expected, "{source}");
        }
        assert_eq!(
            validate(
                r#"
def _id(callback: Callable[[list[Any]], int]) -> Callable[[list[Any]], int]:
    return callback
def make(callback): return _id(callback)
"#,
                "def make(callback: Callable[[list[Any]], int]) -> Callable[[list[Any]], int]: ...\n",
            ),
            ["incomplete-stub-validation"],
        );
        assert_eq!(
            validate(
                r#"
def _id(values):
    values.append(1)
    return values
def make(values): return _id(values)
"#,
                "def _id(values: list[Any]) -> list[Any]: ...\ndef make(values: list[Any]) -> list[Any]: ...\n",
            ),
            ["incomplete-stub-validation"],
        );
        assert_eq!(
            validate(
                r#"
def _narrow(values: list[str]) -> int: return len(values)
def _id(value): return value
def make(): return _id(struct(run=_narrow))
"#,
                r#"
class _Runner(Protocol):
    @property
    def run(self) -> Callable[[list[Any]], int]: ...
def _id(value: _Runner) -> _Runner: ...
def make() -> _Runner: ...
"#,
            ),
            ["incomplete-stub-validation"],
        );
    }

    #[test]
    fn module_storage_requirements_include_value_only_contracts() {
        for (result, expected) in [
            (
                "Callable[..., None]",
                &["incomplete-stub-validation"] as &[&str],
            ),
            ("Callable[[int], None]", &[]),
        ] {
            let source = format!(
                "def opaque() -> {result}: return lambda value: None\ncallbacks: list[Callable[[int], None]] = []\ncallbacks.append(opaque())\n"
            );
            assert_eq!(
                validate(&source, "callbacks: list[Callable[[int], None]]\n"),
                expected,
                "{result}",
            );
        }
    }

    #[test]
    fn storage_requirements_follow_selected_execution_scopes() {
        let helper = "def opaque() -> Callable[..., None]: return lambda value: None\n";
        for body in [
            "callbacks.append(opaque())",
            "(lambda: callbacks.append(opaque()))()",
            "[callbacks.append(opaque()) for _ in [1]]",
            "{1: callbacks.append(opaque()) for _ in [1]}",
        ] {
            for selected in [false, true] {
                let source = format!("{helper}def use():\n    callbacks: list[Callable[[int], None]] = []\n    {body}\n    return 1\n");
                let stub = if selected {
                    "def use() -> int: ...\n"
                } else {
                    ""
                };
                let expected: &[&str] = if selected {
                    &["incomplete-stub-validation"]
                } else {
                    &[]
                };
                assert_eq!(
                    validate(&source, stub),
                    expected,
                    "{body}, selected={selected}"
                );
            }
        }
        assert_eq!(
            validate(
                &format!("{helper}callbacks: list[Callable[[int], None]] = []\ndef make(value=callbacks.append(opaque())): return 1\n"),
                "def make(value: None = ...) -> int: ...\n",
            ),
            ["incomplete-stub-validation"],
        );
    }

    #[test]
    fn module_initialization_does_not_select_unrelated_lazy_bodies() {
        for body in [
            "def unused(): consume(opaque())",
            "unused = lambda: consume(opaque())",
            "unused = lambda: [consume(opaque()) for _ in [1]]",
        ] {
            let source = format!(
                "def consume(value: Callable[[int], None]) -> None: pass\ndef opaque() -> Callable[..., None]: return lambda value: None\n{body}\nVALUE = 1\n"
            );
            assert_eq!(
                validate(&source, "VALUE: int\n"),
                Vec::<&str>::new(),
                "{body}"
            );
        }
        for expression in [
            "lambda value=consume(opaque()): None",
            "[consume(opaque()) for _ in [1]]",
        ] {
            let source = format!(
                "def consume(value: Callable[[int], None]) -> None: pass\ndef opaque() -> Callable[..., None]: return lambda value: None\nunused = {expression}\nVALUE = 1\n"
            );
            assert_eq!(
                validate(&source, "VALUE: int\n"),
                ["incomplete-stub-validation"],
                "{expression}"
            );
        }
    }

    #[test]
    fn selected_function_calls_accept_declared_defaults() {
        for (parameters, declarations, domain, calls) in [
            (
                "values = []",
                "values: list[Any] = ...",
                "list[Any]",
                ["helper(values)", "helper()"],
            ),
            (
                "values, flag = False",
                "values: list[Any], flag: bool = ...",
                "list[Any]",
                ["helper(values)", "helper(values, flag=True)"],
            ),
            (
                "*, values = {}",
                "*, values: dict[str, list[Any]] = ...",
                "dict[str, list[Any]]",
                ["helper(values=values)", "helper()"],
            ),
        ] {
            for call in calls {
                let source = format!(
                    "def helper({parameters}):
    return len(values)
def forward(values):
    return {call}
"
                );
                let stub = format!(
                    "def helper({declarations}) -> int: ...
def forward(values: {domain}) -> int: ...
"
                );
                assert_eq!(validate(&source, &stub), Vec::<String>::new(), "{source}");
            }
        }
        // An external declaration describes supplied arguments. The body may safely
        // handle a different implementation default.
        assert!(validate(
            "def helper(value=None): return 0\ndef forward(): return helper()\n",
            "def helper(value: int = ...) -> int: ...\ndef forward() -> int: ...\n",
        )
        .is_empty());
    }

    #[test]
    fn selected_function_defaults_keep_independent_obligations() {
        for (source, stub, expected) in [
            (
                "def helper(value: int = 'bad') -> int: return 0\ndef forward(): return helper()\n",
                "def helper(value: int = ...) -> int: ...\ndef forward() -> int: ...\n",
                "invalid-parameter-default",
            ),
            (
                "def helper(value='bad'): return value
def forward(): return helper()
",
                "def helper(value: int = ...) -> int: ...
def forward() -> int: ...
",
                "invalid-return-type",
            ),
            (
                "def needs_int(value: int) -> int: return value
def helper(value=needs_int('bad')): # ty: ignore[invalid-argument-type]
    return value
def forward(): return helper()
",
                "def helper(value: int = ...) -> int: ...
def forward() -> int: ...
",
                "incomplete-stub-validation",
            ),
            (
                "def opaque() -> Any: return []
def helper(values=opaque()): return len(values)
def forward(): return helper()
",
                "def helper(values: list[Any] = ...) -> int: ...
def forward() -> int: ...
",
                "incomplete-stub-validation",
            ),
            (
                "def helper(values): return len(values)
def forward(): return helper()
",
                "def helper(values: list[Any]) -> int: ...
def forward() -> int: ...
",
                "missing-argument",
            ),
            (
                "def helper(values=[]):
    values.append(1)
    return len(values)
def forward(values): return helper(values)
",
                "def helper(values: list[Any] = ...) -> int: ...
def forward(values: list[Any]) -> int: ...
",
                "incomplete-stub-validation",
            ),
            (
                "def helper(values: list[Any] = []) -> int:
    values.append(1)
    return len(values)
def forward(values): return helper(values)
",
                "def forward(values: list[Any]) -> int: ...
",
                "incomplete-stub-validation",
            ),
            (
                "def narrow(values: list[str]) -> int: return len(values)
def helper(callback, flag=False): return 0
def forward(): return helper(narrow)
",
                "def helper(callback: Callable[[list[Any]], int], flag: bool = ...) -> int: ...
def forward() -> int: ...
",
                "incomplete-stub-validation",
            ),
            (
                "def helper(values=[]): return len(values)
def forward(values): return helper(*values)
",
                "def helper(values: list[Any] = ...) -> int: ...
def forward(values: tuple[list[Any], ...]) -> int: ...
",
                "incomplete-stub-validation",
            ),
        ] {
            let diagnostics = validate(source, stub);
            assert!(
                diagnostics.iter().any(|id| id == expected),
                "{source}: {diagnostics:?}",
            );
        }
    }

    #[test]
    fn selected_function_calls_follow_caller_scope() {
        for expression in [
            "_id(_narrow)",
            "lambda: _id(_narrow)",
            "[_id(_narrow) for _ in [1]]",
            "{1: _id(_narrow) for _ in [1]}",
        ] {
            let source = format!(
                "def _id(callback): return callback
def _narrow(values: list[str]) -> int: return len(values)
def _caller(): return {expression}
_caller()
"
            );
            for selected in [false, true] {
                let mut stub = "def _id(callback: Callable[[list[Any]], int]) -> Callable[[list[Any]], int]: ...\n".to_owned();
                if selected {
                    stub.push_str("def _caller() -> object: ...\n");
                }
                let expected: &[&str] = if selected {
                    &["incomplete-stub-validation"]
                } else {
                    &[]
                };
                assert_eq!(
                    validate(&source, &stub),
                    expected,
                    "{expression}, selected={selected}",
                );
            }
        }
    }

    #[test]
    fn selected_function_imports_require_source_identity() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let helper = fixture.add_file(
            &mut analysis.db,
            "helper.bzl",
            "def forward(callback): return callback\n",
        );
        let helper_stub = fixture.add_file(&mut analysis.db, "helper.bzli", "def forward(callback: Callable[[list[Any]], int]) -> Callable[[list[Any]], int]: ...\n");
        let source = fixture.add_file(
            &mut analysis.db,
            "source.bzl",
            "load('helper.bzl', _forward='forward')\nforward = _forward\ndef make(callback): return forward(callback)\n",
        );
        let stub = fixture.add_file(&mut analysis.db, "source.bzli", "");
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
        let declaration =
            "def make(callback: Callable[[list[Any]], int]) -> Callable[[list[Any]], int]: ...\n";
        for redirected in [false, true, false] {
            let mut interfaces = vec![(source, stub)];
            let declarations = if redirected {
                interfaces.push((helper, helper_stub));
                declaration.to_owned()
            } else {
                format!("{declaration}def forward(callback: Callable[[list[Any]], int]) -> Callable[[list[Any]], int]: ...\n")
            };
            analysis.update_file(stub, declarations);
            analysis.set_type_interfaces(interfaces).unwrap();
            let previous = analysis
                .db
                .environment()
                .stub_validation(&analysis.db)
                .clone();
            let diagnostics: Vec<_> = analysis
                .validate_stubs(|_| true)
                .unwrap()
                .into_iter()
                .flat_map(|(_, diagnostics)| diagnostics)
                .collect();
            assert_eq!(
                diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.id().as_str())
                    .collect::<Vec<_>>(),
                Vec::<&str>::new(),
                "redirected={redirected}: {diagnostics:?}"
            );
            assert_eq!(
                analysis.db.environment().stub_validation(&analysis.db),
                &previous
            );
        }

        analysis
            .set_type_interfaces([(source, stub), (helper, helper_stub)])
            .unwrap();
        analysis.update_file(stub, declaration.to_owned());
        for narrow in [false, true, false] {
            let argument = if narrow { "_narrow" } else { "callback" };
            let helper = if narrow {
                "def _narrow(values: list[str]) -> int: return len(values)\n"
            } else {
                ""
            };
            analysis.update_file(
                source,
                format!(
                    "load('helper.bzl', _forward='forward')
forward = _forward
{helper}def make(callback): return forward({argument})
"
                ),
            );
            let previous = analysis
                .db
                .environment()
                .stub_validation(&analysis.db)
                .clone();
            let reports = analysis.validate_stubs(|_| true).unwrap();
            let diagnostics: Vec<_> = reports
                .iter()
                .flat_map(|(_, diagnostics)| diagnostics)
                .map(|diagnostic| diagnostic.id().as_str())
                .collect();
            let expected: &[&str] = if narrow {
                &["incomplete-stub-validation"]
            } else {
                &[]
            };
            assert_eq!(diagnostics, expected, "narrow={narrow}: {reports:?}");
            assert_eq!(
                analysis.db.environment().stub_validation(&analysis.db),
                &previous,
            );
        }
    }

    #[test]
    fn contextual_lambda_inputs_check_consuming_operations() {
        assert_eq!(
            validate(
                "def make(): return lambda values: len(values)\n",
                "def make() -> Callable[[list[Any]], int]: ...\n",
            ),
            Vec::<String>::new(),
        );
        assert_eq!(
            validate(
                "def make(): return lambda values: values.append(1)\n",
                "def make() -> Callable[[list[Any]], None]: ...\n",
            ),
            ["incomplete-stub-validation"],
        );
    }

    #[test]
    fn nested_execution_scopes_check_opaque_operations() {
        let helper = r#"
def _opaque() -> Callable[..., None]:
    return lambda: None
"#;
        for (body, result, expected) in [
            (
                "lambda: _opaque()",
                "Callable[[], Callable[..., None]]",
                &[] as &[&str],
            ),
            (
                "lambda: lambda: _opaque()",
                "Callable[[], Callable[[], Callable[..., None]]]",
                &[],
            ),
            (
                "lambda: [_opaque() for _ in [1]][0]",
                "Callable[[], Callable[..., None]]",
                &[],
            ),
            (
                "lambda: {key: _opaque() for key in ['build']}['build']",
                "Callable[[], Callable[..., None]]",
                &[],
            ),
            (
                "lambda: _opaque()()",
                "Callable[[], None]",
                &["incomplete-stub-validation"],
            ),
        ] {
            assert_eq!(
                validate(
                    &format!("{helper}def make(): return {body}\n"),
                    &format!("def make() -> {result}: ...\n"),
                ),
                expected,
                "{body}",
            );
        }
        assert_eq!(
            validate(
                "def make(values): return lambda: values.append(1)\n",
                "def make(values: list[Any]) -> Callable[[], None]: ...\n",
            ),
            ["incomplete-stub-validation"],
        );
    }

    #[test]
    fn owned_defaults_and_provider_bodies_check_inputs() {
        use starpls_common::Db as _;

        for (case, callback_type, body, stub, expected) in [
            ("provider_body_narrow", "str", "def _init(): return {'value': callback_value in test_container}\nInfo, _ = provider(fields = ['value'], init = _init)\n", "class Info:\n    value: Final[bool]\n    def __init__(self) -> None: ...\n", &["incomplete-stub-validation"] as &[&str]),
            ("provider_body_known", "object", "def _init(): return {'value': callback_value in test_container}\nInfo, _ = provider(fields = ['value'], init = _init)\n", "class Info:\n    value: Final[bool]\n    def __init__(self) -> None: ...\n", &[]),
            ("default_narrow", "str", "def make(callback=lambda: callback_value in test_container): return callback\n", "def make(callback: Callable[[], bool] = ...) -> Callable[[], bool]: ...\n", &["incomplete-stub-validation"]),
            ("default_comprehension", "str", "def make(callback=lambda: [callback_value in test_container for _ in [0]]): return callback\n", "def make(callback: Callable[[], list[bool]] = ...) -> Callable[[], list[bool]]: ...\n", &["incomplete-stub-validation"]),
            ("default_known", "object", "def make(callback=lambda: callback_value in test_container): return callback\n", "def make(callback: Callable[[], bool] = ...) -> Callable[[], bool]: ...\n", &[]),
            ("provider_default_narrow", "str", "def _init(callback=lambda: callback_value in test_container): return {'value': callback()}\nInfo, _ = provider(fields = ['value'], init = _init)\n", "class Info:\n    value: Final[bool]\n    def __init__(self, callback: Callable[[], bool] = ...) -> None: ...\n", &["incomplete-stub-validation"]),
            ("provider_default_known", "object", "def _init(callback=lambda: callback_value in test_container): return {'value': callback()}\nInfo, _ = provider(fields = ['value'], init = _init)\n", "class Info:\n    value: Final[bool]\n    def __init__(self, callback: Callable[[], bool] = ...) -> None: ...\n", &[]),
            ("unrelated_default", "str", "deferred = lambda: callback_value in test_container\nVALUE = True\n", "VALUE: bool\n", &[]),
        ] {
            let (mut analysis, loader) = Analysis::new_for_test();
            let mut fixture = Fixture::new(&mut analysis.db);
            let source_text = format!("def callback_value(value: {callback_type}) -> None: pass\n{body}");
            let source = fixture.add_file(&mut analysis.db, "source.bzl", &source_text);
            let stub = fixture.add_file(&mut analysis.db, "source.bzli", stub);
            loader.add_files_from_fixture(&fixture);
            let builtins = starpls_bazel::decode_builtins(include_bytes!("../../../starpls/src/builtin/builtin.pb")).unwrap();
            analysis.set_builtin_defs(builtins.clone(), Default::default()).unwrap();
            let super::super::native::DeclarationSource { path, mut contents } =
                super::super::native::generate(starpls_common::Dialect::Bazel, &builtins, &Default::default()).unwrap();
            contents.push_str("\nclass _ContainmentProbe:\n    def __contains__(self, callback: _starpls_typing.Callable[[_starpls_typing.Any], None], /) -> _starpls_builtins.bool: ...\n_starpls_Bzl_test_container: _ContainmentProbe\n");
            analysis.db.source_system_mut().set_virtual_source(&path, contents);
            let native_file = analysis.db.files.try_virtual_file(&path).unwrap();
            native_file.sync(&mut analysis.db);
            analysis.set_type_interfaces([(source, stub)]).unwrap();
            let reports = analysis.validate_stubs(|_| true).unwrap();
            let diagnostics: Vec<_> = reports.into_iter().flat_map(|(_, diagnostics)| diagnostics).collect();
            let ids: Vec<_> = diagnostics.iter().map(|diagnostic| diagnostic.id().as_str()).collect();
            assert_eq!(ids, expected, "{case}");
            if case == "provider_body_narrow" {
                assert_eq!(diagnostics[0].headline_message(), "Cannot prove initializer for `Info`: some operation inputs could not be proved");
            }
        }
    }

    #[test]
    fn membership_checks_selected_inputs() {
        use starpls_common::Db as _;

        let native_property = validate(
            "def read(value): return value.build_setting_value\n",
            "def read(value: ctx[int]) -> int: ...\n",
        );
        assert!(native_property.is_empty(), "{native_property:?}");
        for (case, callback_type, setup, expression, expected) in [
            (
                "direct_narrow",
                "str",
                "",
                "callback_value in test_container",
                &["incomplete-stub-validation"] as &[&str],
            ),
            (
                "direct_known",
                "object",
                "",
                "callback_value in test_container",
                &[],
            ),
            (
                "subscript_narrow",
                "str",
                "",
                "'x' in test_indexer[callback_value]",
                &["incomplete-stub-validation"],
            ),
            (
                "subscript_known",
                "object",
                "",
                "'x' in test_indexer[callback_value]",
                &[],
            ),
            (
                "stored_subscript_narrow",
                "str",
                "TABLE = test_indexer[callback_value]\n",
                "'x' in TABLE",
                &["incomplete-stub-validation"],
            ),
            (
                "unary_narrow",
                "str",
                "",
                "'x' in +test_unary",
                &["incomplete-stub-validation"],
            ),
            ("unary_known", "str", "", "'x' in +test_unary", &[]),
            (
                "stored_unary_narrow",
                "str",
                "TABLE = +test_unary\n",
                "'x' in TABLE",
                &["incomplete-stub-validation"],
            ),
            (
                "descriptor_narrow",
                "str",
                "",
                "'x' in test_box.field",
                &["incomplete-stub-validation"],
            ),
            ("descriptor_known", "str", "", "'x' in test_box.field", &[]),
            (
                "stored_descriptor_narrow",
                "str",
                "TABLE = test_box.field\n",
                "'x' in TABLE",
                &["incomplete-stub-validation"],
            ),
            (
                "rich_narrow",
                "str",
                "",
                "'x' in (test_comparer < callback_value)",
                &["incomplete-stub-validation"],
            ),
            (
                "rich_known",
                "object",
                "",
                "'x' in (test_comparer < callback_value)",
                &[],
            ),
            (
                "stored_rich_narrow",
                "str",
                "TABLE = test_comparer < callback_value\n",
                "'x' in TABLE",
                &["incomplete-stub-validation"],
            ),
        ] {
            let (mut analysis, loader) = Analysis::new_for_test();
            let mut fixture = Fixture::new(&mut analysis.db);
            let source_text = format!("def callback_value(value: {callback_type}) -> None: pass\n{setup}def make(): return {expression}\n");
            let source = fixture.add_file(&mut analysis.db, "source.bzl", &source_text);
            let stub =
                fixture.add_file(&mut analysis.db, "source.bzli", "def make() -> bool: ...\n");
            loader.add_files_from_fixture(&fixture);
            let builtins = starpls_bazel::decode_builtins(include_bytes!(
                "../../../starpls/src/builtin/builtin.pb"
            ))
            .unwrap();
            analysis
                .set_builtin_defs(builtins.clone(), Default::default())
                .unwrap();
            let super::super::native::DeclarationSource { path, mut contents } =
                super::super::native::generate(
                    starpls_common::Dialect::Bazel,
                    &builtins,
                    &Default::default(),
                )
                .unwrap();
            contents.push_str("\nclass _ContainmentProbe:\n    def __contains__(self, callback: _starpls_typing.Callable[[_starpls_typing.Any], None], /) -> _starpls_builtins.bool: ...\n_starpls_Bzl_test_container: _ContainmentProbe\nclass _IndexerProbe:\n    def __getitem__(self, callback: _starpls_typing.Callable[[_starpls_typing.Any], None], /) -> _starpls_builtins.dict[_starpls_builtins.str, _starpls_typing.Any]: ...\n_starpls_Bzl_test_indexer: _IndexerProbe\n");
            let domain = if case.ends_with("known") {
                "_starpls_builtins.str"
            } else {
                "_starpls_typing.Any"
            };
            contents.push_str(&format!(r#"
_ProbeT = _starpls_typing.TypeVar("_ProbeT")
class _UnaryProbe(_starpls_typing.Generic[_ProbeT]):
    def __pos__(self: _UnaryProbe[_starpls_typing.Callable[[{domain}], None]]) -> _starpls_builtins.dict[_starpls_builtins.str, _starpls_typing.Any]: ...
_starpls_Bzl_test_unary: _UnaryProbe[_starpls_typing.Callable[[_starpls_builtins.str], None]]
class _DescriptorProbe:
    def __get__(self, instance: _BoxProbe[_starpls_typing.Callable[[{domain}], None]], owner: _starpls_builtins.object = None) -> _starpls_builtins.dict[_starpls_builtins.str, _starpls_typing.Any]: ...
_starpls_Bzl_test_descriptor: _DescriptorProbe
class _BoxProbe(_starpls_typing.Generic[_ProbeT]):
    field = _DescriptorProbe()
_starpls_Bzl_test_box: _BoxProbe[_starpls_typing.Callable[[_starpls_builtins.str], None]]
"#));
            contents.push_str(r#"
class _ComparisonProbe:
    def __lt__(self, callback: _starpls_typing.Callable[[_starpls_typing.Any], None], /) -> _starpls_builtins.dict[_starpls_builtins.str, _starpls_typing.Any]: ...
_starpls_Bzl_test_comparer: _ComparisonProbe
"#);
            analysis
                .db
                .source_system_mut()
                .set_virtual_source(&path, contents);
            let native_file = analysis.db.files.try_virtual_file(&path).unwrap();
            native_file.sync(&mut analysis.db);
            analysis.set_type_interfaces([(source, stub)]).unwrap();
            let reports = analysis.validate_stubs(|_| true).unwrap();
            let ids: Vec<_> = reports
                .into_iter()
                .flat_map(|(_, diagnostics)| diagnostics)
                .map(|diagnostic| diagnostic.id().as_str().to_owned())
                .collect();
            assert_eq!(ids, expected, "{case}: {source_text}");
        }
    }

    #[test]
    fn opaque_global_collections_check_consuming_operations() {
        for source in [
            "TABLE = {'string': None}\nRESULT = 'string' in TABLE\n",
            "VALUES = [None]\nRESULT = None not in VALUES\n",
        ] {
            let diagnostics = validation_diagnostics(source, "RESULT: bool\n");
            assert!(diagnostics.is_empty(), "{source}: {diagnostics:#?}");
        }
        let dictionary = "TABLE = {'string': None}\n";
        for (body, stub, expected) in [
            (
                "def contains(key): return key in TABLE\n",
                "def contains(key: str) -> bool: ...\n",
                &[] as &[&str],
            ),
            (
                "def contains(key): return key not in TABLE\n",
                "def contains(key: str) -> bool: ...\n",
                &[],
            ),
            (
                "def read(key): return TABLE[key]\n",
                "def read(key: str) -> None: ...\n",
                &["unsound-return-statement"],
            ),
            (
                "def chained(key): return key in TABLE == {}\n",
                "def chained(key: str) -> bool: ...\n",
                &[],
            ),
            (
                "def make(callback=lambda: 'string' in TABLE): return callback\n",
                "def make(callback: Callable[[], bool] = ...) -> Callable[[], bool]: ...\n",
                &["incomplete-stub-validation"],
            ),
            (
                "def dynamic(receiver, key): return key in receiver\n",
                "def dynamic(receiver: Any, key: str) -> bool: ...\n",
                &["incomplete-stub-validation"],
            ),
        ] {
            let source = format!("{dictionary}{body}");
            let diagnostics = validation_diagnostics(&source, stub);
            let ids: Vec<_> = diagnostics
                .iter()
                .map(|diagnostic| diagnostic.id().as_str().to_owned())
                .collect();
            assert_eq!(ids, expected, "{source}: {diagnostics:#?}");
        }
        assert_eq!(
            validate(
                "def opaque() -> Any: return None\nTABLE = {'run': opaque()}\ndef invoke(): TABLE['run']()\n",
                "def invoke() -> None: ...\n",
            ),
            ["incomplete-stub-validation"],
        );
        let globals = "def opaque() -> Any: return None\nVALUES = [opaque()]\n";
        let collector = r#"
def collect(extra):
    if not extra:
        return VALUES
    out = list(VALUES)
    for item in extra:
        if item not in out:
            out.append(item)
    return out
"#;
        assert!(validate(
            &format!("{globals}{collector}"),
            "def collect(extra: Iterable[object] | None) -> Sequence[object]: ...\n",
        )
        .is_empty());
        for body in ["VALUES.append(1)", "VALUES[0]()"] {
            assert_eq!(
                validate(
                    &format!("{globals}def compute():\n    {body}\n"),
                    "def compute() -> None: ...\n",
                ),
                ["incomplete-stub-validation"],
                "{body}",
            );
        }
    }

    #[test]
    fn discarded_values_preserve_operation_requirements() {
        for (body, expected) in [
            ("json.decode('0')", &[] as &[&str]),
            ("{}", &[]),
            ("{'key': json.decode('0')}", &[]),
            // Dictionary key types are unbounded; validation does not model hashing.
            ("{json.decode('[]'): 1}", &[]),
            ("{[0]: 1}", &[]),
            (
                "json.decode('0').missing_attribute",
                &["incomplete-stub-validation"],
            ),
            (
                "(lambda: json.decode('0').missing_attribute)()",
                &["incomplete-stub-validation"],
            ),
            (
                "for _ in json.decode('0'):\n        pass",
                &["incomplete-stub-validation"],
            ),
            (
                "[item for item in json.decode('0')]",
                &["incomplete-stub-validation"],
            ),
            ("for _ in [json.decode('0')]:\n        pass", &[]),
            (
                "for _first, _second in (json.decode('0'),):\n        pass",
                &["incomplete-stub-validation"],
            ),
        ] {
            let source = format!("def make():\n    {body}\n    return None\n");
            assert_eq!(
                validate(&source, "def make() -> None: ...\n"),
                expected,
                "{body}"
            );
        }
    }

    #[test]
    fn type_is_checks_list_operations_without_widening_writes() {
        let source = r#"
_LIST_TYPE = type([])
def is_list(value):
    return type(value) == _LIST_TYPE

def extend_values(current, value):
    if not is_list(current) or not is_list(value):
        fail("Both values must be lists")
    tail = value
    if current[-len(tail):] == tail:
        return current
    return current + tail

def read(value):
    if is_list(value):
        return value[0]
    return ""
"#;
        let stub = r#"
def is_list(value: object) -> TypeIs[list[Any]]: ...
def extend_values(current: object, value: object) -> Sequence[object]: ...
def read(value: list[str]) -> str: ...
"#;
        let diagnostics = validation_diagnostics(source, stub);
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
        for (condition, expected) in [
            ("not is_list(current)", "invalid-argument-type"),
            ("not is_list(value)", "not-subscriptable"),
        ] {
            let diagnostics = validate(
                &source.replace("not is_list(current) or not is_list(value)", condition),
                stub,
            );
            assert!(
                diagnostics.iter().any(|id| id == expected),
                "{condition}: {diagnostics:?}",
            );
            assert!(
                diagnostics.iter().any(|id| id == "unsupported-operator"),
                "{condition}: {diagnostics:?}",
            );
        }
        assert_eq!(
            validate(
                &source.replace(
                    "return value[0]",
                    "value.append(1)\n        return value[0]"
                ),
                stub,
            ),
            ["invalid-argument-type"],
        );
        assert_eq!(
            validate(
                &source.replace("return \"\"", "return 0"),
                &stub.replace("value: list[str]) -> str", "value: object) -> int"),
            ),
            ["unsound-return-statement"],
        );
        let predicate = source.split("\ndef extend_values").next().unwrap();
        let observed = format!(
            "{predicate}\ndef observe():\n    value = json.decode('[]')\n    if is_list(value):\n        [item for item in value]\n"
        );
        let observed_stub =
            "def is_list(value: object) -> TypeIs[list[Any]]: ...\ndef observe() -> None: ...\n";
        assert_eq!(validate(&observed, observed_stub), Vec::<String>::new());
        assert_eq!(
            validate(
                &observed.replace("if is_list(value):", "if True:"),
                observed_stub
            ),
            ["incomplete-stub-validation"],
        );
        assert_eq!(
            validate(
                predicate,
                "def is_list(value: object) -> TypeIs[list[int]]: ...\n",
            ),
            ["incomplete-stub-validation"],
        );
        // Both implications would be vacuous without the target/input legality check.
        assert_eq!(
            validate(
                "def is_string(value): return type(value) == 'string'\n",
                "def is_string(value: int) -> TypeIs[str]: ...\n",
            ),
            [
                "incomplete-stub-validation",
                "invalid-type-guard-definition"
            ],
        );
    }

    #[test]
    fn type_is_select_preserves_value_domains() {
        let predicate = r#"
_SELECT_TYPE = type(select({"//conditions:default": []}))
def is_select(value):
    return type(value) == _SELECT_TYPE
"#;
        let predicate_stub = "def is_select(value: object) -> TypeIs[select[object]]: ...\n";
        for (annotation, fallback) in [("int", "0"), ("list[str]", "[]")] {
            let source = format!(
                r#"{predicate}
def selected(value):
    if is_select(value):
        return value
    return select({{"//conditions:default": {fallback}}})

def direct(value):
    if not is_select(value):
        return value
    return {fallback}
"#
            );
            let stub = format!(
                "{predicate_stub}def selected(value: select[{annotation}] | {annotation}) -> select[{annotation}]: ...\ndef direct(value: select[{annotation}] | {annotation}) -> {annotation}: ...\n"
            );
            let diagnostics = validation_diagnostics(&source, &stub);
            assert!(diagnostics.is_empty(), "{annotation}: {diagnostics:#?}");
        }
        assert_eq!(
            validate(
                predicate,
                "def is_select(value: object) -> TypeIs[select[int]]: ...\n",
            ),
            ["incomplete-stub-validation"],
        );
        let source = format!(
            "{predicate}def guess(value):\n    if is_select(value):\n        return value\n    return select({{'//conditions:default': 0}})\n"
        );
        let stub = format!("{predicate_stub}def guess(value: object) -> select[int]: ...\n");
        assert_eq!(validate(&source, &stub), ["invalid-return-type"]);
    }

    #[test]
    fn list_copies_preserve_result_domains() {
        for stub in [
            r#"
def is_list(value: object) -> TypeGuard[Sequence[object]]: ...
def clone(value: str | Label | int | bool | list[Any] | None) -> str | Label | int | bool | list[Any] | None: ...
"#,
            r#"
def is_list(value: object) -> TypeIs[list[Any]]: ...
def clone[T: (str, Label, int, bool, list[Any], None)](value: T) -> T: ...
"#,
        ] {
            for (result, expected) in [("value", vec![]), ("{}", vec!["invalid-return-type"])] {
                let source = format!(
                    r#"
_LIST_TYPE = type([])
def is_list(value):
    return type(value) == _LIST_TYPE

def clone(value):
    if is_list(value):
        return list(value)
    return {result}
"#
                );
                let diagnostics = validation_diagnostics(&source, stub);
                let actual = diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.id().as_str())
                    .collect::<Vec<_>>();
                assert_eq!(
                    actual, expected,
                    "{stub}\nreturn {result}: {diagnostics:#?}"
                );
            }
        }
    }

    #[test]
    fn opaque_parameter_inputs_check_consuming_operations() {
        assert!(validate(
            "def forward(callback): return callback\n",
            "def forward(callback: Callable[..., int]) -> Callable[..., int]: ...\n",
        )
        .is_empty());
        let diagnostics = validation_diagnostics(
            "def collect(values):\n    result = list(values)\n    result.append(1)\n    return result\n",
            "def collect(values: list[Any]) -> Sequence[object]: ...\n",
        );
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
        for body in [
            "values.append(1)",
            "values[0] = 1",
            "alias: list[Any] = values\n    alias.append(1)",
        ] {
            assert_eq!(
                validate(
                    &format!("def mutate(values):\n    {body}\n    return 1\n"),
                    "def mutate(values: list[Any]) -> int: ...\n",
                ),
                ["incomplete-stub-validation"],
                "{body}",
            );
        }
        assert_eq!(
            validate(
                "def invoke(callback):\n    callback()\n    return 1\n",
                "def invoke(callback: Callable[..., int]) -> int: ...\n",
            ),
            ["incomplete-stub-validation"],
        );
        let source = r#"
def only_strings(values: list[str]) -> int:
    return len(values[0])
def mixed(runner, flag):
    return runner if flag else struct(run=only_strings)
"#;
        let stub = r#"
class _Runner(Protocol):
    @property
    def run(self) -> Callable[[list[Any]], int]: ...
def mixed(runner: _Runner, flag: bool) -> _Runner: ...
"#;
        assert_eq!(validate(source, stub), ["incomplete-stub-validation"]);
    }

    #[test]
    fn gradual_signatures_require_matching_contracts_and_static_bodies() {
        assert_eq!(
            validate(
                "def helper(value): return 1\ndef compute(value): return helper(value)\n",
                "def helper(value: Any) -> int: ...\ndef compute(value: Any) -> int: ...\n",
            ),
            Vec::<String>::new(),
        );
        for annotation in ["Any", "list[Any]", "Callable[..., Any]"] {
            let stub = format!("def compute(value: {annotation}) -> int: ...\n");
            assert!(
                validate("def compute(value): return 1\n", &stub).is_empty(),
                "{annotation}"
            );
        }
        assert!(validate(
            "def compute(value: Any) -> int: return 1\n",
            "def compute(value: Any) -> int: ...\n"
        )
        .is_empty());
        for (source, stub) in [
            (
                "def compute(value: int) -> int: return 1\n",
                "def compute(value: Any) -> int: ...\n",
            ),
            (
                "def compute(value: list[str]) -> int: return 1\n",
                "def compute(value: list[Any]) -> int: ...\n",
            ),
            (
                "def compute(value): return 1\n",
                "def compute(value) -> int: ...\n",
            ),
            (
                "def compute(value): return 1\n",
                "def compute(value: Any): ...\n",
            ),
            (
                "def compute(value):\n    value.append('x')\n    return 1\n",
                "def compute(value: list[Any]) -> int: ...\n",
            ),
            (
                "def compute(value):\n    value(1, unexpected=True)\n    return 1\n",
                "def compute(value: Callable[..., Any]) -> int: ...\n",
            ),
            (
                "def compute(value):\n    value.missing()\n    return 1\n",
                "def compute(value: Any) -> int: ...\n",
            ),
            (
                "def wants_int(value): return value\ndef compute(value): return wants_int(value)\n",
                "def wants_int(value: int) -> int: ...\ndef compute(value: Any) -> int: ...\n",
            ),
        ] {
            let diagnostics = validate(source, stub);
            assert!(
                diagnostics
                    .iter()
                    .any(|id| id == "incomplete-stub-validation"),
                "{source}: {diagnostics:?}"
            );
        }
        assert_eq!(
            validate(
                "def helper(): return 1\ndef compute(value): return helper()\n",
                "def compute(value: Any) -> int: ...\n",
            ),
            ["unsound-return-statement"]
        );
        assert_eq!(
            validate(
                "def compute(value): return 'wrong'\n",
                "def compute(value: Any) -> int: ...\n"
            ),
            ["invalid-return-type"]
        );
        assert!(validate(
            "def compute(value): return len(value)\n",
            "def compute(value: list[object]) -> int: ...\n"
        )
        .is_empty());
        assert!(validate(
            "def compute(value): return len(value)\n",
            "def compute(value: list[Any]) -> int: ...\n"
        )
        .is_empty());
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
