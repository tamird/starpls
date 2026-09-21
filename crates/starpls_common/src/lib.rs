use std::fmt::Debug;
use std::path::PathBuf;

use ruff_source_file::LineIndex;
use starpls_bazel::APIContext;
pub use system::DocumentStamp;
pub use system::OpenDocument;
pub use system::SourceSystem;

pub use crate::diagnostics::diagnostic;
pub use crate::diagnostics::Diagnostic;
pub use crate::diagnostics::DiagnosticId;
pub use crate::diagnostics::DiagnosticTag;
pub use crate::diagnostics::Severity;

mod diagnostics;
mod system;
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

pub enum ResolvedPath {
    Source { path: PathBuf },
    BuildTarget { build_file: File, target: String },
}

/// The base Salsa database. Supports file-related operations, like getting/setting file contents.
#[salsa::db]
pub trait Db: ruff_db::Db {
    /// Mutating buffers cancels and drains all database snapshots first.
    fn source_system_mut(&mut self) -> &mut SourceSystem;

    /// Loads a file from the filesystem.
    fn load_file(&self, path: &str, dialect: Dialect, from: File) -> anyhow::Result<Option<File>>;

    fn list_load_candidates(
        &self,
        path: &str,
        from: File,
    ) -> anyhow::Result<Option<Vec<LoadItemCandidate>>>;

    fn resolve_path(
        &self,
        path: &str,
        dialect: Dialect,
        from: File,
    ) -> anyhow::Result<Option<ResolvedPath>>;

    fn resolve_build_file(&self, file_id: File) -> Option<String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FileInfo {
    Bazel {
        api_context: APIContext,
        is_external: bool,
    },
}

/// A physical file interpreted in a particular Starlark host context.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct File {
    pub source: ruff_db::files::File,
    pub dialect: Dialect,
    pub info: Option<FileInfo>,
}

impl From<File> for ruff_db::files::File {
    fn from(file: File) -> Self {
        file.source
    }
}

impl File {
    pub fn from_path(
        db: &dyn Db,
        path: &std::path::Path,
        dialect: Dialect,
        info: Option<FileInfo>,
    ) -> anyhow::Result<Self> {
        let path = system_path(path)?;
        let source = ruff_db::files::system_path_to_file(db, path)
            .map_err(|error| anyhow::anyhow!("cannot open {path}: {error}"))?;
        let text = ruff_db::source::source_text(db, source);
        if let Some(error) = text.read_error() {
            anyhow::bail!("cannot read {path}: {error}");
        }
        Ok(Self {
            source,
            dialect,
            info,
        })
    }

    pub fn path(self, db: &dyn Db) -> &std::path::Path {
        let Self {
            source,
            dialect: _,
            info: _,
        } = self;
        source
            .path(db)
            .as_system_path()
            .expect("Starlark files have system paths")
            .as_std_path()
    }

    pub fn contents(self, db: &dyn Db) -> ruff_db::source::SourceText {
        let Self {
            source,
            dialect: _,
            info: _,
        } = self;
        ruff_db::source::source_text(db, source)
    }

    pub fn api_context(self) -> Option<APIContext> {
        let Self {
            source: _,
            dialect: _,
            info,
        } = self;
        info.map(
            |FileInfo::Bazel {
                 api_context,
                 is_external: _,
             }| api_context,
        )
    }

    /// Native annotations follow Bazel's ordinary `.bzl` syntax mode.
    /// Prelude, `.scl`, and other Starlark hosts retain their existing grammar.
    pub fn allows_native_annotations(self, db: &dyn Db) -> bool {
        self.dialect == Dialect::Bazel
            && matches!(self.api_context(), None | Some(APIContext::Bzl))
            && self
                .path(db)
                .extension()
                .is_some_and(|extension| extension == "bzl")
    }

    pub fn is_external(self) -> Option<bool> {
        let Self {
            source: _,
            dialect: _,
            info,
        } = self;
        info.map(
            |FileInfo::Bazel {
                 api_context: _,
                 is_external,
             }| is_external,
        )
    }
}

pub fn system_path(path: &std::path::Path) -> anyhow::Result<&ruff_db::system::SystemPath> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8: {}", path.display()))?;
    Ok(ruff_db::system::SystemPath::new(path))
}

pub fn open_document(
    db: &mut dyn Db,
    path: &std::path::Path,
    dialect: Dialect,
    info: Option<FileInfo>,
    contents: String,
    version: i32,
) -> anyhow::Result<File> {
    let path = system_path(path)?.to_path_buf();
    db.source_system_mut()
        .open(&path, contents, dialect, info, version);
    ruff_db::files::File::sync_path(db, &path);
    File::from_path(db, path.as_std_path(), dialect, info)
}

pub fn update_file(db: &mut dyn Db, file: File, contents: String) {
    let path = file.path(db).to_path_buf();
    open_document(db, &path, file.dialect, file.info, contents, 0).expect("known file path");
}

/// Python syntax version shared by source parsing and semantic queries.
pub const PARSER_VERSION: ruff_python_ast::PythonVersion =
    ruff_python_ast::PythonVersion::latest_ty();

/// The canonical Python-shaped parse. Starlark validation is applied separately.
pub fn parsed_module(db: &dyn Db, file: File) -> &ruff_db::parsed::ParsedModule {
    let file = ruff_db::PythonFile::new_with_source_type(
        db,
        file.source,
        PARSER_VERSION,
        ruff_python_ast::PySourceType::Python,
    );
    ruff_db::parsed::parsed_module(db, file)
}

/// Starlark validation and type comments for the canonical parsed revision.
pub fn syntax_info(db: &dyn Db, file: File) -> &[starpls_syntax::TypeComment] {
    &syntax_info_query(db, file.source, (file.dialect, file.info)).comments
}

/// Unsupported subtrees from the same canonical validation result as syntax diagnostics.
pub fn syntax_exclusions(db: &dyn Db, file: File) -> &[ruff_python_ast::NodeIndex] {
    &syntax_info_query(db, file.source, (file.dialect, file.info)).excluded
}

pub fn syntax_diagnostics(db: &dyn Db, file: File) -> &[Diagnostic] {
    &syntax_info_query(db, file.source, (file.dialect, file.info)).diagnostics
}

// Semantic annotation callbacks read this query during fixpoint inference,
// where Salsa accumulators are unsupported. Reporting consumes these results.
#[derive(Debug, PartialEq, Eq)]
struct SyntaxInfo {
    comments: Vec<starpls_syntax::TypeComment>,
    excluded: Vec<ruff_python_ast::NodeIndex>,
    diagnostics: Box<[Diagnostic]>,
}

#[salsa::tracked(returns(ref))]
fn syntax_info_query(
    db: &dyn Db,
    source: ruff_db::files::File,
    context: (Dialect, Option<FileInfo>),
) -> SyntaxInfo {
    let (dialect, info) = context;
    let file = File {
        source,
        dialect,
        info,
    };
    let contents = file.contents(db);
    let mut diagnostics = Vec::new();
    if let Some(error) = contents.read_error() {
        diagnostics.push(diagnostic(
            file,
            DiagnosticId::Io,
            Severity::Error,
            Default::default(),
            format!("cannot read {}: {error}", file.path(db).display()),
            [],
        ));
    }
    let parsed = parsed_module(db, file).load(db);
    let mut errors = |err: starpls_syntax::SyntaxError| {
        diagnostics.push(diagnostic(
            file,
            DiagnosticId::InvalidSyntax,
            Severity::Error,
            err.range,
            err.message,
            [],
        ));
    };
    let excluded = starpls_syntax::validate(
        &contents,
        &parsed,
        file.allows_native_annotations(db),
        &mut errors,
    );
    let comments = starpls_syntax::parse_type_comments(&contents, parsed.tokens(), &mut errors);
    SyntaxInfo {
        comments,
        excluded,
        diagnostics: diagnostics.into_boxed_slice(),
    }
}

pub fn line_index(db: &dyn Db, file: File) -> LineIndex {
    ruff_db::source::line_index(db, file.source)
}

/// Owned text and index from the same file revision. Cloning shares their storage.
#[derive(Clone)]
pub struct Source {
    pub text: ruff_db::source::SourceText,
    pub index: LineIndex,
}

pub fn source(db: &dyn Db, file: ruff_db::files::File) -> Source {
    Source {
        text: ruff_db::source::source_text(db, file),
        index: ruff_db::source::line_index(db, file),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InFile<T: Clone + Debug> {
    pub file: File,
    pub value: T,
}

impl<T: Copy + Debug> Copy for InFile<T> {}
