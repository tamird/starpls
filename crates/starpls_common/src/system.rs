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
use ruff_db::system::WhichResult;
use ruff_db::system::WritableSystem;
use ruff_notebook::Notebook;
use ruff_notebook::NotebookError;
use ruff_python_ast::PySourceType;

use crate::Dialect;
use crate::FileInfo;

#[derive(Clone, Debug)]
pub struct OpenDocument {
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
    revision: u64,
}

impl SourceSystem {
    pub fn new(base: impl System + 'static) -> Self {
        Self {
            base: Arc::new(base),
            documents: HashMap::new(),
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
    ) {
        let Self {
            base,
            documents,
            revision,
        } = self;
        *revision += 1;
        // Keep editor revisions separate from filesystem timestamp revisions.
        let file_revision = FileRevision::new((1 << 127) | u128::from(*revision));
        let path = SystemPath::absolute(path, base.current_directory());
        documents.insert(
            path,
            OpenDocument {
                contents,
                version,
                dialect,
                info,
                revision: file_revision,
            },
        );
    }

    pub fn close(&mut self, path: &SystemPath) -> Option<OpenDocument> {
        let Self {
            base,
            documents,
            revision: _,
        } = self;
        let path = SystemPath::absolute(path, base.current_directory());
        documents.remove(&path)
    }

    pub fn document(&self, path: &SystemPath) -> Option<&OpenDocument> {
        let Self {
            base,
            documents,
            revision: _,
        } = self;
        let path = SystemPath::absolute(path, base.current_directory());
        documents.get(&path)
    }
}

impl System for SourceSystem {
    fn path_metadata(&self, path: &SystemPath) -> Result<Metadata> {
        if let Some(OpenDocument {
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
            revision: _,
        } = self;
        base.path_metadata(path)
    }

    fn read_to_string(&self, path: &SystemPath) -> Result<String> {
        if let Some(OpenDocument {
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
            revision: _,
        } = self;
        base.read_to_string(path)
    }

    fn source_type(&self, _path: &SystemPath) -> Option<PySourceType> {
        Some(PySourceType::Python)
    }

    fn virtual_path_source_type(&self, _path: &SystemVirtualPath) -> Option<PySourceType> {
        Some(PySourceType::Python)
    }
    fn canonicalize_path(&self, path: &SystemPath) -> Result<SystemPathBuf> {
        let Self {
            base,
            documents: _,
            revision: _,
        } = self;
        base.canonicalize_path(path)
    }
    fn is_same_file(&self, first: &SystemPath, second: &SystemPath) -> Result<bool> {
        let Self {
            base,
            documents: _,
            revision: _,
        } = self;
        base.is_same_file(first, second)
    }
    fn which(&self, binary_name: &str) -> WhichResult {
        let Self {
            base,
            documents: _,
            revision: _,
        } = self;
        base.which(binary_name)
    }
    fn command_executor(&self) -> Option<&dyn CommandExecutor> {
        let Self {
            base,
            documents: _,
            revision: _,
        } = self;
        base.command_executor()
    }
    fn read_to_notebook(&self, path: &SystemPath) -> std::result::Result<Notebook, NotebookError> {
        let Self {
            base,
            documents: _,
            revision: _,
        } = self;
        base.read_to_notebook(path)
    }
    fn read_virtual_path_to_string(&self, path: &SystemVirtualPath) -> Result<String> {
        let Self {
            base,
            documents: _,
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
            revision: _,
        } = self;
        base.read_virtual_path_to_notebook(path)
    }
    fn current_directory(&self) -> &SystemPath {
        let Self {
            base,
            documents: _,
            revision: _,
        } = self;
        base.current_directory()
    }
    fn user_config_directory(&self) -> Option<SystemPathBuf> {
        let Self {
            base,
            documents: _,
            revision: _,
        } = self;
        base.user_config_directory()
    }
    fn cache_dir(&self) -> Option<SystemPathBuf> {
        let Self {
            base,
            documents: _,
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
            revision: _,
        } = self;
        base.read_directory(path)
    }
    fn walk_directory(&self, path: &SystemPath) -> WalkDirectoryBuilder {
        let Self {
            base,
            documents: _,
            revision: _,
        } = self;
        base.walk_directory(path)
    }
    fn env_var(&self, name: &str) -> std::result::Result<String, std::env::VarError> {
        let Self {
            base,
            documents: _,
            revision: _,
        } = self;
        base.env_var(name)
    }
    fn as_writable(&self) -> Option<&dyn WritableSystem> {
        let Self {
            base,
            documents: _,
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
