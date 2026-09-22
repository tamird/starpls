//! Trusted client contracts use ordinary stub exports; implementations remain separate files.

use std::collections::hash_map::Entry;

use ruff_db::Db as _;
use rustc_hash::FxHashMap;
use salsa::Setter;
use starpls_common::File;
use starpls_hir::Db as _;
use ty_python_core::definition::Definition;
use ty_python_core::definition::DefinitionKind;
use ty_python_core::global_scope;
use ty_python_core::place_table;
use ty_python_core::use_def_map;
use ty_python_core::ProgramFile;

use crate::Analysis;
use crate::Database;

impl Analysis {
    /// Replace the explicit trusted contracts atomically after validating every mapping.
    pub fn set_type_interfaces(
        &mut self,
        interfaces: impl IntoIterator<Item = (File, File)>,
    ) -> anyhow::Result<()> {
        let Self { db } = self;
        let mut mappings = FxHashMap::default();
        for (source, interface) in interfaces {
            if !source.allows_native_annotations(db)
                || source
                    .path(db)
                    .extension()
                    .is_none_or(|extension| extension != "bzl")
            {
                anyhow::bail!(
                    "type interface source must be a .bzl file: {}",
                    source.path(db).display()
                );
            }
            if !interface.is_type_interface(db) {
                anyhow::bail!(
                    "type interface must be a .bzli file: {}",
                    interface.path(db).display()
                );
            }
            match mappings.entry(source.source) {
                Entry::Vacant(entry) => {
                    entry.insert((source, interface));
                }
                Entry::Occupied(entry) => {
                    let (source, previous): &(File, File) = entry.get();
                    anyhow::bail!(
                        "duplicate type interface for {}: {} and {}",
                        source.path(db).display(),
                        previous.path(db).display(),
                        interface.path(db).display()
                    );
                }
            }
        }
        db.environment().set_type_interfaces(db).to(mappings);
        Ok(())
    }

    /// Physical inputs whose declarations may be referenced by trusted interfaces.
    pub fn type_interface_sources(&self) -> Vec<File> {
        let Self { db } = self;
        db.environment()
            .type_interfaces(db)
            .values()
            .map(|(source, _)| *source)
            .collect()
    }

    pub fn type_interface_files(&self) -> Vec<File> {
        let Self { db } = self;
        let mut files: Vec<_> = db
            .environment()
            .type_interfaces(db)
            .values()
            .map(|(_, interface)| *interface)
            .collect();
        files.sort_by(|left, right| left.path(db).cmp(right.path(db)));
        files.dedup();
        files
    }
}

impl Database {
    pub(crate) fn type_interface(&self, from: File, source: File) -> Option<File> {
        let mappings = self.environment().type_interfaces(self);
        if mappings.is_empty() {
            return None;
        }
        let (_, interface) = mappings.get(&source.source).or_else(|| {
            let path = starpls_common::system_path(source.path(self)).ok()?;
            let canonical = self.system().canonicalize_path(path).ok()?;
            let source = ruff_db::files::system_path_to_file(self, &canonical).ok()?;
            mappings.get(&source)
        })?;
        // An interface may import the implementation's existing nominal providers.
        // It must not resolve that import back to its own declaration of the name.
        (interface.source != from.source).then_some(*interface)
    }

    pub(crate) fn load_export_file<'db>(
        &'db self,
        from: File,
        source: File,
        name: &str,
    ) -> ProgramFile<'db> {
        if let Some(interface) = self.type_interface(from, source) {
            let file = self.starlark_program_file(interface);
            if !export_definitions(self, file, name).is_empty() {
                return file;
            }
        }
        self.starlark_program_file(source)
    }

    /// Navigation may find an implementation even when its signature is incompatible.
    /// This lookup never participates in selecting or inferring the trusted contract.
    pub(crate) fn interface_implementation<'db>(
        &'db self,
        definition: Definition<'db>,
    ) -> Vec<Definition<'db>> {
        let Some((from, module, name)) = super::load::binding_names(self, definition) else {
            return Vec::new();
        };
        let Ok(Some(source)) = starpls_common::Db::load_file(self, &module, from.dialect, from)
        else {
            return Vec::new();
        };
        let source_file = self.starlark_program_file(source);
        if self.load_export_file(from, source, &name) == source_file {
            return Vec::new();
        }
        export_definitions(self, source_file, &name)
    }
}

pub(crate) fn export_definitions<'db>(
    db: &'db Database,
    file: ProgramFile<'db>,
    name: &str,
) -> Vec<Definition<'db>> {
    let scope = global_scope(db, file);
    let Some(symbol) = place_table(db, scope).symbol_id(name) else {
        return Vec::new();
    };
    use_def_map(db, scope)
        .end_of_scope_symbol_bindings(symbol)
        .filter_map(|binding| binding.binding.definition())
        .filter(|definition| !matches!(definition.kind(db), DefinitionKind::ProvidedBinding(_)))
        .collect()
}

#[cfg(test)]
mod tests {
    use starpls_bazel::APIContext;
    use starpls_common::Dialect;
    use starpls_common::FileInfo;
    use starpls_hir::Fixture;

    use crate::Analysis;
    use crate::FilePosition;
    use crate::LocationLink;

    #[test]
    fn trusted_exports_are_partial_and_independent_of_the_implementation() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let source_text = "def compute(legacy: int) -> int:\n    return 'bad'\nuntouched = 42\n";
        let source = fixture.add_file(&mut analysis.db, "source.bzl", source_text);
        let interface = fixture.add_file(&mut analysis.db, "source.bzli", "");
        let caller_text = "load(\"source.bzl\", \"compute\", \"untouched\", \"only\")\nresult = compute(value='ok')\noriginal = untouched\n";
        let caller = fixture.add_file_with_options(
            &mut analysis.db,
            "BUILD",
            caller_text,
            Dialect::Bazel,
            Some(FileInfo::Bazel {
                api_context: APIContext::Build,
                is_external: false,
            }),
        );
        loader.add_files_from_fixture(&fixture);
        analysis.set_type_interfaces([(source, interface)]).unwrap();
        for (annotation, valid) in [("string", true), ("int", false), ("string", true)] {
            analysis.update_file(
                interface,
                format!("def compute(value: {annotation}) -> {annotation}: ...\nonly: int\n"),
            );
            let snapshot = analysis.snapshot();
            let completions = snapshot
                .completions(
                    FilePosition {
                        file_id: caller,
                        pos: (caller_text.find("only").unwrap() as u32).into(),
                    },
                    None,
                )
                .unwrap()
                .unwrap();
            for name in ["compute", "only", "untouched"] {
                assert_eq!(
                    completions.iter().filter(|item| item.label == name).count(),
                    1,
                    "{name}"
                );
            }
            let diagnostics = snapshot.diagnostics(caller).unwrap();
            assert_eq!(diagnostics.is_empty(), valid, "{diagnostics:?}");
            if !valid {
                let [diagnostic] = diagnostics.as_slice() else {
                    panic!("{diagnostics:?}")
                };
                assert_eq!(diagnostic.id().as_str(), "invalid-argument-type");
            }
            let diagnostics = snapshot.diagnostics(source).unwrap();
            assert!(
                diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.id().as_str() == "invalid-return-type"),
                "{diagnostics:?}"
            );
            let hover = snapshot
                .hover(FilePosition {
                    file_id: caller,
                    pos: (caller_text.rfind("untouched").unwrap() as u32).into(),
                })
                .unwrap()
                .unwrap();
            assert!(
                hover.contents.value.contains("Literal[42]"),
                "{}",
                hover.contents.value
            );
            let position = FilePosition {
                file_id: caller,
                pos: (caller_text.rfind("compute").unwrap() as u32).into(),
            };
            let locations = snapshot.goto_definition(position, false).unwrap().unwrap();
            let [LocationLink::Local {
                target_file_id,
                origin_selection_range: _,
                target_range: _,
                target_selection_range: _,
            }] = locations.as_slice()
            else {
                panic!("{locations:?}")
            };
            assert_eq!(*target_file_id, source.source);
            let position = FilePosition {
                file_id: caller,
                pos: (caller_text.find("only").unwrap() as u32).into(),
            };
            let locations = snapshot.goto_definition(position, false).unwrap().unwrap();
            let [LocationLink::Local {
                target_file_id,
                origin_selection_range: _,
                target_range: _,
                target_selection_range: _,
            }] = locations.as_slice()
            else {
                panic!("{locations:?}")
            };
            assert_eq!(*target_file_id, interface.source);
            let help = snapshot
                .signature_help(FilePosition {
                    file_id: caller,
                    pos: (caller_text.find("'ok'").unwrap() as u32).into(),
                })
                .unwrap()
                .unwrap();
            let [signature] = help.signatures.as_slice() else {
                panic!("{help:?}")
            };
            let expected = if valid { "str" } else { "int" };
            assert_eq!(
                signature.label,
                format!("def compute(value: {expected}) -> {expected}")
            );
        }
        analysis.update_file(source, "untouched = 42\nmalformed(\n".to_owned());
        let snapshot = analysis.snapshot();
        let diagnostics = snapshot.diagnostics(caller).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(!snapshot.diagnostics(source).unwrap().is_empty());
        let locations = snapshot
            .goto_definition(
                FilePosition {
                    file_id: caller,
                    pos: (caller_text.rfind("compute").unwrap() as u32).into(),
                },
                false,
            )
            .unwrap()
            .unwrap();
        let [LocationLink::Local {
            target_file_id,
            origin_selection_range: _,
            target_range: _,
            target_selection_range: _,
        }] = locations.as_slice()
        else {
            panic!("{locations:?}")
        };
        assert_eq!(*target_file_id, interface.source);
    }

    #[test]
    fn navigation_retains_the_loaded_source_and_reexport_policy() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let first = fixture.add_file(&mut analysis.db, "first.bzl", "def compute(): pass\n");
        let origin = fixture.add_file(&mut analysis.db, "origin.bzl", "def original(): pass\n");
        let second = fixture.add_file(
            &mut analysis.db,
            "second.bzl",
            "load(\"origin.bzl\", \"original\")\ncompute = original\n",
        );
        let interface = fixture.add_file(
            &mut analysis.db,
            "shared.bzli",
            "def compute(value: int) -> int: ...\n",
        );
        let text = "load(\"second.bzl\", \"compute\")\ncompute(1)\n";
        let caller = fixture.add_file(&mut analysis.db, "main.bzl", text);
        loader.add_files_from_fixture(&fixture);
        analysis
            .set_type_interfaces([(first, interface), (second, interface)])
            .unwrap();
        let snapshot = analysis.snapshot();
        for (skip, expected) in [(false, second), (true, origin)] {
            let locations = snapshot
                .goto_definition(
                    FilePosition {
                        file_id: caller,
                        pos: (text.rfind("compute").unwrap() as u32).into(),
                    },
                    skip,
                )
                .unwrap()
                .unwrap();
            let [LocationLink::Local {
                target_file_id,
                origin_selection_range: _,
                target_range: _,
                target_selection_range: _,
            }] = locations.as_slice()
            else {
                panic!("{locations:?}")
            };
            assert_eq!(*target_file_id, expected.source);
        }
    }

    #[test]
    fn interface_imports_preserve_original_nominal_provider_identity() {
        let (mut analysis, loader) = Analysis::new_for_test();
        let mut fixture = Fixture::new(&mut analysis.db);
        let source = fixture.add_file(
            &mut analysis.db,
            "source.bzl",
            "Info = provider(fields=[])\ndef consume(): pass\n",
        );
        let interface = fixture.add_file(
            &mut analysis.db,
            "source.bzli",
            "load(\"source.bzl\", Original=\"Info\")\ndef Info() -> Original: ...\ndef consume(value: Original) -> Original: ...\n",
        );
        let caller = fixture.add_file(&mut analysis.db, "main.bzl", "load(\"source.bzl\", \"Info\", \"consume\")\nOther = provider(fields=[])\nresult = consume(Info())\nconsume(Other())\n");
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
        analysis.set_type_interfaces([(source, interface)]).unwrap();
        let snapshot = analysis.snapshot();
        let diagnostics = snapshot.diagnostics(interface).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let diagnostics = snapshot.diagnostics(caller).unwrap();
        let [diagnostic] = diagnostics.as_slice() else {
            panic!("{diagnostics:?}")
        };
        assert_eq!(diagnostic.id().as_str(), "invalid-argument-type");
    }
}
