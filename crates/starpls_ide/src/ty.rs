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
use ty_python_semantic::types::Type;
use ty_python_semantic::AnalysisSettings;
use ty_python_semantic::Db as _;
use ty_python_semantic::ProgramEnvironment;
use ty_site_packages::PythonVersionWithSource;

use crate::Database;

mod context;
mod diagnostics;
mod factory;
pub(crate) mod interface;
pub(crate) mod load;
mod native;
#[cfg(test)]
mod skylib_tests;
mod support;
pub(crate) mod validation;

pub(crate) use diagnostics::check;
pub(crate) use factory::Documentation;
pub(crate) use support::file_system;

pub(crate) struct SemanticSettings {
    program: ProgramSettings,
    rules: RuleSelection,
    rules_with_flow_diagnostics: RuleSelection,
    validation_rules: RuleSelection,
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
            validation_rules: diagnostics::validation_rules(),
            analysis: AnalysisSettings::default(),
        }
    }
}

impl Database {
    pub(crate) fn set_native_metadata(
        &mut self,
        dialect: Dialect,
        builtins: &starpls_bazel::Builtins,
        rules: &starpls_bazel::build::BuildLanguage,
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
        let kind = if file.is_type_interface(self) {
            ty_python_core::ProgramFileKind::Stub
        } else {
            ty_python_core::ProgramFileKind::Source
        };
        ProgramFile::from_python_file_with_kind(self, python_file, program, kind)
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

    /// Names come from the same inventory and usage policy as native declarations.
    /// Lexical bindings and their shadowing are supplied separately by Ty.
    pub(crate) fn builtin_completion_names(
        &self,
        file: starpls_common::File,
        usage: BuiltinUsage,
    ) -> std::collections::BTreeMap<String, bool> {
        use starpls_hir::Db;

        if matches!(usage, BuiltinUsage::Annotation) {
            let definitions = self.get_builtin_defs(&file.dialect);
            let candidates = definitions
                .builtins(self)
                .r#type
                .iter()
                .map(|class| class.name.as_str())
                .chain(starpls_bazel::BUILTINS_VALUES_DENY_LIST.iter().copied())
                .chain([
                    "str", "string", "Any", "Unknown", "unknown", "NoneType", "Sequence",
                    "Iterable", "Final", "Callable", "Protocol",
                ]);
            return candidates
                .filter(|name| {
                    matches!(
                        self.annotation_builtin(file, name),
                        Some(
                            ty_python_semantic::types::Type::ClassLiteral(_)
                                | ty_python_semantic::types::Type::GenericAlias(_)
                                | ty_python_semantic::types::Type::Dynamic(_)
                                | ty_python_semantic::types::Type::SpecialForm(_)
                        )
                    )
                })
                .map(|name| (name.to_owned(), false))
                .collect();
        }

        let mut names: std::collections::BTreeMap<_, _> = starpls_bazel::BUILTINS_VALUES_DENY_LIST
            .iter()
            .filter(|name| !matches!(**name, "True" | "False" | "None"))
            .map(|name| ((*name).to_owned(), true))
            .collect();
        if file.is_type_interface(self) {
            names.insert("Protocol".to_owned(), false);
        }
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
        if name == "Protocol" && !source_file.is_type_interface(self) {
            return Some(ProvidedBindingValue::Unresolved);
        }
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
        let name = if matches!(usage, BuiltinUsage::Annotation)
            || (source_file.is_type_interface(self) && name == "Protocol")
        {
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

    fn provided_annotation<'db>(
        &'db self,
        file: ProgramFile<'db>,
        owner: ruff_python_ast::NodeIndex,
    ) -> Option<ty_python_core::ProvidedAnnotation<'db>> {
        let file = self.starlark_file(file)?;
        starpls_hir::Source::new(self)
            .type_comment_annotation(file, owner)
            .map(ty_python_core::ProvidedAnnotation::Range)
            .or_else(|| {
                let &(target, owner) = self
                    .environment()
                    .stub_validation(self)
                    .annotations
                    .get(&(file.source, owner))?;
                Some(ty_python_core::ProvidedAnnotation::External {
                    file: self.starlark_program_file(target),
                    owner,
                })
            })
    }
}

#[salsa::db]
impl ty_python_semantic::Db for Database {
    fn provided_return_type<'db>(
        &'db self,
        definition: Definition<'db>,
    ) -> Option<ty_python_semantic::provided::ProvidedReturnType<'db>> {
        validation::provider_return_type(self, definition)
    }

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

    fn provided_type_test<'db>(
        &'db self,
        file: ProgramFile<'db>,
        callable: Type<'db>,
        compared_value: Type<'db>,
    ) -> Option<Type<'db>> {
        let tag = compared_value.string_literal_value(self)?;
        if !matches!(callable, Type::FunctionLiteral(_)) {
            return None;
        }
        let binding = self.language_builtin(file, "type", BuiltinUsage::Runtime)?;
        let native_type = binding.resolve_type(self)?;
        if callable != native_type {
            return None;
        }
        let environment = ProgramEnvironment::from_file(file);
        let unknown = Type::unknown();
        // Runtime tags differ from annotation names, and provider names need
        // not identify a unique type. Only canonical core tags are exhaustive.
        let ty = match tag {
            "bool" => KnownClass::Bool.to_instance(self, &environment),
            "int" => KnownClass::Int.to_instance(self, &environment),
            "float" => KnownClass::Float.to_instance(self, &environment),
            "string" => KnownClass::Str.to_instance(self, &environment),
            "NoneType" => Type::none(self, &environment),
            "range" => KnownClass::Range.to_instance(self, &environment),
            "list" => KnownClass::List.to_specialized_instance(self, &environment, &[unknown]),
            "dict" => {
                KnownClass::Dict.to_specialized_instance(self, &environment, &[unknown, unknown])
            }
            "set" => KnownClass::Set.to_specialized_instance(self, &environment, &[unknown]),
            "tuple" => Type::homogeneous_tuple(self, &environment, unknown),
            _ => return None,
        };
        Some(ty)
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

    fn rule_selection(&self, file: File) -> &RuleSelection {
        if self
            .environment()
            .stub_validation(self)
            .files
            .contains(&file)
        {
            &self.semantic.validation_rules
        } else if self.environment().options(self).use_code_flow_analysis {
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
        file.path(self).as_system_path().is_some_and(|path| {
            self.system
                .document(path)
                .is_some_and(|document| document.path.as_path() == path)
        })
    }

    fn dyn_clone(&self) -> Box<dyn ty_python_semantic::Db> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ruff_python_ast::Stmt;
    use starpls_bazel::build::attribute::Discriminator;
    use starpls_bazel::build::AttributeDefinition;
    use starpls_bazel::build::BuildLanguage;
    use starpls_bazel::build::RuleDefinition;
    use starpls_bazel::APIContext;
    use starpls_common::Dialect;
    use starpls_common::FileInfo;
    use ty_python_semantic::types::Type;
    use ty_python_semantic::HasType;
    use ty_python_semantic::SemanticModel;

    use crate::Analysis;

    #[test]
    fn dictionary_constructor_keys_keep_argument_types() {
        let source = r#"def consume(tags: list[str], testonly: bool):
    pass
common = dict(tags=["manual"], testonly=True)
consume(**common)
consume(tags=common["tags"], testonly=common["testonly"])
common["tags"] = ["local"]
consume(**common)
invalid = dict(tags=[1], testonly=True)
consume(**invalid)
"#;
        let (analysis, fixture) = Analysis::from_single_file_fixture(source);
        let diagnostics = analysis
            .snapshot()
            .diagnostics(fixture.main_file())
            .unwrap();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].id().as_str(), "invalid-argument-type");
        assert!(
            usize::from(diagnostics[0].range().unwrap().start())
                >= source.find("consume(**invalid)").unwrap(),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn native_keyword_dictionary_completeness_follows_edits() {
        let (mut analysis, fixture) = Analysis::from_single_file_fixture("");
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                BuildLanguage {
                    rule: vec![RuleDefinition {
                        name: "alias".to_owned(),
                        attribute: [
                            ("name", Discriminator::String, true, false),
                            ("actual", Discriminator::Label, true, true),
                            ("tags", Discriminator::StringList, false, false),
                            ("testonly", Discriminator::Boolean, false, false),
                            ("deprecation", Discriminator::String, false, false),
                        ]
                        .into_iter()
                        .map(
                            |(name, kind, mandatory, configurable)| AttributeDefinition {
                                name: name.to_owned(),
                                r#type: kind as i32,
                                mandatory: Some(mandatory),
                                configurable: Some(configurable),
                                ..Default::default()
                            },
                        )
                        .collect(),
                        ..Default::default()
                    }],
                },
            )
            .unwrap();
        let file = fixture.main_file();
        for (extra_keyword, escape, valid) in [
            ("", "", true),
            ("", "    mutate(options)\n", false),
            ("", "", true),
            (", deprecation=1", "", false),
        ] {
            let source = format!(
                r#"def mutate(value):
    value["deprecation"] = 1

def register(name: str):
    options = dict(tags=["manual"], testonly=True{extra_keyword})
{escape}    native.alias(name=name, actual="//:target", **options)
    native.alias(name=name + "_again", actual="//:target", **options)
"#
            );
            analysis.update_file(file, source.clone());
            let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
            assert_eq!(diagnostics.is_empty(), valid, "{source}\n{diagnostics:?}");
            let first_call = source.find("native.alias(").unwrap();
            for diagnostic in diagnostics {
                assert_eq!(diagnostic.id().as_str(), "invalid-argument-type");
                assert!(
                    usize::from(diagnostic.range().unwrap().start()) >= first_call,
                    "{source}\n{diagnostic:?}"
                );
            }
        }
    }

    #[test]
    fn native_type_tests_preserve_collection_arguments() {
        let (mut analysis, fixture) = Analysis::from_single_file_fixture("");
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                Default::default(),
            )
            .unwrap();
        let file = fixture.main_file();
        for (annotation, tag, matched, excluded) in [
            ("bool | int", "bool", "bool", "int"),
            ("bool | int", "int", "int", "bool"),
            ("str | Target", "string", "str", "Target"),
            ("int | Target", "int", "int", "Target"),
            ("float | None", "float", "float*", "int | None"),
            ("float | int", "float", "float*", "int"),
            ("str | None", "NoneType", "None", "str"),
            ("range | Target", "range", "range", "Target"),
            ("list[Target] | Target", "list", "list[Target]", "Target"),
            (
                "dict[str, Target] | Target",
                "dict",
                "dict[str, Target]",
                "Target",
            ),
            (
                "tuple[int, str] | Target",
                "tuple",
                "tuple[int, str]",
                "Target",
            ),
            ("set[str] | Target", "set", "set[str]", "Target"),
            ("str | int", "str", "str | int", "str | int"),
            ("str | int", "unknown", "str | int", "str | int"),
        ] {
            let source = format!("def probe(value: {annotation}):\n    if type(value) == \"{tag}\":\n        return value # matched\n    else:\n        return value # excluded\n");
            analysis.update_file(file, source.clone());
            let snapshot = analysis.snapshot();
            for (marker, expected) in [("value # matched", matched), ("value # excluded", excluded)]
            {
                let hover = snapshot
                    .hover(crate::FilePosition {
                        file_id: file,
                        pos: (source.find(marker).unwrap() as u32).into(),
                    })
                    .unwrap()
                    .unwrap();
                let expected =
                    expected.replace("Target", "Target[FilesToRunProvider[File | None] | None]");
                assert!(
                    hover.contents.value.contains(&format!(": {expected}\n")),
                    "{tag}, {marker}: {}",
                    hover.contents.value
                );
            }
            let diagnostics = snapshot.diagnostics(file).unwrap();
            assert!(diagnostics.is_empty(), "{tag}: {diagnostics:?}");
        }
    }

    #[test]
    fn native_type_test_identity_follows_edits() {
        let (mut analysis, fixture) = Analysis::from_single_file_fixture("");
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                Default::default(),
            )
            .unwrap();
        let file = fixture.main_file();
        for (declarations, call, expected) in [
            ("classify = type", "classify", "list[Target]"),
            (
                "def classify(value): return \"list\"",
                "classify",
                "list[Target] | Target",
            ),
            ("classify = type", "classify", "list[Target]"),
            (
                "def type(value): return \"list\"",
                "type",
                "list[Target] | Target",
            ),
        ] {
            let source = format!("{declarations}\ndef probe(value: list[Target] | Target):\n    if {call}(value) == \"list\":\n        return value # matched\n    return value\n");
            analysis.update_file(file, source.clone());
            let hover = analysis
                .snapshot()
                .hover(crate::FilePosition {
                    file_id: file,
                    pos: (source.find("value # matched").unwrap() as u32).into(),
                })
                .unwrap()
                .unwrap();
            let expected =
                expected.replace("Target", "Target[FilesToRunProvider[File | None] | None]");
            assert!(
                hover.contents.value.contains(&format!(": {expected}\n")),
                "{declarations}: {}",
                hover.contents.value
            );
        }
        let source = r#"def targets(value: list[Target] | Target) -> list[Target]:
    if type(value) == type([]):
        return value
    return [value]

def incorrect(value: list[Target] | Target) -> list[str]:
    if type(value) == "list":
        return value
    return []
"#;
        analysis.update_file(file, source.to_owned());
        let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].id().as_str(), "invalid-return-type");
    }

    #[test]
    fn native_type_tests_ignore_build_prelude_shadows() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        fixture.add_file(
            &mut analysis.db,
            "defs.bzl",
            r#"def make(condition) -> list[int] | str:
    return [1] if condition else "text"
def type(value):
    return "list"
"#,
        );
        fixture.add_prelude_file(&mut analysis.db, "load(\"defs.bzl\", \"make\", \"type\")\n");
        let source = "value = make(True)\nresult = value if type(value) == \"list\" else None\n";
        let file = fixture.add_file_with_options(
            &mut analysis.db,
            "BUILD",
            source,
            Dialect::Bazel,
            Some(FileInfo::Bazel {
                api_context: APIContext::Build,
                is_external: false,
            }),
        );
        loader.add_files_from_fixture(&fixture);
        let snapshot = analysis.snapshot();
        let hover = snapshot
            .hover(crate::FilePosition {
                file_id: file,
                pos: (source.find("value if").unwrap() as u32).into(),
            })
            .unwrap()
            .unwrap();
        assert!(
            hover.contents.value.contains(": list[int] | str\n"),
            "{}",
            hover.contents.value
        );
        let diagnostics = snapshot.diagnostics(file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn native_annotations_drive_editor_requests_across_import_edits() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        let providers = fixture.add_file(
            &mut analysis.db,
            "//:providers.bzl",
            r#"Info = provider(fields=["value"])
"#,
        );
        let original = r#"load("//:providers.bzl", Label="Info")
def identity(value: Label) -> Label:
    return value
"#;
        let definition = fixture.add_file(&mut analysis.db, "//:defs.bzl", original);
        let caller_source = r#"load("//:defs.bzl", "identity")
load("//:providers.bzl", "Info")
identity(value=Info(value=1))
"#;
        let caller = fixture.add_file_with_options(
            &mut analysis.db,
            "BUILD.bazel",
            caller_source,
            Dialect::Bazel,
            Some(FileInfo::Bazel {
                api_context: APIContext::Build,
                is_external: false,
            }),
        );
        loader.add_files_from_fixture(&fixture);
        analysis
            .set_builtin_defs(
                starpls_bazel::decode_builtins(include_bytes!(
                    "../../starpls/src/builtin/builtin.pb"
                ))
                .unwrap(),
                Default::default(),
            )
            .unwrap();
        for (source, expected) in [
            (original.to_owned(), "Info"),
            (
                original
                    .replace(": Label", ": int")
                    .replace("-> Label", "-> int"),
                "int",
            ),
            (original.to_owned(), "Info"),
        ] {
            analysis.update_file(definition, source.clone());
            let snapshot = analysis.snapshot();
            let position = crate::FilePosition {
                file_id: caller,
                pos: (caller_source.rfind("value=Info").unwrap() as u32).into(),
            };
            let hover = snapshot.hover(position.clone()).unwrap().unwrap();
            assert!(
                hover.contents.value.contains(&format!("value: {expected}")),
                "{}",
                hover.contents.value
            );
            let help = snapshot.signature_help(position.clone()).unwrap().unwrap();
            let [signature] = help.signatures.as_slice() else {
                panic!("expected one signature: {help:?}");
            };
            assert!(
                signature.label.contains(&format!("value: {expected}")),
                "{signature:?}"
            );
            assert!(
                signature.label.ends_with(&format!("-> {expected}")),
                "{signature:?}"
            );
            let locations = snapshot
                .goto_definition(position.clone(), true)
                .unwrap()
                .unwrap();
            let [crate::LocationLink::Local {
                origin_selection_range: _,
                target_range: _,
                target_file_id,
                target_selection_range,
            }] = locations.as_slice()
            else {
                panic!("expected one parameter definition: {locations:?}");
            };
            assert_eq!(*target_file_id, definition.source);
            assert_eq!(&source[*target_selection_range], "value");
            let diagnostics = snapshot.diagnostics(caller).unwrap();
            assert_eq!(
                diagnostics.is_empty(),
                expected == "Info",
                "{diagnostics:?}"
            );
            if expected == "Info" {
                let position = crate::FilePosition {
                    file_id: definition,
                    pos: (source.find(": Label").unwrap() as u32 + 3).into(),
                };
                let locations = snapshot
                    .goto_definition(position.clone(), true)
                    .unwrap()
                    .unwrap();
                let [crate::LocationLink::Local {
                    origin_selection_range: _,
                    target_range: _,
                    target_file_id,
                    target_selection_range: _,
                }] = locations.as_slice()
                else {
                    panic!("expected the loaded provider declaration: {locations:?}");
                };
                assert_eq!(*target_file_id, providers.source);
                let hover = snapshot.hover(position.clone()).unwrap().unwrap();
                assert!(
                    hover.contents.value.contains("Info"),
                    "{}",
                    hover.contents.value
                );
            }
        }
    }

    #[test]
    fn interface_declarations_use_the_canonical_stub_file() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        let file = fixture.add_file(
            &mut analysis.db,
            "api.bzli",
            "value: int\ndef compute(value: int = ...) -> string: ...\n",
        );
        loader.add_files_from_fixture(&fixture);
        let snapshot = analysis.snapshot();
        let db = &snapshot.db;
        let program_file = db.starlark_program_file(file);
        assert!(program_file.is_stub(db));
        let parsed = starpls_common::parsed_module(db, file);
        assert!(std::ptr::eq(
            parsed,
            ruff_db::parsed::parsed_module(db, program_file.python_file(db)),
        ));
        let diagnostics = snapshot.diagnostics(file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        drop(snapshot);
        analysis.update_file(file, "def compute(): return 1\n".to_owned());
        let caller = fixture.add_file(
            &mut analysis.db,
            "main.bzl",
            "load(\"api.bzli\", \"compute\")\ncompute()\n",
        );
        loader.add_files_from_fixture(&fixture);
        let snapshot = analysis.snapshot();
        let diagnostics = snapshot.diagnostics(file).unwrap();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.id().as_str() == "invalid-syntax"),
            "{diagnostics:?}"
        );
        let diagnostics = snapshot.diagnostics(caller).unwrap();
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.id().as_str() == "unresolved-import"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn native_assignment_exports_keep_editor_identity_after_edits() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = starpls_hir::Fixture::new(&mut analysis.db);
        let original = "values: list[int] = [] # type: list[string]\nobserved = values\n";
        let declaration = fixture.add_file(&mut analysis.db, "//:defs.bzl", original);
        let caller = fixture.add_file_with_options(
            &mut analysis.db,
            "BUILD.bazel",
            "load(\"//:defs.bzl\", \"values\")\nvalues.append('bad')\n",
            Dialect::Bazel,
            Some(FileInfo::Bazel {
                api_context: APIContext::Build,
                is_external: false,
            }),
        );
        loader.add_files_from_fixture(&fixture);
        for (source, expected) in [
            (original.to_owned(), "list[int]"),
            (
                format!(
                    "# moved declaration\n{}",
                    original.replace("list[int]", "list[string]")
                ),
                "list[str]",
            ),
            (original.to_owned(), "list[int]"),
        ] {
            analysis.update_file(declaration, source.clone());
            let snapshot = analysis.snapshot();
            let position = crate::FilePosition {
                file_id: declaration,
                pos: u32::try_from(source.rfind("values").unwrap())
                    .unwrap()
                    .into(),
            };
            let hover = snapshot.hover(position.clone()).unwrap().unwrap();
            assert!(
                hover.contents.value.contains(expected),
                "{}",
                hover.contents.value
            );
            let locations = snapshot
                .goto_definition(position.clone(), true)
                .unwrap()
                .unwrap();
            let [crate::LocationLink::Local {
                origin_selection_range: _,
                target_range: _,
                target_file_id,
                target_selection_range,
            }] = locations.as_slice()
            else {
                panic!("expected annotated declaration: {locations:?}");
            };
            assert_eq!(*target_file_id, declaration.source);
            assert_eq!(&source[*target_selection_range], "values");
            assert_eq!(
                usize::from(target_selection_range.start()),
                source.find("values:").unwrap()
            );
            let references = snapshot.find_references(position, true).unwrap().unwrap();
            assert_eq!(references.len(), 2, "{references:?}");
            assert!(
                references
                    .iter()
                    .all(|location| location.file_id == declaration
                        && &source[location.range] == "values"),
                "{references:?}"
            );
            let symbols = snapshot.document_symbols(declaration).unwrap().unwrap();
            let [values, observed] = symbols.as_slice() else {
                panic!("expected both global declarations: {symbols:?}");
            };
            assert_eq!((&*values.name, &*observed.name), ("values", "observed"));
            assert_eq!(values.selection_range, *target_selection_range);
            let diagnostics = snapshot.diagnostics(declaration).unwrap();
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
            let diagnostics = snapshot.diagnostics(caller).unwrap();
            assert_eq!(
                diagnostics.is_empty(),
                expected == "list[str]",
                "{diagnostics:?}"
            );
            if expected == "list[int]" {
                assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
                assert_eq!(diagnostics[0].id().as_str(), "invalid-argument-type");
            }
        }
    }

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
            "value: str",
            "hidden = (value := 'bad')",
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
