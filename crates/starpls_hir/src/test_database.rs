use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use rustc_hash::FxHashMap;
use salsa::Setter;
use starpls_bazel::APIContext;
use starpls_bazel::Builtins;
use starpls_common::File;
use starpls_common::FileInfo;
use starpls_common::LoadItemCandidate;
use starpls_common::ResolvedPath;
use starpls_syntax::TextRange;
use starpls_syntax::TextSize;
use starpls_test_util::make_test_builtins;
use starpls_test_util::FixtureFile;
use starpls_test_util::FixtureType;

use crate::BuiltinDefs;
use crate::Db;
use crate::Dialect;
use crate::Environment;
use crate::InferenceOptions;

#[salsa::db]
#[derive(Clone)]
pub(crate) struct TestDatabase {
    storage: salsa::Storage<Self>,
    files: ruff_db::files::Files,
    system: Arc<starpls_common::SourceSystem>,
    vendored: ruff_db::vendored::VendoredFileSystem,
    environment: Option<Environment>,
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

    fn set_builtin_defs(&mut self, dialect: Dialect, builtins: Builtins, rules: Builtins) {
        let defs = self.environment().builtin_defs(self, dialect);
        defs.set_builtins(self).to(builtins);
        defs.set_rules(self).to(rules);
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

#[allow(unused)]
#[derive(Default)]
pub(crate) struct TestDatabaseBuilder {
    options: InferenceOptions,
    functions: Vec<String>,
    globals: Vec<(String, String)>,
    types: Vec<FixtureType>,
}

#[allow(unused)]
impl TestDatabaseBuilder {
    pub fn add_function(&mut self, name: impl Into<String>) {
        self.functions.push(name.into());
    }

    pub fn add_global(&mut self, name: impl Into<String>, ty: impl Into<String>) {
        self.globals.push((name.into(), ty.into()));
    }

    pub fn add_type(&mut self, ty: FixtureType) {
        self.types.push(ty);
    }

    pub fn set_inference_options(&mut self, options: InferenceOptions) {
        self.options = options;
    }

    pub fn build(self) -> TestDatabase {
        let mut db = TestDatabase::default();
        db.environment().set_options(&mut db).to(self.options);
        db.set_builtin_defs(
            Dialect::Bazel,
            make_test_builtins(self.functions, self.globals, self.types),
            Builtins::default(),
        );
        db
    }
}

pub struct Fixture {
    pub path_to_file_id: FxHashMap<PathBuf, File>,
    pub selected_ranges: Vec<(File, TextRange)>,
    pub cursor_pos: Option<(File, TextSize)>,
}

impl Fixture {
    pub fn main_file(&self) -> File {
        self.path_to_file_id[Path::new("main.bzl")]
    }

    pub fn new(db: &mut dyn Db) -> Self {
        let fixture = Self {
            path_to_file_id: Default::default(),
            selected_ranges: Default::default(),
            cursor_pos: None,
        };

        // Add builtins here as needed for tests.
        // TODO(withered-magic): Make this a little bit nicer.
        let functions = vec!["provider", "rule", "struct"];
        let globals = vec![("attr", "attr")];
        let types = vec![FixtureType::new("attr", vec![], vec!["int", "string"])];
        db.set_builtin_defs(
            Dialect::Bazel,
            make_test_builtins(functions, globals, types),
            Builtins::default(),
        );

        fixture
    }

    /// Provides a convenient way to quickly construct a fixture from a single file, as is commonly
    /// needed by tests.
    pub fn from_single_file(db: &mut dyn Db, contents: &str) -> (Self, File) {
        let mut fixture = Self::new(db);
        let file_id = fixture.add_file(db, "main.bzl", contents);
        (fixture, file_id)
    }

    pub fn add_file(&mut self, db: &mut dyn Db, path: impl AsRef<Path>, contents: &str) -> File {
        self.add_file_with_options(
            db,
            path,
            contents,
            Dialect::Bazel,
            Some(FileInfo::Bazel {
                api_context: APIContext::Bzl,
                is_external: false,
            }),
        )
    }

    pub fn add_prelude_file(&mut self, db: &mut dyn Db, contents: &str) -> File {
        let file_id = self.add_file_with_options(
            db,
            "tools/build_rules/prelude_bazel",
            contents,
            Dialect::Bazel,
            Some(FileInfo::Bazel {
                api_context: APIContext::Prelude,
                is_external: false,
            }),
        );
        db.set_bazel_prelude_file(file_id);
        file_id
    }

    pub fn add_file_with_options(
        &mut self,
        db: &mut dyn Db,
        path: impl AsRef<Path>,
        contents: &str,
        dialect: Dialect,
        info: Option<FileInfo>,
    ) -> File {
        let fixture = FixtureFile::parse(contents);
        let file_id =
            starpls_common::open_document(db, path.as_ref(), dialect, info, fixture.contents, 0)
                .unwrap();
        self.path_to_file_id
            .insert(path.as_ref().to_path_buf(), file_id);

        if let Some(cursor_pos) = fixture.cursor_pos {
            if self.cursor_pos.is_some() {
                panic!("cannot have more than one cursor_pos");
            }
            self.cursor_pos = Some((file_id, cursor_pos));
        }
        self.selected_ranges.extend(
            fixture
                .selected_ranges
                .into_iter()
                .map(|range| (file_id, range)),
        );

        file_id
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
                    let tree = starpls_common::parse(&db, file);
                    assert!(!tree.syntax().text_range().is_empty());
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
