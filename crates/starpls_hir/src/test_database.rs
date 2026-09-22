use std::sync::Arc;

use salsa::Setter;
use starpls_bazel::Builtins;
use starpls_common::File;
use starpls_common::LoadItemCandidate;
use starpls_common::ResolvedPath;

use crate::BuiltinDefs;
use crate::Dialect;
use crate::Environment;
use crate::InferenceOptions;

#[salsa::db]
#[derive(Clone)]
pub(crate) struct TestDatabase {
    files: ruff_db::files::Files,
    system: Arc<starpls_common::SourceSystem>,
    vendored: ruff_db::vendored::VendoredFileSystem,
    environment: Option<Environment>,
    // Release the shared source system before waking a cancelled writer.
    storage: salsa::Storage<Self>,
}

impl Default for TestDatabase {
    fn default() -> Self {
        let mut db = Self {
            storage: Default::default(),
            files: Default::default(),
            system: Arc::new(starpls_common::SourceSystem::new(
                ruff_db::system::InMemorySystem::default(),
            )),
            vendored: Default::default(),
            environment: None,
        };
        db.environment = Some(Environment::initialize(&db, InferenceOptions::default()));
        db
    }
}

#[salsa::db]
impl salsa::Database for TestDatabase {}

#[salsa::db]
impl ruff_db::Db for TestDatabase {
    fn vendored(&self) -> &ruff_db::vendored::VendoredFileSystem {
        &self.vendored
    }
    fn system(&self) -> &dyn ruff_db::system::System {
        self.system.as_ref()
    }
    fn files(&self) -> &ruff_db::files::Files {
        &self.files
    }
}
#[salsa::db]
impl starpls_common::Db for TestDatabase {
    fn source_system_mut(&mut self) -> &mut starpls_common::SourceSystem {
        salsa::Database::trigger_cancellation(self);
        Arc::get_mut(&mut self.system).expect("snapshots have drained")
    }
    fn load_file(
        &self,
        _path: &str,
        _dialect: Dialect,
        _from: File,
    ) -> anyhow::Result<Option<File>> {
        Ok(None)
    }

    fn list_load_candidates(
        &self,
        _path: &str,
        _from: File,
    ) -> anyhow::Result<Option<Vec<LoadItemCandidate>>> {
        Ok(None)
    }

    fn resolve_path(
        &self,
        _path: &str,
        _dialect: Dialect,
        _from: File,
    ) -> anyhow::Result<Option<ResolvedPath>> {
        Ok(None)
    }

    fn resolve_build_file(&self, _file_id: File) -> Option<String> {
        None
    }
}

#[salsa::db]
impl crate::Db for TestDatabase {
    fn environment(&self) -> Environment {
        self.environment
            .expect("database initialization is complete")
    }

    fn set_builtin_defs(
        &mut self,
        dialect: Dialect,
        builtins: Builtins,
        rules: starpls_bazel::build::BuildLanguage,
    ) -> anyhow::Result<()> {
        let defs = self.environment().builtin_defs(self, dialect);
        defs.set_builtins(self).to(builtins);
        defs.set_rules(self).to(rules);
        Ok(())
    }

    fn get_builtin_defs(&self, dialect: &Dialect) -> BuiltinDefs {
        self.environment().builtin_defs(self, *dialect)
    }

    fn set_bazel_prelude_file(&mut self, file_id: File) {
        self.environment().set_prelude_file(self).to(Some(file_id));
    }

    fn get_bazel_prelude_file(&self) -> Option<File> {
        self.environment().prelude_file(self)
    }

    fn set_all_workspace_targets(&mut self, targets: Vec<String>) {
        self.environment()
            .set_all_workspace_targets(self)
            .to(Arc::new(targets));
    }

    fn get_all_workspace_targets(&self) -> Arc<Vec<String>> {
        self.environment().all_workspace_targets(self)
    }
}

#[cfg(test)]
mod parse_tests {
    use ruff_python_ast::HasNodeIndex;
    use ruff_text_size::Ranged;

    use super::TestDatabase;

    #[test]
    fn canonical_parse_uses_module_grammar_for_arbitrary_extensions() {
        let mut db = TestDatabase::default();
        let file = starpls_common::open_document(
            &mut db,
            std::path::Path::new("plain.ipynb"),
            starpls_common::Dialect::Standard,
            None,
            "%timeit a = b".into(),
            0,
        )
        .unwrap();
        let source = file.contents(&db);
        assert!(source.read_error().is_none());
        assert!(!source.is_notebook());
        let parsed = starpls_common::parsed_module(&db, file).load(&db);
        assert!(!parsed.has_valid_syntax());
    }

    #[test]
    fn canonical_parse_handles_deep_chains() {
        std::thread::Builder::new()
            .stack_size(ruff_db::STACK_SIZE)
            .spawn(|| {
                for separator in [" + ", ".", " or "] {
                    let contents = std::iter::repeat_n("x", 10_000)
                        .collect::<Vec<_>>()
                        .join(separator);
                    let mut db = TestDatabase::default();
                    let file = starpls_common::open_document(
                        &mut db,
                        std::path::Path::new("deep.star"),
                        starpls_common::Dialect::Standard,
                        None,
                        contents,
                        0,
                    )
                    .unwrap();
                    let parsed = starpls_common::parsed_module(&db, file).load(&db);
                    assert!(parsed.errors().is_empty());
                    let first = ruff_text_size::TextRange::new(0.into(), 1.into());
                    let covering =
                        ruff_python_ast::find_node::covering_node(parsed.syntax().into(), first);
                    let node = covering.node();
                    assert_eq!(node.range(), first);
                    assert_eq!(parsed.get_by_index(node.node_index().load()).range(), first);
                    assert!(starpls_common::syntax_info(&db, file).is_empty());
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
