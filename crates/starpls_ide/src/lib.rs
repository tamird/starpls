use std::fmt::Debug;
use std::panic;
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(test)]
use dashmap::DashMap;
use ruff_db::diagnostic::DisplayDiagnosticConfig;
use ruff_db::diagnostic::DisplayDiagnostics;
use salsa::Setter;
use starpls_bazel::Builtins;
use starpls_common::Db;
use starpls_common::Diagnostic;
use starpls_common::Dialect;
use starpls_common::File;
use starpls_common::FileInfo;
use starpls_common::LoadItemCandidate;
use starpls_common::ResolvedPath;
use starpls_common::Source;
use starpls_hir::BuiltinDefs;
pub use starpls_hir::Cancelled;
use starpls_hir::Db as _;
use starpls_hir::Environment;
#[cfg(test)]
use starpls_hir::Fixture;
pub use starpls_hir::InferenceOptions;
use starpls_syntax::TextRange;
use starpls_syntax::TextSize;
pub use ty_ide::ReferenceKind;
pub use ty_ide::ReferenceTarget;
pub use ty_ide::SemanticToken;
pub use ty_ide::SemanticTokenModifier;
pub use ty_ide::SemanticTokenType;
pub use ty_ide::SemanticTokens;

pub use crate::completions::CompletionItem;
pub use crate::completions::CompletionItemKind;
pub use crate::completions::CompletionMode;
pub use crate::completions::Edit;
pub use crate::completions::InsertReplaceEdit;
pub use crate::completions::TextEdit;
pub use crate::document_symbols::DocumentSymbol;
pub use crate::document_symbols::SymbolKind;
pub use crate::document_symbols::SymbolTag;
pub use crate::hover::Hover;
pub use crate::hover::Markup;
pub use crate::signature_help::ParameterInfo;
pub use crate::signature_help::SignatureHelp;
pub use crate::signature_help::SignatureInfo;

mod build_targets;
mod completions;
mod diagnostics;
mod document_symbols;
mod find_references;
mod goto_definition;
mod hover;
mod selection;
mod semantic_tokens;
mod show_hir;
mod show_syntax_tree;
mod signature_help;
#[cfg(test)]
mod source;
mod ty;
mod util;

#[cfg(test)]
mod incremental;

pub type Cancellable<T> = Result<T, Cancelled>;

#[salsa::db]
#[derive(Clone)]
pub(crate) struct Database {
    files: ruff_db::files::Files,
    system: Arc<starpls_common::SourceSystem>,
    vendored: ruff_db::vendored::VendoredFileSystem,
    loader: Arc<dyn FileLoader>,
    environment: Option<Environment>,
    semantic: Arc<ty::SemanticSettings>,
    #[cfg(test)]
    executions: Arc<std::sync::atomic::AtomicUsize>,
    // Drop shared source ownership before Salsa wakes a cancelled writer.
    // That writer requires unique access to `system` after snapshots drain.
    storage: salsa::Storage<Self>,
}

#[salsa::db]
impl salsa::Database for Database {}

#[salsa::db]
impl ruff_db::Db for Database {
    fn vendored(&self) -> &ruff_db::vendored::VendoredFileSystem {
        let Self {
            storage: _,
            files: _,
            system: _,
            vendored,
            loader: _,
            environment: _,
            semantic: _,
            #[cfg(test)]
                executions: _,
        } = self;
        vendored
    }
    fn system(&self) -> &dyn ruff_db::system::System {
        let Self {
            storage: _,
            files: _,
            system,
            vendored: _,
            loader: _,
            environment: _,
            semantic: _,
            #[cfg(test)]
                executions: _,
        } = self;
        system.as_ref()
    }
    fn files(&self) -> &ruff_db::files::Files {
        let Self {
            storage: _,
            files,
            system: _,
            vendored: _,
            loader: _,
            environment: _,
            semantic: _,
            #[cfg(test)]
                executions: _,
        } = self;
        files
    }
}
#[salsa::db]
impl starpls_common::Db for Database {
    fn source_system_mut(&mut self) -> &mut starpls_common::SourceSystem {
        salsa::Database::trigger_cancellation(self);
        let Self {
            storage: _,
            files: _,
            system,
            vendored: _,
            loader: _,
            environment: _,
            semantic: _,
            #[cfg(test)]
                executions: _,
        } = self;
        Arc::get_mut(system).expect("snapshots have drained")
    }

    fn load_file(&self, path: &str, dialect: Dialect, from: File) -> anyhow::Result<Option<File>> {
        self.environment().load_revision(self);
        self.loader.load_file(self, path, dialect, from)
    }
    fn list_load_candidates(
        &self,
        path: &str,
        from: File,
    ) -> anyhow::Result<Option<Vec<LoadItemCandidate>>> {
        self.loader
            .list_load_candidates(self, path, from.dialect, from)
    }
    fn resolve_path(
        &self,
        path: &str,
        dialect: Dialect,
        from: File,
    ) -> anyhow::Result<Option<ResolvedPath>> {
        self.environment().load_revision(self);
        self.loader.resolve_path(self, path, dialect, from)
    }
    fn resolve_build_file(&self, file: File) -> Option<String> {
        self.loader.resolve_build_file(self, file)
    }
}

#[salsa::db]
impl starpls_hir::Db for Database {
    fn environment(&self) -> Environment {
        self.environment
            .expect("database initialization is complete")
    }

    fn set_builtin_defs(
        &mut self,
        dialect: Dialect,
        builtins: Builtins,
        rules: Builtins,
    ) -> anyhow::Result<()> {
        self.set_native_metadata(dialect, &builtins, &rules)?;
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

/// Provides the main API for querying facts about the source code. This wraps the main `Database` struct.
pub struct Analysis {
    db: Database,
}

impl Analysis {
    pub fn new(loader: Arc<dyn FileLoader>, options: InferenceOptions) -> anyhow::Result<Self> {
        let cwd = std::env::current_dir()?;
        let cwd = starpls_common::system_path(&cwd)?;
        Ok(Self::with_system(
            loader,
            options,
            ruff_db::system::OsSystem::new(cwd),
        ))
    }

    pub fn with_system(
        loader: Arc<dyn FileLoader>,
        options: InferenceOptions,
        system: impl ruff_db::system::System + 'static,
    ) -> Self {
        let vendored = ty::file_system().clone();
        let semantic = Arc::new(ty::SemanticSettings::new(&vendored));
        let mut db = Database {
            files: Default::default(),
            system: Arc::new(starpls_common::SourceSystem::new(system)),
            vendored,
            storage: Default::default(),
            loader,
            environment: None,
            semantic,
            #[cfg(test)]
            executions: Default::default(),
        };
        #[cfg(test)]
        {
            let executions = db.executions.clone();
            db.storage = salsa::Storage::new(Some(Box::new(move |event| {
                if matches!(event.kind, salsa::EventKind::WillExecute { .. }) {
                    executions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            })));
        }
        db.environment = Some(Environment::initialize(&db, options));
        for dialect in [Dialect::Standard, Dialect::Bazel] {
            db.set_builtin_defs(dialect, Builtins::default(), Builtins::default())
                .expect("bundled native declarations are valid");
        }
        Self { db }
    }

    pub fn file(
        &self,
        path: &std::path::Path,
        dialect: Dialect,
        info: Option<FileInfo>,
    ) -> anyhow::Result<File> {
        let Self { db } = self;
        let system_path = starpls_common::system_path(path)?;
        let source = db.system.source_path(system_path)?;
        let info = db
            .loader
            .file_info(path, source.as_std_path(), dialect, info)?;
        File::from_path(db, source.as_std_path(), dialect, info)
    }

    pub fn open_document(
        &mut self,
        path: &std::path::Path,
        dialect: Dialect,
        info: Option<FileInfo>,
        contents: String,
        version: i32,
    ) -> anyhow::Result<File> {
        let Self { db } = self;
        let system_path = starpls_common::system_path(path)?;
        let source = db.system.source_path(system_path)?;
        let info = db
            .loader
            .file_info(path, source.as_std_path(), dialect, info)?;
        starpls_common::open_document(db, path, dialect, info, contents, version)
    }

    pub fn close_document(&mut self, path: &std::path::Path) -> anyhow::Result<Option<File>> {
        let Self { db } = self;
        let path = starpls_common::system_path(path)?;
        let path = db.system.source_path(path)?;
        let Some(document) = db.system.document(&path) else {
            return Ok(None);
        };
        let file = File::from_path(db, path.as_std_path(), document.dialect, document.info)?;
        db.source_system_mut().close(&path);
        ruff_db::files::File::sync_path(db, &path);
        Ok(Some(file))
    }

    pub fn document(&self, path: &std::path::Path) -> Option<&starpls_common::OpenDocument> {
        let Self { db } = self;
        db.system.document(starpls_common::system_path(path).ok()?)
    }

    pub fn open_files(&self) -> Vec<File> {
        let Self { db } = self;
        db.system
            .documents()
            .map(|(path, document)| {
                File::from_path(db, path.as_std_path(), document.dialect, document.info)
                    .expect("open documents have readable source text")
            })
            .collect()
    }

    /// Open documents and explicitly configured contracts are diagnostic roots.
    pub fn diagnostic_files(&self) -> Vec<File> {
        let mut files = self.open_files();
        for interface in self.type_interface_files() {
            if !files.iter().any(|file| file.source == interface.source) {
                files.push(interface);
            }
        }
        files
    }

    /// Refresh only paths named by the host's filesystem notifications.
    pub fn sync_files(&mut self, paths: &[PathBuf]) -> anyhow::Result<()> {
        let Self { db } = self;
        salsa::Database::trigger_cancellation(db);
        for path in paths {
            let path = starpls_common::system_path(path)?;
            let path = db.system.source_path(path)?;
            ruff_db::files::File::sync_path(db, &path);
        }
        let environment = db.environment();
        let revision = environment.load_revision(db) + 1;
        environment.set_load_revision(db).to(revision);
        Ok(())
    }

    pub fn update_file(&mut self, file: File, contents: String) {
        let Self { db } = self;
        starpls_common::update_file(db, file, contents);
    }

    /// Refresh filesystem metadata after fetching external repositories, then
    /// invalidate host resolution results that did not yet identify a file.
    pub fn invalidate_loads(&mut self) {
        let Self { db } = self;
        salsa::Database::trigger_cancellation(db);
        ruff_db::files::Files::sync_all(db);
        let environment = db.environment();
        let revision = environment.load_revision(db) + 1;
        environment.set_load_revision(db).to(revision);
    }

    /// Replace host resolution only after all snapshots using the old host drain.
    pub fn replace_loader(&mut self, loader: Arc<dyn FileLoader>) -> anyhow::Result<()> {
        let Self { db } = self;
        salsa::Database::trigger_cancellation(db);
        db.loader = loader;
        let mut error = None;
        for (source, document) in db.system.documents() {
            if let Err(admission) = db.loader.file_info(
                document.path.as_std_path(),
                source.as_std_path(),
                document.dialect,
                document.info,
            ) {
                error.get_or_insert(admission);
            }
        }
        self.invalidate_loads();
        match error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub fn snapshot(&self) -> AnalysisSnapshot {
        let Self { db } = self;
        AnalysisSnapshot { db: db.clone() }
    }

    pub fn set_builtin_defs(&mut self, builtins: Builtins, rules: Builtins) -> anyhow::Result<()> {
        let Self { db } = self;
        db.set_builtin_defs(Dialect::Bazel, builtins, rules)
    }

    pub fn set_bazel_prelude_file(&mut self, file_id: File) {
        let Self { db } = self;
        db.set_bazel_prelude_file(file_id);
    }

    pub fn set_all_workspace_targets(&mut self, targets: Vec<String>) {
        let Self { db } = self;
        db.set_all_workspace_targets(targets);
    }

    #[cfg(test)]
    pub(crate) fn new_for_test() -> (Analysis, Arc<SimpleFileLoader>) {
        let loader = Arc::new(SimpleFileLoader::default());
        let analysis = Analysis::with_system(
            loader.clone(),
            Default::default(),
            ruff_db::system::InMemorySystem::default(),
        );
        (analysis, loader)
    }

    #[cfg(test)]
    pub(crate) fn from_single_file_fixture(fixture: &str) -> (Analysis, Fixture) {
        let (mut analysis, loader) = Self::new_for_test();
        let (fixture, _) = Fixture::from_single_file(&mut analysis.db, fixture);
        loader.add_files_from_fixture(&fixture);
        (analysis, fixture)
    }
}

pub struct AnalysisSnapshot {
    db: Database,
}

impl AnalysisSnapshot {
    pub fn path(&self, file: impl Into<ruff_db::files::File>) -> &std::path::Path {
        let Self { db } = self;
        file.into()
            .path(db)
            .as_system_path()
            .expect("navigation targets have system paths")
            .as_std_path()
    }

    pub fn document(&self, path: &std::path::Path) -> Option<&starpls_common::OpenDocument> {
        let Self { db } = self;
        db.system.document(starpls_common::system_path(path).ok()?)
    }

    pub fn open_file(&self, path: &std::path::Path) -> Cancellable<Option<File>> {
        self.query(|db| {
            let path = starpls_common::system_path(path).ok()?;
            let path = db.system.source_path(path).ok()?;
            let document = db.system.document(&path)?;
            File::from_path(db, path.as_std_path(), document.dialect, document.info).ok()
        })
    }

    pub fn is_type_interface_root(&self, file: File) -> bool {
        let Self { db } = self;
        db.environment()
            .type_interfaces(db)
            .values()
            .any(|(_, interface)| interface.source == file.source)
    }

    pub fn file_revision(&self, file: File) -> Cancellable<ruff_db::file_revision::FileRevision> {
        self.query(|db| file.source.revision(db))
    }

    pub fn completions(
        &self,
        pos: FilePosition,
        trigger_character: Option<String>,
    ) -> Cancellable<Option<Vec<CompletionItem>>> {
        self.query(|db| completions::completions(db, pos, trigger_character))
    }

    pub fn diagnostics(&self, file_id: File) -> Cancellable<Vec<Diagnostic>> {
        self.query(|db| diagnostics::diagnostics(db, file_id))
    }

    /// Renders diagnostics obtained from this snapshot using its captured source.
    pub fn render_diagnostics(
        &self,
        diagnostics: &[Diagnostic],
        config: &DisplayDiagnosticConfig,
    ) -> Cancellable<String> {
        self.query(|db| DisplayDiagnostics::new(db, config, diagnostics).to_string())
    }

    pub fn document_symbols(&self, file_id: File) -> Cancellable<Option<Vec<DocumentSymbol>>> {
        self.query(|db| document_symbols::document_symbols(db, file_id))
    }

    pub fn find_references(
        &self,
        pos: FilePosition,
        include_declaration: bool,
    ) -> Cancellable<Option<Vec<Location>>> {
        let file = pos.file_id;
        self.query(|db| {
            find_references::find_references(db, pos, include_declaration).map(|references| {
                references
                    .into_iter()
                    .map(|reference| Location {
                        file_id: file,
                        range: util::text_range(reference.range()),
                    })
                    .collect()
            })
        })
    }

    pub fn document_highlights(
        &self,
        pos: FilePosition,
    ) -> Cancellable<Option<Vec<ReferenceTarget>>> {
        self.query(|db| find_references::find_references(db, pos, true))
    }

    pub fn goto_definition(
        &self,
        pos: FilePosition,
        skip_re_exports: bool,
    ) -> Cancellable<Option<Vec<LocationLink>>> {
        self.query(|db| goto_definition::goto_definition(db, pos, skip_re_exports))
    }

    pub fn hover(&self, pos: FilePosition) -> Cancellable<Option<Hover>> {
        self.query(|db| hover::hover(db, pos))
    }

    pub fn source(&self, file: impl Into<ruff_db::files::File>) -> Cancellable<Source> {
        let file = file.into();
        self.query(move |db| starpls_common::source(db, file))
    }

    pub fn show_hir(&self, file_id: File) -> Cancellable<Option<String>> {
        self.query(|db| show_hir::show_hir(db, file_id))
    }

    pub fn show_syntax_tree(&self, file_id: File) -> Cancellable<Option<String>> {
        self.query(|db| show_syntax_tree::show_syntax_tree(db, file_id))
    }

    pub fn signature_help(&self, pos: FilePosition) -> Cancellable<Option<SignatureHelp>> {
        self.query(|db| signature_help::signature_help(db, pos))
    }

    pub fn semantic_tokens(
        &self,
        file: File,
        range: Option<TextRange>,
    ) -> Cancellable<SemanticTokens> {
        self.query(|db| semantic_tokens::semantic_tokens(db, file, range))
    }

    /// Helper method to handle Salsa cancellations.
    fn query<'a, F, T>(&'a self, f: F) -> Cancellable<T>
    where
        F: FnOnce(&'a Database) -> T + panic::UnwindSafe,
    {
        starpls_hir::Cancelled::catch(|| {
            let Self { db } = self;
            f(db)
        })
    }
}

impl panic::RefUnwindSafe for AnalysisSnapshot {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub file_id: File,
    pub range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocationLink {
    Local {
        origin_selection_range: Option<TextRange>,
        target_range: TextRange,
        target_selection_range: TextRange,
        target_file_id: ruff_db::files::File,
    },
    External {
        origin_selection_range: Option<TextRange>,
        target_path: PathBuf,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilePosition {
    pub file_id: File,
    pub pos: TextSize,
}

/// A trait for loading a path and listing its exported symbols.
pub trait FileLoader: Send + Sync + 'static {
    /// Establish host context before a file enters semantic analysis.
    fn file_info(
        &self,
        _path: &std::path::Path,
        _source: &std::path::Path,
        _dialect: Dialect,
        info: Option<FileInfo>,
    ) -> anyhow::Result<Option<FileInfo>> {
        Ok(info)
    }

    fn resolve_path(
        &self,
        db: &dyn Db,
        path: &str,
        dialect: Dialect,
        from: File,
    ) -> anyhow::Result<Option<ResolvedPath>>;

    /// Open the Starlark file corresponding to the given `path` and of the given `Dialect`.
    fn load_file(
        &self,
        db: &dyn Db,
        path: &str,
        dialect: Dialect,
        from: File,
    ) -> anyhow::Result<Option<File>>;

    /// Returns a list of Starlark modules that can be loaded from the given `path`.
    fn list_load_candidates(
        &self,
        db: &dyn Db,
        path: &str,
        dialect: Dialect,
        from: File,
    ) -> anyhow::Result<Option<Vec<LoadItemCandidate>>>;

    /// If the specified file is a BUILD file, returns its package.
    fn resolve_build_file(&self, db: &dyn Db, file_id: File) -> Option<String>;
}

/// Simple implementation of [`FileLoader`] backed by a HashMap.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct SimpleFileLoader {
    files: DashMap<String, File>,
    requests: std::sync::Mutex<Vec<String>>,
}

#[cfg(test)]
impl SimpleFileLoader {
    pub(crate) fn add_files_from_fixture(&self, fixture: &Fixture) {
        for (path, file) in &fixture.path_to_file_id {
            self.files.insert(path.to_string_lossy().to_string(), *file);
        }
    }
}

#[cfg(test)]
impl FileLoader for SimpleFileLoader {
    fn resolve_path(
        &self,
        _db: &dyn Db,
        _path: &str,
        _dialect: Dialect,
        _from: File,
    ) -> anyhow::Result<Option<ResolvedPath>> {
        Ok(None)
    }

    fn load_file(
        &self,
        db: &dyn Db,
        path: &str,
        dialect: Dialect,
        _from: File,
    ) -> anyhow::Result<Option<File>> {
        self.requests.lock().unwrap().push(path.to_owned());
        let result = if let Some(file) = self.files.get(path) {
            File::from_path(db, file.path(db), file.dialect, file.info)
        } else {
            File::from_path(db, std::path::Path::new(path), dialect, None)
        };
        result.map(Some)
    }

    fn list_load_candidates(
        &self,
        _db: &dyn Db,
        _path: &str,
        _dialect: Dialect,
        _from: File,
    ) -> anyhow::Result<Option<Vec<LoadItemCandidate>>> {
        Ok(None)
    }

    fn resolve_build_file(&self, _db: &dyn Db, _file_id: File) -> Option<String> {
        None
    }
}
