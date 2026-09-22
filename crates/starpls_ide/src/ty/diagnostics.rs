//! Starlark diagnostic policy over Ty's inference, bindings, and reachability facts.

use std::sync::LazyLock;

use ruff_db::diagnostic::Annotation;
use ruff_db::diagnostic::Diagnostic;
use ruff_db::diagnostic::DiagnosticId;
use ruff_db::diagnostic::DiagnosticTag;
use ruff_db::diagnostic::LintName;
use ruff_db::diagnostic::Severity;
use ruff_db::diagnostic::Span;
use ruff_python_ast::visitor;
use ruff_python_ast::visitor::Visitor;
use ruff_python_ast::ArgOrKeyword;
use ruff_python_ast::Expr;
use ruff_python_ast::ExprCall;
use ruff_text_size::Ranged;
use ruff_text_size::TextRange;
use starpls_common::File;
use starpls_hir::Db as _;
use ty_python_core::definition::Definition;
use ty_python_core::definition::DefinitionKind;
use ty_python_core::place::ScopedPlaceId;
use ty_python_core::scope::ScopeKind;
use ty_python_core::semantic_index;
use ty_python_semantic::lint::Level;
use ty_python_semantic::lint::LintMetadata;
use ty_python_semantic::lint::LintRegistry;
use ty_python_semantic::lint::LintRegistryBuilder;
use ty_python_semantic::lint::LintSource;
use ty_python_semantic::lint::LintStatus;
use ty_python_semantic::lint::RuleSelection;
use ty_python_semantic::provided::BuiltinUsage;
use ty_python_semantic::types::check_types_with_diagnostics;
use ty_python_semantic::types::ide_support::resolved_call_signature;
use ty_python_semantic::types::ide_support::unreachable_ranges;
use ty_python_semantic::types::ide_support::unused_definitions;
use ty_python_semantic::SemanticModel;

use super::load;
use crate::Database;

const fn lint(name: &'static str, summary: &'static str) -> LintMetadata {
    LintMetadata {
        name: LintName::of(name),
        summary,
        raw_documentation: summary,
        default_level: Level::Warn,
        status: LintStatus::stable(env!("CARGO_PKG_VERSION")),
        file: file!(),
        line: line!(),
    }
}

static LOAD_ERROR: LintMetadata = lint("load-error", "Reports Starlark module loading errors.");
static UNUSED_DEFINITION: LintMetadata = lint(
    "unused-definition",
    "Reports unused private and local definitions.",
);
static UNREACHABLE_CODE: LintMetadata =
    lint("unreachable-code", "Reports code that cannot be reached.");
static DEPRECATED_ARGUMENT: LintMetadata = lint(
    "deprecated-argument",
    "Reports deprecated arguments to Starlark builtins.",
);

pub(super) static INVALID_STUB_IMPLEMENTATION: LintMetadata = lint(
    "invalid-stub-implementation",
    "Reports implementations incompatible with their stubs.",
);
pub(super) static INCOMPLETE_STUB_VALIDATION: LintMetadata = lint(
    "incomplete-stub-validation",
    "Reports stub contracts that could not be proved.",
);
pub(super) static INVALID_PROVIDER_INTERFACE: LintMetadata = lint(
    "invalid-provider-interface",
    "Reports invalid or conflicting provider declarations in stubs.",
);

pub(super) fn registry() -> &'static LintRegistry {
    static REGISTRY: LazyLock<LintRegistry> = LazyLock::new(|| {
        let mut builder =
            LintRegistryBuilder::from(ty_python_semantic::default_lint_registry().clone());
        for lint in [
            &LOAD_ERROR,
            &UNUSED_DEFINITION,
            &UNREACHABLE_CODE,
            &DEPRECATED_ARGUMENT,
            &INVALID_STUB_IMPLEMENTATION,
            &INCOMPLETE_STUB_VALIDATION,
            &INVALID_PROVIDER_INTERFACE,
        ] {
            builder.register_lint(lint);
        }
        builder.build()
    });
    &REGISTRY
}

pub(super) fn rules(use_code_flow_analysis: bool) -> RuleSelection {
    let registry = registry();
    let mut rules = RuleSelection::from_registry(registry);
    // Starpls accepts suppressions without diagnosing their style or unused comments.
    for name in [
        "unused-ignore-comment",
        "unused-type-ignore-comment",
        "invalid-ignore-comment",
        "ignore-comment-unknown-rule",
        "blanket-ignore-comment",
    ] {
        rules.disable(registry.get(name).expect("Ty registers suppression lints"));
    }
    if use_code_flow_analysis {
        rules.enable(
            registry
                .get("possibly-unresolved-reference")
                .expect("Ty registers possibly unresolved references"),
            Severity::Warning,
            LintSource::Default,
        );
    }
    rules
}

pub(super) fn validation_rules() -> RuleSelection {
    let mut rules = rules(true);
    for name in [
        "unsound-return-statement",
        "unsound-assignment",
        "invalid-stub-implementation",
        "incomplete-stub-validation",
    ] {
        rules.enable(
            registry().get(name).expect("validation lint is registered"),
            Severity::Error,
            LintSource::Default,
        );
    }
    rules
}

pub(crate) fn check(db: &Database, file: File) -> Vec<Diagnostic> {
    check_with_diagnostics(db, file, Vec::new())
}

pub(super) fn check_with_diagnostics(
    db: &Database,
    file: File,
    mut diagnostics: Vec<Diagnostic>,
) -> Vec<Diagnostic> {
    let program_file = db.starlark_program_file(file);
    let parsed = ruff_db::parsed::parsed_module(db, program_file.python_file(db)).load(db);
    let options = db.environment().options(db);
    diagnostics.extend(load::diagnostics(db, program_file));
    diagnostics.extend(super::interface::diagnostics(db, file));
    if !options.allow_unused_definitions {
        let index = semantic_index(db, program_file);
        for definition in unused_definitions(db, program_file) {
            let kind = definition.kind(db);
            if !matches!(
                kind,
                DefinitionKind::Assignment(_)
                    | DefinitionKind::AnnotatedAssignment(_)
                    | DefinitionKind::AugmentedAssignment(_)
                    | DefinitionKind::For(_)
                    | DefinitionKind::Comprehension(_)
                    | DefinitionKind::Function(_)
            ) {
                continue;
            }
            let scope = definition.file_scope(db);
            let ScopedPlaceId::Symbol(symbol) = definition.place(db) else {
                continue;
            };
            let name = index.place_table(scope).symbol(symbol).name();
            let is_module = index.scope(scope).kind() == ScopeKind::Module;
            if name == "_"
                || (is_module && !name.starts_with('_'))
                || (file.is_type_interface(db) && index.scope(scope).kind() == ScopeKind::Class)
            {
                continue;
            }
            diagnostics.push(tagged(
                file,
                kind.target_range(&parsed),
                &UNUSED_DEFINITION,
                Severity::Warning,
                DiagnosticTag::Unnecessary,
                format!("\"{name}\" is not accessed"),
            ));
        }
    }
    if options.use_code_flow_analysis {
        for unreachable in unreachable_ranges(db, program_file) {
            diagnostics.push(tagged(
                file,
                unreachable.range,
                &UNREACHABLE_CODE,
                Severity::Warning,
                DiagnosticTag::Unnecessary,
                "Code is unreachable".to_owned(),
            ));
        }
    }
    let model = SemanticModel::new(db, program_file);
    let fail = db
        .language_builtin(program_file, "fail", BuiltinUsage::Runtime)
        .and_then(|binding| binding.resolve_type(db))
        .and_then(|ty| ty.definition(db, &model.program_environment()))
        .and_then(|definition| definition.definition());
    if let Some(fail) = fail {
        let mut visitor = DeprecatedArguments {
            file,
            model,
            fail,
            diagnostics: &mut diagnostics,
        };
        for statement in parsed.suite() {
            visitor.visit_stmt(statement);
        }
    }
    check_types_with_diagnostics(db, program_file, diagnostics)
}

fn tagged(
    file: File,
    range: TextRange,
    lint: &'static LintMetadata,
    severity: Severity,
    tag: DiagnosticTag,
    message: String,
) -> Diagnostic {
    let mut diagnostic = Diagnostic::new(DiagnosticId::Lint(lint.name()), severity, message);
    let mut annotation = Annotation::primary(Span::from(file.source).with_range(range));
    annotation.push_tag(tag);
    diagnostic.annotate(annotation);
    diagnostic
}

struct DeprecatedArguments<'db, 'diagnostics> {
    file: File,
    model: SemanticModel<'db>,
    fail: Definition<'db>,
    diagnostics: &'diagnostics mut Vec<Diagnostic>,
}

impl DeprecatedArguments<'_, '_> {
    fn check_call(&mut self, call: &ExprCall) {
        if !call.arguments.keywords.iter().any(|keyword| {
            keyword
                .arg
                .as_ref()
                .is_some_and(|name| matches!(name.as_str(), "msg" | "attr"))
        }) {
            return;
        }
        let Some(signature) = resolved_call_signature(&self.model, call) else {
            return;
        };
        if signature.definition != Some(self.fail) {
            return;
        }
        for (index, argument) in call.arguments.iter_source_order().enumerate() {
            let ArgOrKeyword::Keyword(keyword) = argument else {
                continue;
            };
            let Some(name) = &keyword.arg else { continue };
            if !matches!(name.as_str(), "msg" | "attr") {
                continue;
            }
            let Some(Some(parameter)) =
                signature.argument_to_displayed_parameter_mapping.get(index)
            else {
                continue;
            };
            if signature.parameters[*parameter].name != name.as_str() {
                continue;
            }
            self.diagnostics.push(tagged(
                self.file,
                name.range(),
                &DEPRECATED_ARGUMENT,
                Severity::Info,
                DiagnosticTag::Deprecated,
                format!("Argument \"{name}\" is deprecated"),
            ));
        }
    }
}

impl<'ast> Visitor<'ast> for DeprecatedArguments<'_, '_> {
    fn visit_expr(&mut self, expression: &'ast Expr) {
        if let Expr::Call(call) = expression {
            self.check_call(call);
        }
        visitor::walk_expr(self, expression);
    }
}
