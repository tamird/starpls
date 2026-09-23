use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use ruff_db::file_revision::FileRevision;
use ruff_db::system::walk_directory::WalkDirectoryBuilder;
use ruff_db::system::CommandExecutor;
use ruff_db::system::DirectoryEntry;
use ruff_db::system::FileType;
use ruff_db::system::Metadata;
use ruff_db::system::Result;
use ruff_db::system::System;
use ruff_db::system::SystemPath;
use ruff_db::system::SystemPathBuf;
use ruff_db::system::SystemVirtualPath;
use ruff_db::system::SystemVirtualPathBuf;
use ruff_db::system::WhichResult;
use ruff_db::system::WritableSystem;
use ruff_notebook::Notebook;
use ruff_notebook::NotebookError;
use ruff_python_ast::PySourceType;

use crate::Dialect;
use crate::FileInfo;

#[derive(Clone, Debug)]
pub struct OpenDocument {
    pub path: SystemPathBuf,
    pub contents: String,
    pub version: i32,
    pub dialect: Dialect,
    pub info: Option<FileInfo>,
    revision: FileRevision,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocumentStamp {
    pub version: i32,
    revision: FileRevision,
}

impl OpenDocument {
    pub fn stamp(&self) -> DocumentStamp {
        let Self {
            path: _,
            contents: _,
            version,
            dialect: _,
            info: _,
            revision,
        } = self;
        DocumentStamp {
            version: *version,
            revision: *revision,
        }
    }
}

/// Editor buffers belong to a database snapshot. Mutation is only exposed after
/// the database has cancelled and drained outstanding snapshots.
#[derive(Clone, Debug)]
pub struct SourceSystem {
    base: Arc<dyn System>,
    documents: HashMap<SystemPathBuf, OpenDocument>,
    virtual_sources: HashMap<SystemVirtualPathBuf, String>,
    revision: u64,
    aliases: Arc<Mutex<SourceAliases>>,
}

/// Semantic paths can share disk contents and an editor buffer. Disk revisions
/// include the target identity: two archive members can have the same timestamp.
#[derive(Debug, Default)]
struct SourceAliases {
    sources: HashMap<SystemPathBuf, SourceAlias>,
    paths: HashMap<SystemPathBuf, HashSet<SystemPathBuf>>,
    revision: u64,
}

#[derive(Debug)]
struct SourceAlias {
    source: SystemPathBuf,
    disk_revision: Option<FileRevision>,
    revision: FileRevision,
}

impl SourceAliases {
    fn insert(&mut self, path: SystemPathBuf, source: SystemPathBuf) {
        let Self {
            sources,
            paths,
            revision: _,
        } = self;
        let alias = SourceAlias {
            source: source.clone(),
            disk_revision: None,
            revision: FileRevision::default(),
        };
        match sources.entry(path.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(alias);
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if entry.get().source == source {
                    return;
                }
                let old = entry.insert(alias);
                let aliases = paths.get_mut(&old.source).expect("indexed source");
                aliases.remove(&path);
                if aliases.is_empty() {
                    paths.remove(&old.source);
                }
            }
        }
        paths.entry(source).or_default().insert(path);
    }

    fn metadata(&mut self, path: &SystemPath, metadata: Metadata) -> Metadata {
        let Self {
            sources,
            paths: _,
            revision,
        } = self;
        let alias = sources.get_mut(path).expect("indexed source");
        if alias.disk_revision != Some(metadata.revision()) {
            *revision += 1;
            alias.disk_revision = Some(metadata.revision());
            alias.revision = FileRevision::new(u128::from(*revision));
        }
        Metadata::new(alias.revision, metadata.permissions(), metadata.file_type())
    }
}

impl SourceSystem {
    pub fn new(base: impl System + 'static) -> Self {
        Self {
            base: Arc::new(base),
            documents: HashMap::new(),
            virtual_sources: HashMap::new(),
            revision: 0,
            aliases: Arc::default(),
        }
    }

    pub fn open(
        &mut self,
        path: &SystemPath,
        contents: String,
        dialect: Dialect,
        info: Option<FileInfo>,
        version: i32,
    ) -> Result<SystemPathBuf> {
        let source = self.source_path(path)?;
        let path = SystemPath::absolute(path, self.base.current_directory());
        if let Some(document) = self.documents.get(&source) {
            if document.path != path {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("file is already open at {}", document.path),
                ));
            }
        }
        let Self {
            base: _,
            documents,
            virtual_sources: _,
            aliases: _,
            revision,
        } = self;
        *revision += 1;
        // Keep editor revisions separate from filesystem timestamp revisions.
        let file_revision = FileRevision::new((1 << 127) | u128::from(*revision));
        documents.insert(
            source.clone(),
            OpenDocument {
                path: path.clone(),
                contents,
                version,
                dialect,
                info,
                revision: file_revision,
            },
        );
        Ok(path)
    }

    /// Files affected by a buffer edit or filesystem event, including aliases
    /// of both the old and new target when a symlink has changed.
    pub fn paths_for_change(&self, path: &SystemPath) -> Result<Vec<SystemPathBuf>> {
        let path = SystemPath::absolute(path, self.base.current_directory());
        let source = self.source_path(&path)?;
        let mut aliases = self.aliases.lock().expect("source aliases poisoned");
        let mut affected = HashSet::from([path.clone()]);
        for source in aliases
            .sources
            .get(&path)
            .map(|alias| &alias.source)
            .into_iter()
            .chain([&source])
        {
            if let Some(paths) = aliases.paths.get(source) {
                affected.extend(paths.iter().cloned());
            }
        }
        aliases.insert(path, source);
        Ok(affected.into_iter().collect())
    }

    /// Logical interpretations already admitted for this backing source.
    pub fn source_aliases(&self, path: &SystemPath) -> Result<Vec<SystemPathBuf>> {
        let source = self.source_path(path)?;
        let aliases = self.aliases.lock().expect("source aliases poisoned");
        Ok(aliases
            .paths
            .get(&source)
            .into_iter()
            .flatten()
            .cloned()
            .collect())
    }

    pub fn close(&mut self, path: &SystemPath) -> Option<OpenDocument> {
        let path = self.source_path(path).ok()?;
        let Self {
            base: _,
            documents,
            virtual_sources: _,
            aliases: _,
            revision: _,
        } = self;
        documents.remove(&path)
    }

    pub fn set_document_info(&mut self, source: &SystemPath, info: Option<FileInfo>) {
        self.documents.get_mut(source).expect("open document").info = info;
    }

    pub fn document(&self, path: &SystemPath) -> Option<&OpenDocument> {
        let path = self.source_path(path).ok()?;
        let Self {
            base: _,
            documents,
            virtual_sources: _,
            aliases: _,
            revision: _,
        } = self;
        documents.get(&path)
    }

    /// Editor and disk reads share physical identity while an open URI stays stable.
    pub fn source_path(&self, path: &SystemPath) -> Result<SystemPathBuf> {
        let path = SystemPath::absolute(path, self.base.current_directory());
        if self.documents.contains_key(&path) {
            return Ok(path);
        }
        if let Some((source, _)) = self
            .documents
            .iter()
            .find(|(_, document)| document.path == path)
        {
            return Ok(source.clone());
        }
        self.disk_source_path(&path)
    }

    /// An open URI keeps its buffer until the editor closes it, even if a
    /// dependency refresh changes the file reached by that URI on disk.
    pub fn validate_document(&self, path: &SystemPath) -> anyhow::Result<()> {
        let source = self.source_path(path)?;
        let target = self.disk_source_path(path)?;
        if target != source {
            anyhow::bail!(
                "open file {path} refers to {target} after the dependency change (buffer: {source}); close and reopen it to use the new repository"
            );
        }
        Ok(())
    }

    fn disk_source_path(&self, path: &SystemPath) -> Result<SystemPathBuf> {
        let path = SystemPath::absolute(path, self.base.current_directory());
        let mut ancestor = path.as_path();
        loop {
            match self.base.canonicalize_path(ancestor) {
                Ok(root) => {
                    let suffix = path.strip_prefix(ancestor).expect("ancestor of path");
                    return Ok(if suffix.as_str().is_empty() {
                        root
                    } else {
                        root.join(suffix)
                    });
                }
                Err(error) => {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        return Err(error);
                    }
                    let Some(parent) = ancestor.parent() else {
                        return Err(error);
                    };
                    ancestor = parent;
                }
            }
        }
    }

    /// Replace an application-owned declaration input before syncing its Ruff file.
    pub fn set_virtual_source(&mut self, path: &SystemVirtualPath, source: String) -> bool {
        match self.virtual_sources.entry(path.to_path_buf()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if entry.get() == &source {
                    return false;
                }
                entry.insert(source);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(source);
            }
        }
        true
    }

    pub fn documents(&self) -> impl Iterator<Item = (&SystemPath, &OpenDocument)> {
        let Self {
            base: _,
            documents,
            virtual_sources: _,
            aliases: _,
            revision: _,
        } = self;
        documents
            .iter()
            .map(|(path, document)| (path.as_path(), document))
    }
}

impl System for SourceSystem {
    fn path_metadata(&self, path: &SystemPath) -> Result<Metadata> {
        let path = SystemPath::absolute(path, self.base.current_directory());
        let source = self.source_path(&path)?;
        self.aliases
            .lock()
            .expect("source aliases poisoned")
            .insert(path.clone(), source.clone());
        if let Some(OpenDocument {
            path: _,
            contents: _,
            version: _,
            dialect: _,
            info: _,
            revision,
        }) = self.documents.get(&source)
        {
            return Ok(Metadata::new(*revision, None, FileType::File));
        }
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            aliases: _,
            revision: _,
        } = self;
        let metadata = base.path_metadata(&path)?;
        Ok(self
            .aliases
            .lock()
            .expect("source aliases poisoned")
            .metadata(&path, metadata))
    }

    fn read_to_string(&self, path: &SystemPath) -> Result<String> {
        if let Some(OpenDocument {
            path: _,
            contents,
            version: _,
            dialect: _,
            info: _,
            revision: _,
        }) = self.document(path)
        {
            return Ok(contents.clone());
        }
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            aliases: _,
            revision: _,
        } = self;
        base.read_to_string(path)
    }

    fn source_type(&self, _path: &SystemPath) -> Option<PySourceType> {
        Some(PySourceType::Python)
    }

    fn virtual_path_source_type(&self, path: &SystemVirtualPath) -> Option<PySourceType> {
        if self.virtual_sources.contains_key(path) {
            Some(PySourceType::Stub)
        } else {
            Some(PySourceType::Python)
        }
    }
    fn canonicalize_path(&self, path: &SystemPath) -> Result<SystemPathBuf> {
        let source = self.source_path(path)?;
        if self.documents.contains_key(&source) {
            return Ok(source);
        }
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            aliases: _,
            revision: _,
        } = self;
        base.canonicalize_path(path)
    }
    fn is_same_file(&self, first: &SystemPath, second: &SystemPath) -> Result<bool> {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            aliases: _,
            revision: _,
        } = self;
        base.is_same_file(first, second)
    }
    fn which(&self, binary_name: &str) -> WhichResult {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            aliases: _,
            revision: _,
        } = self;
        base.which(binary_name)
    }
    fn command_executor(&self) -> Option<&dyn CommandExecutor> {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            aliases: _,
            revision: _,
        } = self;
        base.command_executor()
    }
    fn read_to_notebook(&self, path: &SystemPath) -> std::result::Result<Notebook, NotebookError> {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            aliases: _,
            revision: _,
        } = self;
        base.read_to_notebook(path)
    }
    fn read_virtual_path_to_string(&self, path: &SystemVirtualPath) -> Result<String> {
        if let Some(source) = self.virtual_sources.get(path) {
            return Ok(source.clone());
        }
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            aliases: _,
            revision: _,
        } = self;
        base.read_virtual_path_to_string(path)
    }
    fn read_virtual_path_to_notebook(
        &self,
        path: &SystemVirtualPath,
    ) -> std::result::Result<Notebook, NotebookError> {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            aliases: _,
            revision: _,
        } = self;
        base.read_virtual_path_to_notebook(path)
    }
    fn current_directory(&self) -> &SystemPath {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            aliases: _,
            revision: _,
        } = self;
        base.current_directory()
    }
    fn user_config_directory(&self) -> Option<SystemPathBuf> {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            aliases: _,
            revision: _,
        } = self;
        base.user_config_directory()
    }
    fn cache_dir(&self) -> Option<SystemPathBuf> {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            aliases: _,
            revision: _,
        } = self;
        base.cache_dir()
    }
    fn read_directory(
        &self,
        path: &SystemPath,
    ) -> Result<Box<dyn Iterator<Item = Result<DirectoryEntry>> + '_>> {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            aliases: _,
            revision: _,
        } = self;
        base.read_directory(path)
    }
    fn walk_directory(&self, path: &SystemPath) -> WalkDirectoryBuilder {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            aliases: _,
            revision: _,
        } = self;
        base.walk_directory(path)
    }
    fn env_var(&self, name: &str) -> std::result::Result<String, std::env::VarError> {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            aliases: _,
            revision: _,
        } = self;
        base.env_var(name)
    }
    fn as_writable(&self) -> Option<&dyn WritableSystem> {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            aliases: _,
            revision: _,
        } = self;
        base.as_writable()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn dyn_clone(&self) -> Box<dyn System> {
        Box::new(self.clone())
    }
}
