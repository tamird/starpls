use std::collections::HashMap;
use std::sync::Arc;

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
}

impl SourceSystem {
    pub fn new(base: impl System + 'static) -> Self {
        Self {
            base: Arc::new(base),
            documents: HashMap::new(),
            virtual_sources: HashMap::new(),
            revision: 0,
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
            revision,
        } = self;
        *revision += 1;
        // Keep editor revisions separate from filesystem timestamp revisions.
        let file_revision = FileRevision::new((1 << 127) | u128::from(*revision));
        documents.insert(
            source.clone(),
            OpenDocument {
                path,
                contents,
                version,
                dialect,
                info,
                revision: file_revision,
            },
        );
        Ok(source)
    }

    pub fn close(&mut self, path: &SystemPath) -> Option<OpenDocument> {
        let path = self.source_path(path).ok()?;
        let Self {
            base: _,
            documents,
            virtual_sources: _,
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
            revision: _,
        } = self;
        documents
            .iter()
            .map(|(path, document)| (path.as_path(), document))
    }
}

impl System for SourceSystem {
    fn path_metadata(&self, path: &SystemPath) -> Result<Metadata> {
        if let Some(OpenDocument {
            path: _,
            contents: _,
            version: _,
            dialect: _,
            info: _,
            revision,
        }) = self.document(path)
        {
            return Ok(Metadata::new(*revision, None, FileType::File));
        }
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            revision: _,
        } = self;
        base.path_metadata(path)
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
            revision: _,
        } = self;
        base.canonicalize_path(path)
    }
    fn is_same_file(&self, first: &SystemPath, second: &SystemPath) -> Result<bool> {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            revision: _,
        } = self;
        base.is_same_file(first, second)
    }
    fn which(&self, binary_name: &str) -> WhichResult {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            revision: _,
        } = self;
        base.which(binary_name)
    }
    fn command_executor(&self) -> Option<&dyn CommandExecutor> {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            revision: _,
        } = self;
        base.command_executor()
    }
    fn read_to_notebook(&self, path: &SystemPath) -> std::result::Result<Notebook, NotebookError> {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
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
            revision: _,
        } = self;
        base.read_virtual_path_to_notebook(path)
    }
    fn current_directory(&self) -> &SystemPath {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            revision: _,
        } = self;
        base.current_directory()
    }
    fn user_config_directory(&self) -> Option<SystemPathBuf> {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            revision: _,
        } = self;
        base.user_config_directory()
    }
    fn cache_dir(&self) -> Option<SystemPathBuf> {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
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
            revision: _,
        } = self;
        base.read_directory(path)
    }
    fn walk_directory(&self, path: &SystemPath) -> WalkDirectoryBuilder {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            revision: _,
        } = self;
        base.walk_directory(path)
    }
    fn env_var(&self, name: &str) -> std::result::Result<String, std::env::VarError> {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
            revision: _,
        } = self;
        base.env_var(name)
    }
    fn as_writable(&self) -> Option<&dyn WritableSystem> {
        let Self {
            base,
            documents: _,
            virtual_sources: _,
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
