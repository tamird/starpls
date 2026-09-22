use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::path::MAIN_SEPARATOR;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use crossbeam_channel::Sender;
use parking_lot::RwLock;
use starpls_bazel::client::BazelClient;
use starpls_bazel::label::PartialParse;
use starpls_bazel::label::RepoKind;
use starpls_bazel::APIContext;
use starpls_bazel::Label;
use starpls_bazel::ParseError;
use starpls_bazel::{self};
use starpls_common::Db;
use starpls_common::Dialect;
use starpls_common::File;
use starpls_common::FileInfo;
use starpls_common::LoadItemCandidate;
use starpls_common::LoadItemCandidateKind;
use starpls_common::ResolvedPath;
use starpls_ide::FileLoader;

use crate::event_loop::FetchExternalRepoRequest;
use crate::event_loop::Task;

macro_rules! try_opt {
    ($expr:expr) => {
        match { $expr } {
            Some(res) => res,
            None => return Ok(None),
        }
    };
}

/// Shared recursive admission for the checker and workspace editor operations.
pub(crate) fn visit_source_entry(entry: &walkdir::DirEntry, ignore_patterns: &[String]) -> bool {
    let name = entry.file_name();
    !name
        .to_str()
        .is_some_and(|name| name.starts_with('.') && name != ".")
        && !is_ignored_name(name, ignore_patterns)
}

pub(crate) fn is_repository_root(path: &Path) -> bool {
    ["MODULE.bazel", "REPO.bazel", "WORKSPACE", "WORKSPACE.bazel"]
        .iter()
        .any(|name| path.join(name).is_file())
}

pub(crate) fn is_ignored_name(name: &std::ffi::OsStr, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| name == pattern.as_str())
}

pub(crate) fn source_kind(
    workspace: &Path,
    path: &Path,
    extensions: &[&str],
) -> Option<(Dialect, Option<APIContext>)> {
    let (dialect, context) = dialect_and_api_context_for_workspace_path(workspace, path)?;
    if dialect == Dialect::Standard
        && !path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extensions.contains(&extension))
    {
        return None;
    }
    Some((dialect, context))
}

pub(crate) const DEPENDENCY_FILES: [&str; 7] = [
    "MODULE.bazel",
    "MODULE.bazel.lock",
    "WORKSPACE",
    "WORKSPACE.bazel",
    "WORKSPACE.bzlmod",
    ".bazelrc",
    ".bazelversion",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum RepositoryFetch {
    Pending,
    Ready,
    Failed,
}

enum RepositoryMapping {
    Pending,
    Ready(Arc<HashMap<String, String>>),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RepositoryContext {
    Resolved(Repository),
    Unknown,
    Displaced,
}

pub(crate) struct DefaultFileLoader {
    bazel_client: Arc<dyn BazelClient>,
    workspace: PathBuf,
    workspace_name: Option<String>,
    external_output_base: Option<PathBuf>,
    fetch_repo_sender: Sender<Task>,
    bzlmod_enabled: bool,
    repositories: RwLock<HashMap<PathBuf, RepositoryContext>>,
    configuration_inputs: RwLock<HashMap<PathBuf, Option<Vec<u8>>>>,
    repository_roots: RwLock<HashSet<PathBuf>>,
    configuration_revision: Option<u64>,
    defer_mappings: bool,
    repository_fetches: RwLock<HashMap<String, RepositoryFetch>>,
    repository_mappings: RwLock<HashMap<String, RepositoryMapping>>,
    paused: AtomicBool,
}

impl DefaultFileLoader {
    pub(crate) fn new(
        bazel_client: Arc<dyn BazelClient>,
        workspace: PathBuf,
        workspace_name: Option<String>,
        external_output_base: impl Into<Option<PathBuf>>,
        fetch_repo_sender: Sender<Task>,
        bzlmod_enabled: bool,
    ) -> Self {
        let loader = Self {
            bazel_client,
            workspace,
            workspace_name,
            external_output_base: external_output_base.into(),
            fetch_repo_sender,
            bzlmod_enabled,
            repositories: Default::default(),
            configuration_inputs: Default::default(),
            repository_roots: Default::default(),
            configuration_revision: None,
            defer_mappings: false,
            repository_fetches: Default::default(),
            repository_mappings: Default::default(),
            paused: AtomicBool::new(false),
        };
        loader.watch_repository(&loader.workspace);
        loader
    }

    pub(crate) fn with_context(
        &self,
        workspace_name: Option<String>,
        external: PathBuf,
        bzlmod: bool,
    ) -> Self {
        Self::new(
            self.bazel_client.clone(),
            self.workspace.clone(),
            workspace_name,
            external,
            self.fetch_repo_sender.clone(),
            bzlmod,
        )
    }

    pub(crate) fn for_editor(mut self, revision: u64) -> Self {
        self.configuration_revision = Some(revision);
        self
    }

    pub(crate) fn with_deferred_mappings(mut self) -> Self {
        self.defer_mappings = true;
        self
    }

    /// A bounded batch leaves room for the command and environment on every
    /// supported platform, including generated repositories with long names.
    pub(crate) fn pending_repository_mappings(&self) -> Vec<String> {
        let mut pending: Vec<_> = self
            .repository_mappings
            .read()
            .iter()
            .filter(|(_, state)| matches!(state, RepositoryMapping::Pending))
            .map(|(name, _)| name.clone())
            .collect();
        pending.sort_unstable();
        let mut bytes = 0;
        let count = pending
            .iter()
            .take_while(|name| {
                bytes += name.len() + 1;
                bytes <= 16 * 1024
            })
            .count()
            .max(1);
        pending.truncate(count);
        pending
    }

    /// Call after invalidating load queries and draining their readers.
    pub(crate) fn resolve_repository_mappings(&self, repositories: &[String]) {
        let names: Vec<_> = repositories.iter().map(String::as_str).collect();
        let result = self
            .bazel_client
            .dump_repo_mappings(&names)
            .and_then(|mappings| {
                if mappings.len() != repositories.len() {
                    bail!("repository mapping batch returned an unexpected result count");
                }
                Ok(mappings)
            });
        let mut cached = self.repository_mappings.write();
        match result {
            Ok(mappings) => {
                cached.extend(
                    repositories
                        .iter()
                        .cloned()
                        .zip(mappings.into_iter().map(RepositoryMapping::Ready)),
                );
            }
            Err(error) => {
                let message = format!("repository mapping batch failed: {error:#}");
                for repository in repositories {
                    cached.insert(
                        repository.clone(),
                        RepositoryMapping::Failed(message.clone()),
                    );
                }
            }
        }
    }

    pub(crate) fn has_bazel_context(&self) -> bool {
        self.external_output_base.is_some()
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.has_bazel_context() && !self.paused.load(Ordering::Relaxed)
    }

    pub(crate) fn mappings_ready(&self) -> bool {
        self.repository_mappings
            .read()
            .values()
            .all(|mapping| matches!(mapping, RepositoryMapping::Ready(_)))
    }

    pub(crate) fn fresh(&self) -> Self {
        Self::new(
            self.bazel_client.clone(),
            self.workspace.clone(),
            self.workspace_name.clone(),
            self.external_output_base.clone(),
            self.fetch_repo_sender.clone(),
            self.bzlmod_enabled,
        )
    }

    pub(crate) fn pause(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    pub(crate) fn bzlmod_enabled(&self) -> bool {
        self.bzlmod_enabled
    }

    pub(crate) fn finish_mapping(
        &self,
        repository: String,
        result: anyhow::Result<starpls_bazel::client::RepoMapping>,
    ) {
        let mapping = match result {
            Ok(mapping) => RepositoryMapping::Ready(mapping),
            Err(error) => RepositoryMapping::Failed(format!("{error:#}")),
        };
        self.repository_mappings.write().insert(repository, mapping);
    }

    fn repository_mapping(
        &self,
        repository: &str,
    ) -> anyhow::Result<Option<Arc<HashMap<String, String>>>> {
        if self.paused.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let mut mappings = self.repository_mappings.write();
        match mappings.entry(repository.to_owned()) {
            Entry::Occupied(entry) => {
                return match entry.get() {
                    RepositoryMapping::Pending => Ok(None),
                    RepositoryMapping::Ready(mapping) => Ok(Some(mapping.clone())),
                    RepositoryMapping::Failed(error) => Err(anyhow::Error::msg(error.clone())),
                }
            }
            Entry::Vacant(entry) => {
                if let Some(revision) = self.configuration_revision {
                    entry.insert(RepositoryMapping::Pending);
                    self.fetch_repo_sender.send(Task::ResolveRepoMapping {
                        repository: repository.to_owned(),
                        revision,
                    })?;
                    return Ok(None);
                }
                if self.defer_mappings {
                    entry.insert(RepositoryMapping::Pending);
                    return Ok(None);
                }
            }
        }
        drop(mappings);
        let result = self.bazel_client.dump_repo_mapping(repository);
        self.finish_mapping(repository.to_owned(), result);
        self.repository_mapping(repository)
    }

    fn watch_repository(&self, root: &Path) {
        if self.repository_roots.write().insert(root.to_path_buf()) {
            for name in DEPENDENCY_FILES {
                self.watch_manifest(&root.join(name));
                if let Ok(canonical) = root.canonicalize() {
                    self.watch_manifest(&canonical.join(name));
                }
            }
        }
    }

    pub(crate) fn watch_manifest(&self, path: &Path) {
        self.configuration_inputs
            .write()
            .insert(path.to_path_buf(), fs::read(path).ok());
    }

    pub(crate) fn configuration_paths(&self) -> Vec<PathBuf> {
        self.configuration_inputs.read().keys().cloned().collect()
    }

    pub(crate) fn read_manifest(&self, path: &Path) -> std::io::Result<String> {
        let contents = fs::read(path);
        self.configuration_inputs
            .write()
            .insert(path.to_path_buf(), contents.as_ref().ok().cloned());
        let contents = contents?;
        String::from_utf8(contents)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    pub(crate) fn configuration_inputs(&self) -> HashMap<PathBuf, Option<Vec<u8>>> {
        self.configuration_inputs.read().clone()
    }

    pub(crate) fn repository_roots(&self) -> anyhow::Result<Vec<PathBuf>> {
        self.repository_roots
            .read()
            .iter()
            .map(|root| match root.canonicalize() {
                Ok(root) => Ok(root),
                Err(error) => {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        Ok(root.clone())
                    } else {
                        Err(error)
                            .with_context(|| format!("cannot watch repository {}", root.display()))
                    }
                }
            })
            .collect()
    }

    pub(crate) fn begin_fetch(&self, repository: String) -> bool {
        match self.repository_fetches.write().entry(repository) {
            Entry::Vacant(entry) => {
                entry.insert(RepositoryFetch::Pending);
                true
            }
            Entry::Occupied(_) => false,
        }
    }

    pub(crate) fn finish_fetch(
        &self,
        repositories: impl IntoIterator<Item = String>,
        success: bool,
    ) {
        let state = if success {
            RepositoryFetch::Ready
        } else {
            RepositoryFetch::Failed
        };
        self.repository_fetches
            .write()
            .extend(repositories.into_iter().map(|name| (name, state)));
    }

    fn document_context(
        &self,
        previous: &Self,
        original: &Path,
        source: &Path,
    ) -> anyhow::Result<Option<Repository>> {
        let old = match previous.repositories.read().get(source) {
            Some(RepositoryContext::Resolved(repository)) => Some(repository.clone()),
            Some(RepositoryContext::Unknown) => {
                if previous.has_bazel_context() {
                    return Ok(None);
                }
                None
            }
            Some(RepositoryContext::Displaced) => bail!(
                "repository context for open file {} changed; close and reopen it",
                original.display()
            ),
            None => None,
        };
        let target = match original.canonicalize() {
            Ok(target) => target,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(error).with_context(|| {
                        format!("cannot resolve open file {}", original.display())
                    });
                }
                source.to_path_buf()
            }
        };
        let repository_moved = if let Some(repository) = &old {
            if repository.name.is_empty() {
                false
            } else {
                match self
                    .external_output_base
                    .as_ref()
                    .context("Bazel configuration is loading")?
                    .join(&repository.name)
                    .canonicalize()
                {
                    Ok(root) => root != repository.root,
                    Err(error) => {
                        if error.kind() != std::io::ErrorKind::NotFound {
                            return Err(error).context("cannot resolve the open file's repository");
                        }
                        true
                    }
                }
            }
        } else {
            false
        };
        if target != source || repository_moved {
            bail!("open file {} refers to {} after the dependency change (buffer: {}); close and reopen it to use the new repository", original.display(), target.display(), source.display());
        }
        if !previous.has_bazel_context() {
            if old
                .as_ref()
                .is_some_and(|repository| !repository.name.is_empty())
            {
                return Ok(old);
            }
            return self.repository_for_source(original, source);
        }
        Ok(old)
    }

    pub(crate) fn restore_document_context(
        &self,
        previous: &Self,
        original: &Path,
        source: &Path,
    ) -> anyhow::Result<()> {
        let admission = self
            .document_context(previous, original, source)
            .and_then(|repository| self.record_repository(source, repository));
        match admission {
            Ok(()) => Ok(()),
            Err(error) => {
                // Admission precedes publication. A refusal also replaces any
                // prepared context that would reinterpret this open buffer.
                self.repositories
                    .write()
                    .insert(source.to_path_buf(), RepositoryContext::Displaced);
                Err(error)
            }
        }
    }

    pub(crate) fn open_repository_changed(&self, original: &Path, source: &Path) -> bool {
        if self.repositories.read().get(source) == Some(&RepositoryContext::Displaced) {
            return false;
        }
        self.document_context(self, original, source).is_err()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Repository {
    pub(crate) name: String,
    pub(crate) root: PathBuf,
}

pub(crate) struct ResolvedLabel {
    pub(crate) resolved_path: PathBuf,
    pub(crate) repository: Repository,
}

impl DefaultFileLoader {
    fn resolve_label(
        &self,
        db: &dyn Db,
        label: &Label,
        from: File,
    ) -> anyhow::Result<Option<ResolvedLabel>> {
        let from_path = from.path(db);
        let repository = try_opt!(self.repository_for_source(from_path, from_path)?);
        if (!self.has_bazel_context() || self.paused.load(Ordering::Relaxed))
            && (!repository.name.is_empty()
                || (!label.repo().is_empty() && label.kind() != RepoKind::Current))
        {
            return Ok(None);
        }
        self.ensure_repository(&repository)?;
        if self.bzlmod_enabled
            && label.kind() == RepoKind::Apparent
            && !label.repo().is_empty()
            && self.repository_mapping(&repository.name)?.is_none()
        {
            return Ok(None);
        }
        let repository = self.resolve_repository(label, &repository)?;
        let resolved_path = if label.is_relative() {
            package_for_path(from_path, &repository.root)?
        } else {
            repository.root.join(label.package())
        }
        .join(label.target());

        Ok(Some(ResolvedLabel {
            resolved_path,
            repository,
        }))
    }

    pub(crate) fn loaded_paths(&self) -> Vec<PathBuf> {
        self.repositories.read().keys().cloned().collect()
    }

    pub(crate) fn is_editable(&self, path: &Path) -> anyhow::Result<bool> {
        let workspace = self.workspace.canonicalize()?;
        if !path.starts_with(&workspace) {
            return Ok(false);
        }
        if path
            .parent()
            .into_iter()
            .flat_map(Path::ancestors)
            .take_while(|parent| *parent != workspace)
            .any(is_repository_root)
        {
            return Ok(false);
        }
        // The source is already canonical, including an unsaved new document.
        Ok(self
            .repository_for_source(path, path)?
            .is_some_and(|repository| repository.name.is_empty()))
    }

    pub(crate) fn main_repository(&self) -> Repository {
        Repository {
            name: String::new(),
            root: self.workspace.clone(),
        }
    }

    pub(crate) fn fetch_repository(&self, repository: &Repository) -> anyhow::Result<()> {
        self.watch_repository(&repository.root);
        if repository.name.is_empty() {
            return Ok(());
        }
        if self.bzlmod_enabled {
            self.bazel_client.fetch_repo(&repository.name)?;
        } else {
            self.bazel_client
                .null_query_external_repo_targets(&repository.name)?;
        }
        self.finish_fetch([repository.name.clone()], true);
        Ok(())
    }

    pub(crate) fn selected_module(
        &self,
        repository: &Repository,
    ) -> anyhow::Result<Option<starpls_bazel::client::SelectedModule>> {
        if self.bzlmod_enabled {
            self.bazel_client.selected_module(&repository.name)
        } else {
            Ok(None)
        }
    }

    pub(crate) fn resolve_repository(
        &self,
        label: &Label,
        from: &Repository,
    ) -> anyhow::Result<Repository> {
        self.watch_repository(&from.root);
        if self.paused.load(Ordering::Relaxed)
            && (!from.name.is_empty() || !label.repo().is_empty())
        {
            bail!("Bazel configuration is loading");
        }
        let name = match label.kind() {
            RepoKind::Current => return Ok(from.clone()),
            RepoKind::Canonical => label.repo().to_owned(),
            RepoKind::Apparent => {
                if label.repo().is_empty() {
                    String::new()
                } else if self.bzlmod_enabled {
                    let mapping = self
                        .repository_mapping(&from.name)?
                        .context("Bazel repository mapping is loading")?;
                    let name = mapping.get(label.repo()).cloned();
                    name.with_context(|| {
                        format!(
                            "cannot resolve repository @{} from @@{}",
                            label.repo(),
                            from.name
                        )
                    })?
                } else if self.workspace_name.as_deref() == Some(label.repo()) {
                    String::new()
                } else {
                    label.repo().to_owned()
                }
            }
        };
        let root = if name.is_empty() {
            self.workspace.clone()
        } else {
            self.external_output_base
                .as_ref()
                .context("Bazel configuration is loading")?
                .join(&name)
        };
        self.watch_repository(&root);
        Ok(Repository { name, root })
    }

    /// Each physical file has one repository context for the lifetime of the loader.
    pub(crate) fn register_path(
        &self,
        path: &Path,
        repository: &Repository,
    ) -> anyhow::Result<PathBuf> {
        let path = path.canonicalize()?;
        let Repository { name, root } = repository;
        let root = root.canonicalize()?;
        let repository = Repository {
            name: name.clone(),
            root,
        };
        self.record_repository(&path, Some(repository))?;
        Ok(path)
    }

    fn record_repository(&self, path: &Path, repository: Option<Repository>) -> anyhow::Result<()> {
        if let Some(repository) = &repository {
            self.watch_repository(&repository.root);
        }
        let repository = repository.map_or(RepositoryContext::Unknown, RepositoryContext::Resolved);
        match self.repositories.write().entry(path.to_path_buf()) {
            Entry::Vacant(entry) => {
                entry.insert(repository);
            }
            Entry::Occupied(entry) => {
                if entry.get() != &repository {
                    bail!(
                        "conflicting Bazel repositories for {}: {:?} and {:?}",
                        entry.key().display(),
                        entry.get(),
                        repository
                    );
                }
            }
        }
        Ok(())
    }

    pub(crate) fn repository_for_path(&self, path: &Path) -> anyhow::Result<Option<Repository>> {
        let canonical = path.canonicalize()?;
        self.repository_for_source(path, &canonical)
    }

    fn repository_for_source(
        &self,
        path: &Path,
        canonical: &Path,
    ) -> anyhow::Result<Option<Repository>> {
        match self.repositories.read().get(canonical) {
            Some(RepositoryContext::Unknown) => return Ok(None),
            Some(RepositoryContext::Displaced) => return Ok(None),
            Some(RepositoryContext::Resolved(repository)) => {
                let repository = repository.clone();
                if !self
                    .external_output_base
                    .as_ref()
                    .is_some_and(|base| path.starts_with(base))
                {
                    return Ok(Some(repository));
                }
            }
            None => {}
        }
        let external = self.external_output_base.as_ref().and_then(|base| {
            path.strip_prefix(base)
                .ok()
                .map(|relative| (base, relative))
        });
        let repository = if let Some((base, relative)) = external {
            let name = relative
                .components()
                .next()
                .context("missing Bazel repository name")?;
            let name = name
                .as_os_str()
                .to_str()
                .context("Bazel repository name is not UTF-8")?;
            Some(Repository {
                name: name.to_owned(),
                root: base.join(name),
            })
        } else if path.starts_with(&self.workspace)
            || path.extension().is_some_and(|ext| ext == "bzli")
        {
            let nested = canonical
                .parent()
                .into_iter()
                .flat_map(Path::ancestors)
                .take_while(|parent| *parent != self.workspace)
                .any(is_repository_root);
            if !self.has_bazel_context() && nested {
                None
            } else {
                Some(self.main_repository())
            }
        } else {
            None
        };
        let repository = match repository {
            Some(Repository { name, root }) => {
                let root = match root.canonicalize() {
                    Ok(root) => root,
                    Err(error) => {
                        if error.kind() != std::io::ErrorKind::NotFound {
                            return Err(error.into());
                        }
                        root
                    }
                };
                Some(Repository { name, root })
            }
            None => None,
        };
        self.record_repository(canonical, repository.clone())?;
        Ok(repository)
    }

    fn ensure_repository(&self, repository: &Repository) -> anyhow::Result<()> {
        if !repository.name.is_empty()
            && self.configuration_revision.is_some()
            && self.repository_fetches.read().get(&repository.name) != Some(&RepositoryFetch::Ready)
        {
            let _ = self.fetch_repo_sender.send(Task::FetchExternalRepoRequest(
                FetchExternalRepoRequest {
                    repo: repository.name.clone(),
                    revision: self.configuration_revision.unwrap_or(0),
                },
            ));
            bail!(
                "repository @@{} must be fetched after the dependency change",
                repository.name
            );
        }
        Ok(())
    }

    fn read_file(
        &self,
        db: &dyn Db,
        path: PathBuf,
        dialect: Dialect,
        info: Option<FileInfo>,
        repository: Option<Repository>,
    ) -> anyhow::Result<File> {
        let result = (|| {
            if let Some(repository) = &repository {
                self.ensure_repository(repository)?;
            }
            let system_path = starpls_common::system_path(&path)?;
            let canonical = match db.system().canonicalize_path(system_path) {
                Ok(path) => path,
                Err(error) => {
                    // Record missing inputs so creating a dependency invalidates loads.
                    File::from_path(db, &path, dialect, info)?;
                    return Err(error.into());
                }
            };
            if let Some(Repository { name, root }) = &repository {
                let root = starpls_common::system_path(root)?;
                let root = db.system().canonicalize_path(root)?;
                let repository = Repository {
                    name: name.clone(),
                    root: root.as_std_path().to_path_buf(),
                };
                self.record_repository(canonical.as_std_path(), Some(repository))?;
            }
            let info = self.file_info(
                canonical.as_std_path(),
                canonical.as_std_path(),
                dialect,
                info,
            )?;
            File::from_path(db, canonical.as_std_path(), dialect, info)
        })();
        if result.is_err() {
            if let Some(Repository {
                name: canonical_repo,
                root: _,
            }) = repository
            {
                if !canonical_repo.is_empty()
                    && self.external_output_base.as_ref().is_some_and(|base| {
                        !base.join(&canonical_repo).try_exists().unwrap_or(false)
                    })
                {
                    let _ = self.fetch_repo_sender.send(Task::FetchExternalRepoRequest(
                        FetchExternalRepoRequest {
                            repo: canonical_repo,
                            revision: self.configuration_revision.unwrap_or(0),
                        },
                    ));
                }
            }
        }
        result
    }
}

impl FileLoader for DefaultFileLoader {
    fn file_info(
        &self,
        path: &Path,
        source: &Path,
        dialect: Dialect,
        info: Option<FileInfo>,
    ) -> anyhow::Result<Option<FileInfo>> {
        if dialect != Dialect::Bazel {
            return Ok(info);
        }
        let repository = self.repository_for_source(path, source)?;
        Ok(info.map(
            |FileInfo::Bazel {
                 api_context,
                 is_external,
             }| FileInfo::Bazel {
                api_context,
                is_external: repository
                    .map_or(is_external, |repository| !repository.name.is_empty()),
            },
        ))
    }
    fn resolve_path(
        &self,
        db: &dyn Db,
        path: &str,
        dialect: Dialect,
        from: File,
    ) -> anyhow::Result<Option<ResolvedPath>> {
        if dialect != Dialect::Bazel {
            return Ok(None);
        }

        // Parse the load path as a Bazel label.
        let label = match Label::parse(path) {
            Ok(label) => label,
            Err(err) => return Err(anyhow!("error parsing label: {}", err.err)),
        };

        let resolved_label = try_opt!(self.resolve_label(db, &label, from)?);
        self.ensure_repository(&resolved_label.repository)?;
        let res = if fs::metadata(&resolved_label.resolved_path)
            .ok()
            .map(|metadata| metadata.is_file())
            .unwrap_or_default()
        {
            ResolvedPath::Source {
                path: resolved_label.resolved_path,
            }
        } else {
            if label.target().is_empty() {
                return Ok(None);
            }

            let parent = try_opt!(resolved_label.resolved_path.parent());
            let build_file = try_opt!(fs::read_dir(parent)
                .into_iter()
                .flat_map(|entries| entries.into_iter())
                .find_map(|entry| match entry.ok()?.file_name().to_str()? {
                    file_name @ ("BUILD" | "BUILD.bazel") => Some(file_name.to_string()),
                    _ => None,
                }));
            let path = parent.join(build_file);

            let build_file = self.read_file(
                db,
                path,
                Dialect::Bazel,
                Some(FileInfo::Bazel {
                    api_context: APIContext::Build,
                    is_external: false,
                }),
                Some(resolved_label.repository),
            )?;

            ResolvedPath::BuildTarget {
                build_file,
                target: label.target().to_string(),
            }
        };

        Ok(Some(res))
    }

    fn load_file(
        &self,
        db: &dyn Db,
        path: &str,
        dialect: Dialect,
        from: File,
    ) -> anyhow::Result<Option<File>> {
        let (path, info, repository) = match dialect {
            Dialect::Standard => {
                // Find the importing file's directory.
                let mut from_path = from.path(db).to_path_buf();
                assert!(from_path.pop());

                // Resolve the given path relative to the importing file's directory.
                (from_path.join(path), None, None)
            }
            Dialect::Bazel => {
                // Parse the load path as a Bazel label.
                let label = match Label::parse(path) {
                    Ok(label) => label,
                    Err(err) => return Err(anyhow!("error parsing label: {}", err.err)),
                };

                // Only .bzl files can be loaded.
                if !label.target().ends_with(".bzl") {
                    bail!("cannot load a non-bzl file");
                }

                let ResolvedLabel {
                    resolved_path,
                    repository,
                } = try_opt!(self.resolve_label(db, &label, from)?);

                let is_external = !repository.name.is_empty();
                (
                    resolved_path,
                    Some(FileInfo::Bazel {
                        api_context: APIContext::Bzl,
                        is_external,
                    }),
                    Some(repository),
                )
            }
        };

        let file = self.read_file(db, path, dialect, info, repository)?;
        Ok(Some(file))
    }

    fn list_load_candidates(
        &self,
        db: &dyn Db,
        path: &str,
        dialect: Dialect,
        from: File,
    ) -> anyhow::Result<Option<Vec<LoadItemCandidate>>> {
        let from_path = from.path(db).to_path_buf();
        match dialect {
            Dialect::Standard => {
                let from_dir = from_path.parent().unwrap();
                let has_trailing_slash = path.ends_with(MAIN_SEPARATOR);
                let mut path = from_dir.join(path);
                if !has_trailing_slash && !path.pop() {
                    return Ok(None);
                }

                let path = path.canonicalize()?;
                let mut candidates = vec![];
                let readdir = fs::read_dir(path)?;

                for entry in readdir {
                    let entry = entry?;
                    let file_type = entry.file_type()?;
                    if file_type.is_file() {
                        if let Some(name) = entry.file_name().to_str() {
                            if name.ends_with(".star") || name.ends_with(".sky") {
                                candidates.push(LoadItemCandidate {
                                    kind: LoadItemCandidateKind::File,
                                    path: name.to_string(),
                                    replace_trailing_slash: false,
                                })
                            }
                        }
                    }
                }

                Ok(Some(candidates))
            }
            Dialect::Bazel => {
                let (label, err) = match Label::parse(path) {
                    Ok(label) => (label, None),
                    Err(PartialParse { partial, err }) => (partial, Some(err)),
                };
                let repository = try_opt!(self.repository_for_source(&from_path, &from_path)?);
                if !self.is_ready() && (!repository.name.is_empty() || !label.repo().is_empty()) {
                    return Ok(None);
                }
                self.ensure_repository(&repository)?;

                if !label.has_leading_slashes()
                    && !label.is_relative()
                    && err != Some(ParseError::InvalidRepo)
                {
                    if label.kind() == RepoKind::Apparent && self.bzlmod_enabled {
                        let mapping = try_opt!(self.repository_mapping(&repository.name)?);
                        let names = mapping.keys().cloned();
                        return Ok(Some(
                            names
                                .into_iter()
                                .map(|name| LoadItemCandidate {
                                    kind: LoadItemCandidateKind::Directory,
                                    path: name,
                                    replace_trailing_slash: false,
                                })
                                .collect(),
                        ));
                    }
                    return Ok(match label.kind() {
                        RepoKind::Canonical | RepoKind::Apparent => Some(
                            fs::read_dir(try_opt!(self.external_output_base.as_ref()))?
                                .filter_map(|entry| {
                                    let entry = entry.ok()?;
                                    entry.file_type().ok()?.is_dir().then(|| LoadItemCandidate {
                                        kind: LoadItemCandidateKind::Directory,
                                        path: entry.file_name().to_string_lossy().to_string(),
                                        replace_trailing_slash: false,
                                    })
                                })
                                .chain(self.workspace_name.as_ref().map(|name| LoadItemCandidate {
                                    kind: LoadItemCandidateKind::Directory,
                                    path: name.clone(),
                                    replace_trailing_slash: false,
                                }))
                                .collect(),
                        ),
                        _ => None,
                    });
                }

                let repository = self.resolve_repository(&label, &repository)?;
                self.ensure_repository(&repository)?;
                let root = repository.root;
                let package = if label.is_relative() {
                    package_for_path(&from_path, &root)?
                } else {
                    PathBuf::new()
                };

                match err {
                    Some(ParseError::EmptyPackage) => {
                        // An empty package usually indicates that the user is about to
                        // starting typing the package name.
                        read_dir_packages_and_targets(root, false).map(Some)
                    }

                    Some(ParseError::EmptyTarget) => {
                        // Same logic as above, but for the target.
                        read_dir_targets(if label.is_relative() {
                            package
                        } else {
                            root.join(label.package())
                        })
                        .map(Some)
                    }

                    Some(ParseError::InvalidPackageEndingSlash) | None => {
                        // TODO(withered-magic): Handle targets like in `//foo:bar/baz.bzl`.
                        if label.is_relative() {
                            // If the label is relative, check for target candidates in the current package.
                            read_dir_targets(package).map(Some)
                        } else if !label.target().is_empty() && !label.has_target_shorthand() {
                            // Check for target candidates in the label's package.
                            let package_dir = root.join(label.package());
                            let (target_dir, _) =
                                try_opt!(strip_slashes_or_pop_dir(label.target()));
                            read_dir_targets(package_dir.join(target_dir)).map(Some)
                        } else {
                            // Otherwise, find package candidates.
                            let (package_dir, has_trailing_slash) =
                                try_opt!(strip_slashes_or_pop_dir(label.package()));
                            read_dir_packages_and_targets(
                                root.join(package_dir),
                                has_trailing_slash,
                            )
                            .map(Some)
                        }
                    }

                    _ => {
                        // Don't offer completions for any other parsing errors.
                        Ok(None)
                    }
                }
            }
        }
    }

    fn resolve_build_file(&self, db: &dyn Db, file_id: File) -> Option<String> {
        let path = file_id.path(db).to_path_buf();
        let path = path.strip_prefix(&self.workspace).ok()?;
        if matches!(
            &*path.file_name()?.to_string_lossy(),
            "BUILD" | "BUILD.bazel"
        ) {
            Some(path.parent()?.to_string_lossy().to_string())
        } else {
            None
        }
    }
}

fn package_for_path(path: &Path, root: &Path) -> anyhow::Result<PathBuf> {
    if !path.starts_with(root) {
        bail!(
            "{} is outside repository {}",
            path.display(),
            root.display()
        );
    }
    for directory in path.ancestors().skip(1) {
        if directory.join("BUILD").try_exists()? || directory.join("BUILD.bazel").try_exists()? {
            return Ok(directory.to_path_buf());
        }
        if directory == root {
            return Ok(root.to_path_buf());
        }
    }
    bail!(
        "{} is outside repository {}",
        path.display(),
        root.display()
    )
}

fn read_dir_packages_and_targets(
    path: impl AsRef<Path>,
    has_trailing_slash: bool,
) -> anyhow::Result<Vec<LoadItemCandidate>> {
    Ok(fs::read_dir(path)?
        .flatten()
        .filter_map(|entry| {
            entry
                .file_type()
                .map(|file_type| (file_type, entry.file_name()))
                .ok()
        })
        .filter_map(|(file_type, file_name)| {
            file_name.to_str().and_then(|file_name| {
                let (kind, path, replace_trailing_slash) = if file_type.is_dir() {
                    (
                        LoadItemCandidateKind::Directory,
                        file_name.to_string(),
                        false,
                    )
                } else if file_name.ends_with(".bzl") {
                    (
                        LoadItemCandidateKind::File,
                        format!(":{}", file_name),
                        has_trailing_slash,
                    )
                } else {
                    return None;
                };
                Some(LoadItemCandidate {
                    kind,
                    path,
                    replace_trailing_slash,
                })
            })
        })
        .collect())
}

fn read_dir_targets(path: impl AsRef<Path>) -> anyhow::Result<Vec<LoadItemCandidate>> {
    Ok(fs::read_dir(path)?
        .flatten()
        .filter_map(|entry| {
            entry
                .file_type()
                .map(|file_type| (file_type, entry.file_name()))
                .ok()
        })
        .filter_map(|(file_type, file_name)| {
            file_name.to_str().and_then(|file_name| {
                Some(LoadItemCandidate {
                    kind: if file_type.is_dir() {
                        LoadItemCandidateKind::Directory
                    } else if file_name.ends_with(".bzl") {
                        LoadItemCandidateKind::File
                    } else {
                        return None;
                    },
                    path: file_name.to_string(),
                    replace_trailing_slash: false,
                })
            })
        })
        .collect())
}

fn strip_slashes_or_pop_dir(input: &str) -> Option<(PathBuf, bool)> {
    Some(if input.ends_with('/') {
        (PathBuf::from(input.trim_end_matches('/')), true)
    } else {
        let mut target_dir = PathBuf::from(input);
        if !target_dir.pop() {
            return None;
        }
        (target_dir, false)
    })
}

pub(crate) fn dialect_and_api_context_for_workspace_path(
    workspace: impl AsRef<Path>,
    path: impl AsRef<Path>,
) -> Option<(Dialect, Option<APIContext>)> {
    let path = path.as_ref();
    let basename = path.file_name().and_then(|name| name.to_str())?;
    Some(match basename {
        "BUILD" | "BUILD.bazel" => (Dialect::Bazel, Some(APIContext::Build)),
        "REPO.bazel" => (Dialect::Bazel, Some(APIContext::Repo)),
        "VENDOR.bazel" => (Dialect::Bazel, Some(APIContext::Vendor)),
        "MODULE.bazel" => (Dialect::Bazel, Some(APIContext::Module)),
        path if path.ends_with(".MODULE.bazel") => (Dialect::Bazel, Some(APIContext::Module)),
        "WORKSPACE" | "WORKSPACE.bazel" | "WORKSPACE.bzlmod" => {
            (Dialect::Bazel, Some(APIContext::Workspace))
        }
        path if path.ends_with(".BUILD.bazel") || path.ends_with(".BUILD") => {
            (Dialect::Bazel, Some(APIContext::Build))
        }
        path if path.ends_with(".cquery") || path.ends_with(".query.bzl") => {
            (Dialect::Bazel, Some(APIContext::Cquery))
        }
        _ => match path.extension().and_then(|ext| ext.to_str()) {
            Some("bzl" | "bzli") => (Dialect::Bazel, Some(APIContext::Bzl)),
            _ => {
                if path == workspace.as_ref().join("tools/build_rules/prelude_bazel") {
                    (Dialect::Bazel, Some(APIContext::Prelude))
                } else {
                    (Dialect::Standard, None)
                }
            }
        },
    })
}

#[cfg(test)]
pub(crate) mod source_tests {
    use std::path::Path;
    use std::sync::Arc;

    use ruff_db::system::InMemorySystem;
    use ruff_db::system::SystemPath;
    use ruff_db::system::WritableSystem;
    use starpls_bazel::client::BazelCLI;
    use starpls_common::Dialect;
    use starpls_ide::Analysis;
    use starpls_ide::FilePosition;

    use super::DefaultFileLoader;

    #[derive(Default)]
    pub(crate) struct TestBazelClient {
        pub(crate) retarget: std::sync::Mutex<Option<(std::path::PathBuf, std::path::PathBuf)>>,
        pub(crate) mapping_requests: std::sync::Mutex<Vec<Vec<String>>>,
    }

    impl starpls_bazel::client::BazelClient for TestBazelClient {
        fn build_language(&self) -> anyhow::Result<Vec<u8>> {
            unimplemented!()
        }
        fn info(&self) -> anyhow::Result<starpls_bazel::client::BazelInfo> {
            unimplemented!()
        }
        fn null_query_external_repo_targets(&self, repo: &str) -> anyhow::Result<()> {
            self.fetch_repo(repo)
        }
        fn query_all_workspace_targets(&self) -> anyhow::Result<Vec<String>> {
            unimplemented!()
        }
        fn fetch_repo(&self, repo: &str) -> anyhow::Result<()> {
            if repo == "rules+" {
                if let Some((link, target)) = self.retarget.lock().unwrap().take() {
                    #[cfg(unix)]
                    {
                        std::fs::remove_file(&link)?;
                        std::os::unix::fs::symlink(target, link)?;
                    }
                    #[cfg(not(unix))]
                    unreachable!("Unix fixture: {link:?} {target:?}");
                }
            }
            Ok(())
        }
        fn dump_repo_mappings(
            &self,
            repositories: &[&str],
        ) -> anyhow::Result<Vec<starpls_bazel::client::RepoMapping>> {
            self.mapping_requests.lock().unwrap().push(
                repositories
                    .iter()
                    .map(|repository| (*repository).to_owned())
                    .collect(),
            );
            if repositories.contains(&"fail+") {
                anyhow::bail!("cannot evaluate requested repository batch");
            }
            Ok(repositories
                .iter()
                .map(|from| {
                    Arc::new(
                        [
                            (
                                "dep".to_owned(),
                                if *from == "stubs+" {
                                    "rules+"
                                } else {
                                    "wrong+"
                                }
                                .to_owned(),
                            ),
                            ("stubs".to_owned(), "stubs+".to_owned()),
                            ("rules".to_owned(), "rules+".to_owned()),
                        ]
                        .into(),
                    )
                })
                .collect())
        }
        fn selected_module(
            &self,
            _: &str,
        ) -> anyhow::Result<Option<starpls_bazel::client::SelectedModule>> {
            Ok(None)
        }
    }

    #[test]
    fn deferred_mappings_batch_deduplicate_and_preserve_ready_entries() {
        let client = Arc::new(TestBazelClient::default());
        let (sender, _) = crossbeam_channel::unbounded();
        let loader =
            DefaultFileLoader::new(client.clone(), Default::default(), None, None, sender, true)
                .with_deferred_mappings();
        for repository in ["stubs+", "rules+", "stubs+"] {
            assert!(loader.repository_mapping(repository).unwrap().is_none());
        }
        assert!(client.mapping_requests.lock().unwrap().is_empty());
        let pending = loader.pending_repository_mappings();
        assert_eq!(pending, ["rules+", "stubs+"]);
        loader.resolve_repository_mappings(&pending);
        assert_eq!(
            client.mapping_requests.lock().unwrap().as_slice(),
            &[pending]
        );
        assert!(loader.pending_repository_mappings().is_empty());
        assert_eq!(
            loader.repository_mapping("stubs+").unwrap().unwrap()["dep"],
            "rules+"
        );
        assert_eq!(
            loader.repository_mapping("rules+").unwrap().unwrap()["dep"],
            "wrong+"
        );

        for repository in ["fail+", "other+"] {
            assert!(loader.repository_mapping(repository).unwrap().is_none());
        }
        loader.resolve_repository_mappings(&loader.pending_repository_mappings());
        for repository in ["fail+", "other+"] {
            assert!(loader
                .repository_mapping(repository)
                .unwrap_err()
                .to_string()
                .contains("repository mapping batch failed"));
        }
        assert!(loader.repository_mapping("stubs+").unwrap().is_some());
        assert!(loader.pending_repository_mappings().is_empty());
    }

    #[test]
    fn generated_repository_names_use_bounded_mapping_batches() {
        let client = Arc::new(TestBazelClient::default());
        let (sender, _) = crossbeam_channel::unbounded();
        let loader =
            DefaultFileLoader::new(client.clone(), Default::default(), None, None, sender, true)
                .with_deferred_mappings();
        for index in 0..1000 {
            loader
                .repository_mapping(&format!("npm+{}+{index:04}", "x".repeat(100)))
                .unwrap();
        }
        loop {
            let batch = loader.pending_repository_mappings();
            if batch.is_empty() {
                break;
            }
            assert!(batch.iter().map(|name| name.len() + 1).sum::<usize>() <= 16 * 1024);
            loader.resolve_repository_mappings(&batch);
        }
        let requests = client.mapping_requests.lock().unwrap();
        assert_eq!(requests.iter().map(Vec::len).sum::<usize>(), 1000);
        assert_eq!(requests.len(), 7);
    }

    #[test]
    #[cfg(unix)]
    fn canonical_files_keep_their_repository_context() {
        use starpls_bazel::Label;
        use starpls_ide::FileLoader;

        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("repository-context");
        let workspace = root.join("workspace");
        let external = root.join("external");
        // The override lives inside the main workspace and has a different mapping.
        let stubs = workspace.join("local-stubs");
        for directory in [&stubs, &external.join("rules+"), &external.join("wrong+")] {
            std::fs::create_dir_all(directory).unwrap();
        }
        let text = "load(\"@dep//:defs.bzl\", \"identity\")\nvalue: int\n";
        std::fs::write(stubs.join("defs.bzli"), text).unwrap();
        std::fs::write(external.join("rules+/defs.bzl"), "def identity(): pass\n").unwrap();
        std::fs::write(stubs.join("BUILD"), "").unwrap();
        std::os::unix::fs::symlink(&stubs, external.join("stubs+")).unwrap();
        let (sender, _) = crossbeam_channel::unbounded();
        let loader = Arc::new(DefaultFileLoader::new(
            Arc::new(TestBazelClient::default()),
            workspace.clone(),
            None,
            external.clone(),
            sender,
            true,
        ));
        let mut analysis = Analysis::new(loader.clone(), Default::default()).unwrap();
        let repository = loader
            .resolve_repository(
                &Label::parse("@stubs//:defs.bzli").unwrap(),
                &loader.main_repository(),
            )
            .unwrap();
        let canonical = loader
            .register_path(&external.join("stubs+/defs.bzli"), &repository)
            .unwrap();
        let info = Some(starpls_common::FileInfo::Bazel {
            api_context: starpls_bazel::APIContext::Bzl,
            is_external: false,
        });
        let file = analysis.file(&canonical, Dialect::Bazel, info).unwrap();
        assert_eq!(file.is_external(), Some(true));
        let alias = external.join("stubs+/defs.bzli");
        let opened = analysis
            .open_document(&alias, Dialect::Bazel, info, text.into(), 1)
            .unwrap();
        assert_eq!(file, opened);
        assert_eq!(
            analysis.document(&canonical).unwrap().path.as_std_path(),
            alias
        );
        let snapshot = analysis.snapshot();
        let diagnostics = snapshot.diagnostics(file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        // Inspect resolution through the same implementation used by load queries.
        let repository = loader.repository_for_path(&canonical).unwrap().unwrap();
        assert_eq!(repository.name, "stubs+");
        assert_eq!(
            loader
                .resolve_repository(&Label::parse("@dep//:defs.bzl").unwrap(), &repository)
                .unwrap()
                .root,
            external.join("rules+")
        );
        assert_eq!(
            loader
                .resolve_repository(&Label::parse("@@wrong+//:defs.bzl").unwrap(), &repository)
                .unwrap()
                .root,
            external.join("wrong+")
        );
        assert_eq!(
            loader
                .resolve_repository(&Label::parse("//:defs.bzl").unwrap(), &repository)
                .unwrap()
                .root,
            stubs
        );
        assert_eq!(
            super::package_for_path(&canonical, &repository.root).unwrap(),
            stubs
        );
        let error = loader
            .register_path(&canonical, &loader.main_repository())
            .unwrap_err();
        assert!(
            error.to_string().contains("conflicting Bazel repositories"),
            "{error}"
        );
        assert_eq!(
            loader.repository_for_path(&canonical).unwrap().unwrap(),
            repository
        );
        assert_eq!(
            loader
                .file_info(&canonical, &canonical, Dialect::Bazel, info)
                .unwrap(),
            file.info
        );
        drop(snapshot);
        // An unsaved symlink buffer is also the configured physical stub.
        analysis
            .open_document(&alias, Dialect::Bazel, info, "value: Missing\n".into(), 2)
            .unwrap();
        analysis.sync_files(std::slice::from_ref(&alias)).unwrap();
        let snapshot = analysis.snapshot();
        assert_eq!(snapshot.open_file(&alias).unwrap(), Some(file));
        assert!(!snapshot.diagnostics(file).unwrap().is_empty());
        drop(snapshot);
        assert_eq!(analysis.close_document(&alias).unwrap(), Some(file));
        let diagnostics = analysis.snapshot().diagnostics(file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        // Opening a second explicit repository spelling cannot reuse the first context.
        std::os::unix::fs::symlink(&stubs, external.join("other+")).unwrap();
        let error = analysis
            .file(&external.join("other+/defs.bzli"), Dialect::Bazel, info)
            .unwrap_err();
        assert!(
            error.to_string().contains("conflicting Bazel repositories"),
            "{error}"
        );
        let new_alias = external.join("stubs+/new.bzl");
        let new_physical = stubs.join("new.bzl");
        let new_text = "load(\"@dep//:defs.bzl\", \"identity\")\nvalue = 1\n";
        let new_file = analysis
            .open_document(&new_alias, Dialect::Bazel, info, new_text.into(), 1)
            .unwrap();
        let snapshot = analysis.snapshot();
        assert_eq!(snapshot.path(new_file), new_physical);
        let diagnostics = snapshot.diagnostics(new_file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        drop(snapshot);
        std::fs::write(&new_physical, "value = 0\n").unwrap();
        analysis
            .sync_files(std::slice::from_ref(&new_physical))
            .unwrap();
        assert_eq!(
            analysis.file(&new_physical, Dialect::Bazel, info).unwrap(),
            new_file
        );
        assert_eq!(analysis.document(&new_physical).unwrap().contents, new_text);
        assert_eq!(
            analysis.snapshot().source(new_file).unwrap().text.as_str(),
            new_text
        );
        analysis.update_file(new_file, "value = 2\n".into());
        assert_eq!(
            analysis.document(&new_physical).unwrap().contents,
            "value = 2\n"
        );
        assert_eq!(
            analysis.document(&new_physical).unwrap().path.as_std_path(),
            new_alias
        );
        let fresh = loader.fresh().for_editor(1);
        fresh
            .register_path(&canonical, &fresh.main_repository())
            .unwrap();
        assert!(fresh
            .restore_document_context(&loader, &canonical, &canonical)
            .is_err());
        assert_eq!(fresh.repository_for_path(&canonical).unwrap(), None);
        let unassociated = root.join("unassociated.bzl");
        std::fs::write(&unassociated, "value = 1\n").unwrap();
        assert_eq!(loader.repository_for_path(&unassociated).unwrap(), None);
        fresh
            .restore_document_context(&loader, &unassociated, &unassociated)
            .unwrap();
        assert_eq!(fresh.repository_for_path(&unassociated).unwrap(), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn opening_a_relative_dependency_invalidates_failed_resolution() {
        let disk = InMemorySystem::default();
        let (sender, _) = crossbeam_channel::unbounded();
        let loader = DefaultFileLoader::new(
            Arc::new(BazelCLI::new("bazel")),
            Default::default(),
            None,
            None,
            sender,
            false,
        );
        let mut analysis =
            Analysis::with_system(Arc::new(loader), Default::default(), disk.clone());
        let text = "load(\"dep.star\", \"value\")\nresult = value\n";
        let main = analysis
            .open_document(
                Path::new("/main.star"),
                Dialect::Standard,
                None,
                text.into(),
                1,
            )
            .unwrap();
        let diagnostics = analysis.snapshot().diagnostics(main).unwrap();
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic
                .headline_message()
                .starts_with("Could not resolve module")),
            "{diagnostics:?}"
        );

        disk.write_file(SystemPath::new("/dep.star"), "value = 1\n")
            .unwrap();
        analysis
            .open_document(
                Path::new("/dep.star"),
                Dialect::Standard,
                None,
                "value = 42\n".into(),
                1,
            )
            .unwrap();
        let snapshot = analysis.snapshot();
        let diagnostics = snapshot.diagnostics(main).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let hover = snapshot
            .hover(FilePosition {
                file_id: main,
                pos: (text.rfind("value").unwrap() as u32).into(),
            })
            .unwrap()
            .unwrap();
        assert!(
            hover.contents.value.contains("Literal[42]"),
            "{}",
            hover.contents.value
        );
    }
}
