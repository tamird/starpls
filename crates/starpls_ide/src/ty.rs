use ruff_db::diagnostic::Diagnostic;
use ruff_db::files::File;
use ruff_db::vendored::VendoredFileSystem;
use ruff_python_ast::name::Name;
use starpls_bazel::APIContext;
use starpls_common::Dialect;
use starpls_common::FileInfo;
use starpls_hir::Db as _;
use ty_python_core::definition::Definition;
use ty_python_core::definition::ProvidedStatement;
use ty_python_core::program::Program;
use ty_python_core::program::ProgramSettings;
use ty_python_core::ProgramFile;
use ty_python_semantic::dependency::DependencyMetadata;
use ty_python_semantic::lint::LintRegistry;
use ty_python_semantic::lint::RuleSelection;
use ty_python_semantic::provided::BuiltinUsage;
use ty_python_semantic::provided::ProvidedBindingResolution;
use ty_python_semantic::provided::ProvidedBindingValue;
use ty_python_semantic::types::KnownClass;
use ty_python_semantic::AnalysisSettings;
use ty_python_semantic::Db as _;
use ty_python_semantic::ProgramEnvironment;
use ty_site_packages::PythonVersionWithSource;

use crate::Database;

mod context;
mod diagnostics;
mod factory;
mod load;
mod native;
mod support;

pub(crate) use diagnostics::check;
pub(crate) use factory::Documentation;
pub(crate) use support::file_system;

pub(crate) struct SemanticSettings {
    program: ProgramSettings,
    rules: RuleSelection,
    rules_with_flow_diagnostics: RuleSelection,
    analysis: AnalysisSettings,
}

impl SemanticSettings {
    pub(crate) fn new(vendored: &VendoredFileSystem) -> Self {
        let mut program = ProgramSettings::empty(vendored);
        // Semantic node identities must use the same parse as starpls_common::parsed_module.
        program.python_version.version = starpls_common::PARSER_VERSION;
        Self {
            program,
            rules: diagnostics::rules(false),
            rules_with_flow_diagnostics: diagnostics::rules(true),
            analysis: AnalysisSettings::default(),
        }
    }
}

impl Database {
    pub(crate) fn set_native_metadata(
        &mut self,
        dialect: Dialect,
        builtins: &starpls_bazel::Builtins,
        rules: &starpls_bazel::Builtins,
    ) -> anyhow::Result<()> {
        use starpls_common::Db;

        let native::DeclarationSource { path, contents } =
            native::generate(dialect, builtins, rules)?;
        let changed = self.source_system_mut().set_virtual_source(&path, contents);
        if let Some(file) = self.files.try_virtual_file(&path) {
            if changed {
                file.sync(self);
            }
        } else {
            self.files.virtual_file(self, &path);
        }
        Ok(())
    }

    /// Selects semantics using the same host identity as source navigation and loads.
    pub(crate) fn starlark_program_file(&self, file: starpls_common::File) -> ProgramFile<'_> {
        let starpls_common::File {
            source,
            dialect,
            info,
        } = file;
        let dialect = match dialect {
            Dialect::Standard => "standard",
            Dialect::Bazel => "bazel",
        };
        let context = match info {
            None => "none",
            Some(FileInfo::Bazel {
                api_context,
                is_external: _,
            }) => match api_context {
                APIContext::Bzl => "bzl",
                APIContext::Build => "build",
                APIContext::Module => "module",
                APIContext::Repo => "repo",
                APIContext::Workspace => "workspace",
                APIContext::Prelude => "prelude",
                APIContext::Cquery => "cquery",
                APIContext::Vendor => "vendor",
            },
        };
        let origin = if file.is_external() == Some(true) {
            "external"
        } else {
            "local"
        };
        let namespace = Name::new(format!("starpls:{dialect}:{context}:{origin}"));
        let default = Program::from_settings(self, &self.semantic.program);
        let program = Program::with_semantic_namespace(
            self,
            default.python_platform(self),
            default.resolver_environment(self),
            &namespace,
        );
        let python_file = ruff_db::PythonFile::new_with_source_type(
            self,
            source,
            program.python_version(self),
            ruff_python_ast::PySourceType::Python,
        );
        ProgramFile::from_python_file(self, python_file, program)
    }

    pub(crate) fn starlark_file(&self, file: ProgramFile<'_>) -> Option<starpls_common::File> {
        if file.file(self).path(self).is_vendored_path() {
            return None;
        }
        let namespace = file.program(self).semantic_namespace(self).as_ref()?;
        let mut parts = namespace.as_str().split(':');
        if parts.next()? != "starpls" {
            return None;
        }
        let dialect = match parts.next()? {
            "standard" => Dialect::Standard,
            "bazel" => Dialect::Bazel,
            _ => return None,
        };
        let api_context = match parts.next()? {
            "none" => None,
            "bzl" => Some(APIContext::Bzl),
            "build" => Some(APIContext::Build),
            "module" => Some(APIContext::Module),
            "repo" => Some(APIContext::Repo),
            "workspace" => Some(APIContext::Workspace),
            "prelude" => Some(APIContext::Prelude),
            "cquery" => Some(APIContext::Cquery),
            "vendor" => Some(APIContext::Vendor),
            _ => return None,
        };
        let is_external = match parts.next()? {
            "local" => false,
            "external" => true,
            _ => return None,
        };
        if parts.next().is_some() {
            return None;
        }
        Some(starpls_common::File {
            source: file.file(self),
            dialect,
            info: api_context.map(|api_context| FileInfo::Bazel {
                api_context,
                is_external,
            }),
        })
    }
}

impl Database {
    /// Select language declarations without BUILD prelude lookup. Unattached
    /// type comments have no source declaration scope or prelude contract.
    pub(crate) fn annotation_builtin<'db>(
        &'db self,
        source: starpls_common::File,
        name: &str,
    ) -> Option<ty_python_semantic::types::Type<'db>> {
        let file = self.starlark_program_file(source);
        match self.language_builtin(file, name, BuiltinUsage::Annotation) {
            Some(binding) => binding.resolve_type(self),
            None => ty_python_semantic::SemanticModel::new(self, self.program_file(source.source))
                .builtin_type(name, BuiltinUsage::Annotation),
        }
    }

    pub(crate) fn prelude_for_file(
        &self,
        file: starpls_common::File,
    ) -> Option<starpls_common::File> {
        use starpls_hir::Db;

        if file.api_context() == Some(APIContext::Build) && file.is_external() == Some(false) {
            self.get_bazel_prelude_file()
        } else {
            None
        }
    }

    /// Runtime names come from the same finite inventory as native declarations.
    /// Types and lexical shadowing are supplied separately by Ty.
    pub(crate) fn builtin_completion_names(
        &self,
        file: starpls_common::File,
    ) -> std::collections::BTreeMap<String, bool> {
        use starpls_hir::Db;

        let mut names: std::collections::BTreeMap<_, _> = starpls_bazel::BUILTINS_VALUES_DENY_LIST
            .iter()
            .filter(|name| !matches!(**name, "True" | "False" | "None"))
            .map(|name| ((*name).to_owned(), true))
            .collect();
        if let Some(context) = file.api_context() {
            let definitions = self.get_builtin_defs(&file.dialect);
            names.extend(
                native::globals(
                    file.dialect,
                    context,
                    definitions.builtins(self),
                    definitions.rules(self),
                )
                .into_iter()
                .map(|(name, value)| (name, value.callable.is_some())),
            );
        }
        names
    }

    fn language_builtin<'db>(
        &'db self,
        file: ProgramFile<'db>,
        name: &str,
        usage: BuiltinUsage,
    ) -> Option<ProvidedBindingValue<'db>> {
        use starpls_hir::Db;

        let source_file = self.starlark_file(file)?;
        if name == "string" {
            if matches!(usage, BuiltinUsage::Runtime) {
                return Some(ProvidedBindingValue::Unresolved);
            }
            let environment = ProgramEnvironment::from_file(file);
            return Some(ProvidedBindingValue::Value(
                KnownClass::Str.to_class_literal(self, &environment),
            ));
        }
        let definitions = self.get_builtin_defs(&source_file.dialect);
        definitions.builtins(self);
        definitions.rules(self);
        if starpls_bazel::BUILTINS_VALUES_DENY_LIST.contains(&name) {
            if matches!(usage, BuiltinUsage::Runtime) {
                let native_file = self
                    .files
                    .try_virtual_file(&native::path(source_file.dialect))?;
                let declaration = ProvidedBindingValue::Export {
                    file: self.program_file(native_file.file()),
                    name: Name::new(name),
                };
                if declaration.clone().resolve_type(self).is_some() {
                    return Some(declaration);
                }
            }
            return None;
        }
        if matches!(usage, BuiltinUsage::Annotation) {
            match name {
                "unknown" => {
                    return Some(ProvidedBindingValue::Value(
                        ty_python_semantic::types::Type::unknown(),
                    ))
                }
                "NoneType" => {
                    return Some(ProvidedBindingValue::Value(
                        KnownClass::NoneType
                            .to_class_literal(self, &ProgramEnvironment::from_file(file)),
                    ))
                }
                "Unknown" => {
                    return Some(ProvidedBindingValue::Value(
                        ty_python_semantic::types::Type::unknown(),
                    ))
                }
                "Any" => {
                    return Some(ProvidedBindingValue::Value(
                        ty_python_semantic::types::Type::Dynamic(
                            ty_python_semantic::types::DynamicType::Any,
                        ),
                    ))
                }
                "Sequence" => {
                    return Some(ProvidedBindingValue::Value(
                        KnownClass::Sequence
                            .to_class_literal(self, &ProgramEnvironment::from_file(file)),
                    ))
                }
                "Iterable" => {
                    return Some(ProvidedBindingValue::Value(
                        KnownClass::Iterable
                            .to_class_literal(self, &ProgramEnvironment::from_file(file)),
                    ))
                }
                _ => {}
            }
        }
        let context = match source_file.info {
            Some(FileInfo::Bazel {
                api_context,
                is_external: _,
            }) => api_context,
            None => APIContext::Bzl,
        };
        let native_file = self
            .files
            .try_virtual_file(&native::path(source_file.dialect))?;
        let name = if matches!(usage, BuiltinUsage::Annotation) {
            Name::new(format!("_starpls_annotation_{name}"))
        } else {
            Name::new(native::export_name(context, name))
        };
        Some(ProvidedBindingValue::Export {
            file: ty_python_semantic::Db::program_file(self, native_file.file()),
            name,
        })
    }
}

#[salsa::db]
impl ty_module_resolver::Db for Database {}

#[salsa::db]
impl ty_python_core::Db for Database {
    fn should_check_file(&self, file: File) -> bool {
        !file.path(self).is_vendored_path()
    }

    fn source_exclusions(&self, file: ProgramFile<'_>) -> ty_python_core::SourceExclusions {
        let Some(file) = self.starlark_file(file) else {
            return ty_python_core::SourceExclusions::default();
        };
        let parsed = starpls_common::parsed_module(self, file).load(self);
        ty_python_core::SourceExclusions::from_statements(
            starpls_common::syntax_exclusions(self, file)
                .iter()
                .map(|node| {
                    let ruff_python_ast::AnyRootNodeRef::Stmt(statement) =
                        parsed.get_by_index(*node)
                    else {
                        unreachable!("validation excludes whole statements")
                    };
                    statement
                }),
        )
    }

    fn provided_statements(&self, file: ProgramFile<'_>) -> Vec<ProvidedStatement> {
        load::statements(self, file)
    }

    fn provided_annotation(
        &self,
        file: ProgramFile<'_>,
        owner: ruff_python_ast::NodeIndex,
    ) -> Option<ruff_text_size::TextRange> {
        let file = self.starlark_file(file)?;
        starpls_hir::Source::new(self).type_comment_annotation(file, owner)
    }
}

#[salsa::db]
impl ty_python_semantic::Db for Database {
    fn provided_parameter_type<'db>(
        &'db self,
        definition: Definition<'db>,
    ) -> Option<ty_python_semantic::types::Type<'db>> {
        context::parameter_type(self, definition)
    }

    fn provided_call_result<'db>(
        &'db self,
        call: &ty_python_semantic::types::CheckedCall<'_, 'db>,
    ) -> Option<ty_python_semantic::types::Type<'db>> {
        factory::result(self, call)
    }

    fn provided_builtin<'db>(
        &'db self,
        file: ProgramFile<'db>,
        name: &str,
        usage: BuiltinUsage,
    ) -> Option<ProvidedBindingValue<'db>> {
        let source_file = self.starlark_file(file)?;
        if let Some(prelude) = self.prelude_for_file(source_file) {
            let binding = ProvidedBindingValue::Export {
                file: self.starlark_program_file(prelude),
                name: Name::new(name),
            };
            if binding.clone().resolve_type(self).is_some() {
                return Some(binding);
            }
        }
        self.language_builtin(file, name, usage)
    }

    fn provided_binding<'db>(
        &'db self,
        definition: Definition<'db>,
    ) -> ProvidedBindingResolution<'db> {
        load::resolve(self, definition)
    }

    fn check_file(&self, file: File) -> Vec<Diagnostic> {
        if !ty_python_core::Db::should_check_file(self, file) {
            return Vec::new();
        }
        ty_python_semantic::check_file_unwrap(self, self.program_file(file))
    }

    fn program_file(&self, file: File) -> ProgramFile<'_> {
        let program = Program::from_settings(self, &self.semantic.program);
        ProgramFile::new(self, file, program)
    }

    fn python_version_with_source(&self, _file: File) -> &PythonVersionWithSource {
        &self.semantic.program.python_version
    }

    fn rule_selection(&self, _file: File) -> &RuleSelection {
        if self.environment().options(self).use_code_flow_analysis {
            &self.semantic.rules_with_flow_diagnostics
        } else {
            &self.semantic.rules
        }
    }

    fn lint_registry(&self) -> &LintRegistry {
        diagnostics::registry()
    }

    fn analysis_settings(&self, _file: File) -> &AnalysisSettings {
        &self.semantic.analysis
    }

    fn dependency_metadata(&self, _file: File) -> Option<&DependencyMetadata> {
        None
    }

    fn verbose(&self) -> bool {
        false
    }

    fn is_open_file(&self, file: File) -> bool {
        file.path(self)
            .as_system_path()
            .is_some_and(|path| self.system.document(path).is_some())
    }

    fn dyn_clone(&self) -> Box<dyn ty_python_semantic::Db> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ruff_python_ast::Stmt;
    use starpls_bazel::APIContext;
    use starpls_common::Dialect;
    use starpls_common::FileInfo;
    use ty_python_semantic::types::Type;
    use ty_python_semantic::HasType;
    use ty_python_semantic::SemanticModel;

    use crate::Analysis;

    #[test]
    fn recursive_recovery_preserves_syntax_diagnostics() {
        for diagnostics_first in [false, true] {
            let (analysis, fixture) = Analysis::from_single_file_fixture(
                "f = lambda param=: param\nfor item in []: pass\n",
            );
            let file = fixture.main_file();
            let db = &analysis.db;
            if diagnostics_first {
                let _ = starpls_hir::diagnostics_for_file(db, file).collect::<Vec<_>>();
            }
            let program_file = db.starlark_program_file(file);
            let model = SemanticModel::new(db, program_file);
            let parsed = starpls_common::parsed_module(db, file).load(db);
            let Stmt::Assign(assignment) = &parsed.syntax().body[0] else {
                panic!("expected lambda assignment");
            };
            assert!(assignment.value.inferred_type(&model).is_some());
            assert!(!starpls_common::syntax_diagnostics(db, file).is_empty());
            let diagnostics = starpls_hir::diagnostics_for_file(db, file).collect::<Vec<_>>();
            assert!(
                diagnostics.iter().any(|diagnostic| {
                    diagnostic.headline_message()
                        == "Starlark does not allow top-level for statements"
                }),
                "{diagnostics:?}"
            );
        }
    }

    #[test]
    fn unsupported_statements_do_not_change_valid_bindings() {
        let (mut analysis, _) = Analysis::new_for_test();
        let path = Path::new("/admission.bzl");
        for invalid in [
            "while True:\n    value = 'bad'",
            "class value:\n    pass",
            "value: str = 'bad'",
            "hidden = (value := 'bad')",
            "def value(arg: int) -> str:\n    return 'bad'",
            "@unknown_decorator\ndef value():\n    return 'bad'",
            "async def value():\n    return 'bad'",
            "def value[T]():\n    return 'bad'",
            "for value in ['bad']:\n    pass\nelse:\n    value = 'bad'",
            "hidden = [value async for value in []]",
            "hidden = value = 'bad'",
            "hidden = {**{'x': (value := 'bad')}}",
            "if not ...:\n    value = 'bad'",
            "def callback(arg):\n    # type: (int) -> int\n    raise NotImplementedError",
            "def nested():\n    before = 1\n    while True:\n        before = 'bad'\n    after = before\n    return after",
        ] {
            for source in [
                "value = 1\nobserved = value\n".to_owned(),
                format!("value = 1\n{invalid}\nobserved = value\n"),
                "value = 1\nobserved = value\n".to_owned(),
            ] {
                let file = analysis
                    .open_document(path, Dialect::Bazel, None, source.clone(), 0)
                    .unwrap();
                let snapshot = analysis.snapshot();
                let db = &snapshot.db;
                let file = db.starlark_program_file(file);
                let model = SemanticModel::new(db, file);
                let parsed = ruff_db::parsed::parsed_module(db, file.python_file(db)).load(db);
                let Stmt::Assign(last) = parsed.suite().last().unwrap() else {
                    unreachable!()
                };
                let observed = last.value.inferred_type(&model).unwrap();
                assert_eq!(
                    observed
                        .display(db, &model.program_environment())
                        .to_string(),
                    "Literal[1]",
                    "{source}"
                );
                let names = model
                    .lexical_completions(ty_python_core::FileScopeId::global())
                    .map(|item| item.name.to_string())
                    .collect::<Vec<_>>();
                assert!(
                    !names.iter().any(|name| name == "hidden"),
                    "{source}: {names:?}"
                );
                // Query full diagnostics after a type-first request on the same revision.
                let _ = super::check(db, db.starlark_file(file).unwrap());
            }
        }
    }

    #[test]
    fn infer_real_starlark_source_with_ty() {
        let (mut analysis, _) = Analysis::new_for_test();
        let path = Path::new("/main.bzl");
        for (version, source, expected) in [
            (
                1,
                r#"numbers = [1, 2]
def twice(value):
    # type: (int) -> int
    return value + value
answer = twice(numbers[0])
"#,
                ["list[int]", "int"],
            ),
            (
                2,
                r#"numbers = ['one', 'two']
def twice(value):
    # type: (string) -> string
    return value + value
answer = twice(numbers[0])
"#,
                ["list[str]", "str"],
            ),
        ] {
            let file = analysis
                .open_document(path, Dialect::Bazel, None, source.to_string(), version)
                .unwrap();
            let snapshot = analysis.snapshot();
            let db = &snapshot.db;
            let program_file = db.starlark_program_file(file);
            let module = ruff_db::parsed::parsed_module(db, program_file.python_file(db)).load(db);
            let model = SemanticModel::new(db, program_file);
            let mut types = Vec::new();
            for statement in module.suite() {
                match statement {
                    Stmt::Assign(assignment) => {
                        let ty = assignment.value.inferred_type(&model).unwrap();
                        types.push(ty.display(db, &model.program_environment()).to_string());
                    }
                    Stmt::FunctionDef(function) => {
                        assert!(matches!(
                            function.inferred_type(&model),
                            Some(Type::FunctionLiteral(_))
                        ));
                    }
                    _ => panic!("unexpected fixture statement"),
                }
            }
            assert_eq!(types, expected);
            assert!(ty_python_semantic::check_file_unwrap(db, program_file).is_empty());
            assert_eq!(file.contents(db).as_str(), source);
        }
    }

    #[test]
    fn contextual_collections_use_ty_constraints() {
        let (mut analysis, _) = Analysis::new_for_test();
        let source = r#"
def consume(values):
    # type: (list[int | None]) -> None
    pass

consume([1])
existing = [1]
consume(existing)
"#;
        let file = analysis
            .open_document(
                Path::new("/context.bzl"),
                Dialect::Bazel,
                None,
                source.to_owned(),
                1,
            )
            .unwrap();
        let snapshot = analysis.snapshot();
        let db = &snapshot.db;
        let program_file = db.starlark_program_file(file);
        let diagnostics = ty_python_semantic::check_file_unwrap(db, program_file);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].id().to_string(), "invalid-argument-type");
        assert!(
            diagnostics[0]
                .concise_message()
                .to_string()
                .contains("list[int]"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn comments_check_defaults_parameter_uses_and_returns() {
        let (mut analysis, _) = Analysis::new_for_test();
        let source = r#"
def text(value):
    # type: (string) -> None
    pass

def checked(value="bad"):
    # type: (int) -> int
    text(value)
    return "bad"
"#;
        let file = analysis
            .open_document(
                Path::new("/comments.bzl"),
                Dialect::Bazel,
                None,
                source.to_owned(),
                1,
            )
            .unwrap();
        let snapshot = analysis.snapshot();
        let db = &snapshot.db;
        let file = db.starlark_program_file(file);
        let diagnostics = ty_python_semantic::check_file_unwrap(db, file);
        let mut ids: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.id().to_string())
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            [
                "invalid-argument-type",
                "invalid-parameter-default",
                "invalid-return-type"
            ],
            "{diagnostics:?}"
        );
    }

    #[test]
    fn program_identity_preserves_host_context() {
        let (mut analysis, _) = Analysis::new_for_test();
        let file = analysis
            .open_document(
                Path::new("/shared.bzl"),
                Dialect::Bazel,
                None,
                "value = 1".to_owned(),
                1,
            )
            .unwrap();
        let snapshot = analysis.snapshot();
        let db = &snapshot.db;
        let default = db.starlark_program_file(file);
        assert!(std::ptr::eq(
            starpls_common::parsed_module(db, file),
            ruff_db::parsed::parsed_module(db, default.python_file(db)),
        ));
        for context in [APIContext::Bzl, APIContext::Build, APIContext::Module] {
            for is_external in [false, true] {
                let mut contextual_file = file;
                contextual_file.info = Some(FileInfo::Bazel {
                    api_context: context,
                    is_external,
                });
                let program_file = db.starlark_program_file(contextual_file);
                assert_ne!(default, program_file);
                assert_eq!(default.python_file(db), program_file.python_file(db));
                assert_eq!(db.starlark_file(program_file), Some(contextual_file));
            }
        }
    }
}
