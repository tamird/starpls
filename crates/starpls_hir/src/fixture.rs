use std::path::Path;
use std::path::PathBuf;

use rustc_hash::FxHashMap;
use starpls_bazel::APIContext;
use starpls_bazel::Builtins;
use starpls_common::Dialect;
use starpls_common::File;
use starpls_common::FileInfo;
use starpls_syntax::TextRange;
use starpls_syntax::TextSize;
use starpls_test_util::make_test_builtins;
use starpls_test_util::FixtureFile;
use starpls_test_util::FixtureType;

use crate::Db;

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
        )
        .expect("test builtin metadata is valid");

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
