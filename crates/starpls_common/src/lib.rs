use std::fmt::Debug;
use std::path::PathBuf;

use ruff_source_file::LineIndex;
use salsa::Accumulator;
use starpls_bazel::APIContext;
use starpls_syntax::parse_module;
use starpls_syntax::Module;
use starpls_syntax::ParseTree;

pub use crate::diagnostics::Diagnostic;
pub use crate::diagnostics::DiagnosticTag;
pub use crate::diagnostics::Diagnostics;
pub use crate::diagnostics::FileRange;
pub use crate::diagnostics::Severity;

mod diagnostics;
mod util;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dialect {
    Standard,
    Bazel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadItemCandidateKind {
    Directory,
    File,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadItemCandidate {
    pub kind: LoadItemCandidateKind,
    pub path: String,
    pub replace_trailing_slash: bool,
}

/// A Key corresponding to an interned file path. Use these instead of `Path`s to refer to files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(pub u32);

pub enum ResolvedPath {
    Source {
        path: PathBuf,
    },
    BuildTarget {
        build_file: FileId,
        target: String,
        contents: Option<String>,
    },
}

/// The base Salsa database. Supports file-related operations, like getting/setting file contents.
#[salsa::db]
pub trait Db: salsa::Database {
    /// Creates a file or updates the existing input for this identity. Opening
    /// a previously loaded file must preserve its importers' dependencies.
    fn create_file(
        &mut self,
        file_id: FileId,
        dialect: Dialect,
        info: Option<FileInfo>,
        contents: String,
    ) -> File;

    /// Sets the contents the `File` identified by the given `FileId`. Has no affect
    /// if the file doesn't exist.
    fn update_file(&mut self, file_id: FileId, contents: String);

    /// Loads a file from the filesystem.
    fn load_file(&self, path: &str, dialect: Dialect, from: FileId)
        -> anyhow::Result<Option<File>>;

    /// Returns the `File` identified by the given `FileId`.
    fn get_file(&self, file_id: FileId) -> Option<File>;

    fn list_load_candidates(
        &self,
        path: &str,
        from: FileId,
    ) -> anyhow::Result<Option<Vec<LoadItemCandidate>>>;

    fn resolve_path(
        &self,
        path: &str,
        dialect: Dialect,
        from: FileId,
    ) -> anyhow::Result<Option<ResolvedPath>>;

    fn resolve_build_file(&self, file_id: FileId) -> Option<String>;
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FileInfo {
    Bazel {
        api_context: APIContext,
        is_external: bool,
    },
}

#[salsa::input(debug)]
pub struct File {
    #[returns(clone)]
    pub id: FileId,
    #[returns(clone)]
    pub dialect: Dialect,
    #[returns(clone)]
    pub info: Option<FileInfo>,
    #[returns(ref)]
    pub contents: String,
}

impl File {
    pub fn api_context(&self, db: &dyn Db) -> Option<APIContext> {
        self.info(db).map(|data| match data {
            FileInfo::Bazel { api_context, .. } => api_context,
        })
    }

    pub fn is_external(&self, db: &dyn Db) -> Option<bool> {
        self.info(db).map(|data| match data {
            FileInfo::Bazel { is_external, .. } => is_external,
        })
    }
}

pub type Parse = ParseTree<Module>;

#[salsa::tracked(returns(ref))]
pub fn parse(db: &dyn Db, file: File) -> Parse {
    let parse = parse_module(file.contents(db), &mut |err| {
        Diagnostics(Diagnostic {
            message: err.message,
            range: FileRange {
                file_id: file.id(db),
                range: err.range,
            },
            severity: Severity::Error,
            tags: None,
        })
        .accumulate(db)
    });
    parse
}

#[salsa::tracked(returns(ref))]
pub fn line_index(db: &dyn Db, file: File) -> LineIndex {
    LineIndex::from_source_text(file.contents(db))
}

/// Text and its index borrowed from the same file revision.
#[derive(Clone, Copy)]
pub struct Source<'a> {
    pub text: &'a str,
    pub index: &'a LineIndex,
}

pub fn source(db: &dyn Db, file: File) -> Source<'_> {
    Source {
        text: file.contents(db),
        index: line_index(db, file),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InFile<T: Clone + Debug> {
    pub file: File,
    pub value: T,
}

impl<T: Copy + Debug> Copy for InFile<T> {}
