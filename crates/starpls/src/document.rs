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
use starpls_bazel::client::repository_fetch_batch_len;
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

#[derive(Debug)]
pub(crate) struct RepositoryFetchResult {
    pub(crate) name: String,
    pub(crate) result: Result<(), String>,
}

/// Obtain per-repository results without publishing into a loader's cache.
pub(crate) fn fetch_repositories(
    client: &dyn BazelClient,
    repositories: &[String],
    bzlmod: bool,
    mut progress: impl FnMut(&str),
) -> Vec<RepositoryFetchResult> {
    let mut remaining = repositories;
    let mut results = Vec::with_capacity(repositories.len());
    while !remaining.is_empty() {
        let count = if bzlmod {
            repository_fetch_batch_len(remaining)
        } else {
            1
        };
        let (batch, rest) = remaining.split_at(count);
        remaining = rest;
        progress(&format!("Fetching {} repositories", batch.len()));
        let result = if bzlmod {
            client.fetch_repos(&batch.iter().map(String::as_str).collect::<Vec<_>>())
        } else {
            client.null_query_external_repo_targets(&batch[0])
        };
        if let Err(error) = &result {
            if batch.len() > 1 {
                progress(&format!(
                    "Fetch batch failed: {error:#}; retrying {} repositories individually",
                    batch.len()
                ));
                results.extend(batch.iter().map(|repo| {
                    RepositoryFetchResult {
                        name: repo.clone(),
                        result: client
                            .fetch_repo(repo)
                            .map_err(|error| format!("{error:#}")),
                    }
                }));
                continue;
            }
        }
        let result = result.map_err(|error| format!("{error:#}"));
        results.extend(batch.iter().map(|repo| RepositoryFetchResult {
            name: repo.clone(),
            result: result.clone(),
        }));
    }
    results
}

#[derive(Clone, PartialEq, Eq)]
enum RepositoryFetch {
    Pending,
    Ready,
    Failed(String),
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
    // CLI invocations retain requests across revisions: cached queries may not
    // request their dependencies again after repository work completes.
    load_requests: Option<RwLock<indexmap::IndexSet<(File, String)>>>,
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
            load_requests: None,
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

    pub(crate) fn with_load_recording(mut self) -> Self {
        self.load_requests = Some(Default::default());
        self
    }

    pub(crate) fn recorded_loads(&self) -> Vec<(File, String)> {
        self.load_requests
            .as_ref()
            .map(|requests| requests.read().iter().cloned().collect())
            .unwrap_or_default()
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
        let result = self.bazel_client.dump_repo_mappings(&names);
        let _ = self.finish_repository_mappings(repositories, result);
    }

    /// Call after invalidating load queries and draining their readers.
    pub(crate) fn finish_repository_mappings(
        &self,
        repositories: &[String],
        result: anyhow::Result<Vec<starpls_bazel::client::RepoMapping>>,
    ) -> anyhow::Result<()> {
        let result = result.and_then(|mappings| {
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
                Ok(())
            }
            Err(error) => {
                let message = format!("repository mapping batch failed: {error:#}");
                for repository in repositories {
                    cached.insert(
                        repository.clone(),
                        RepositoryMapping::Failed(message.clone()),
                    );
                }
                Err(error)
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
                if self.configuration_revision.is_some() {
                    entry.insert(RepositoryMapping::Pending);
                    self.fetch_repo_sender.send(Task::ResolveRepoMappings)?;
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
        result: Result<(), String>,
    ) {
        let state = match result {
            Ok(()) => RepositoryFetch::Ready,
            Err(error) => RepositoryFetch::Failed(error),
        };
        self.repository_fetches
            .write()
            .extend(repositories.into_iter().map(|name| (name, state.clone())));
    }

    fn document_context(&self, previous: &Self, path: &Path) -> anyhow::Result<Option<Repository>> {
        let old = match previous.repositories.read().get(path) {
            Some(RepositoryContext::Resolved(repository)) => Some(repository.clone()),
            Some(RepositoryContext::Unknown) => {
                if previous.has_bazel_context() {
                    return Ok(None);
                }
                None
            }
            Some(RepositoryContext::Displaced) => bail!(
                "repository context for open file {} changed; close and reopen it",
                path.display()
            ),
            None => None,
        };
        if !previous.has_bazel_context() {
            if old
                .as_ref()
                .is_some_and(|repository| !repository.name.is_empty())
            {
                return Ok(old);
            }
            return self.repository_for_path(path);
        }
        Ok(old)
    }

    pub(crate) fn restore_document_context(
        &self,
        previous: &Self,
        path: &Path,
        source_validation: anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let admission = source_validation
            .and_then(|()| self.document_context(previous, path))
            .and_then(|repository| self.record_repository(path, repository));
        match admission {
            Ok(()) => Ok(()),
            Err(error) => {
                // Admission precedes publication. A refusal also replaces any
                // prepared context that would reinterpret this open buffer.
                self.repositories
                    .write()
                    .insert(path.to_path_buf(), RepositoryContext::Displaced);
                Err(error)
            }
        }
    }

    pub(crate) fn is_displaced(&self, path: &Path) -> bool {
        self.repositories.read().get(path) == Some(&RepositoryContext::Displaced)
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
    fn label_needs_bazel_context(&self, label: &Label, from: &Repository) -> bool {
        // Before configuration arrives, false does not yet mean legacy mode:
        // even an empty apparent name may require a Bzlmod mapping.
        !from.name.is_empty()
            || !label.repo().is_empty()
            || (label.kind() == RepoKind::Apparent
                && (!self.has_bazel_context() || self.bzlmod_enabled))
    }

    fn resolve_label(
        &self,
        db: &dyn Db,
        label: &Label,
        from: File,
    ) -> anyhow::Result<Option<ResolvedLabel>> {
        let from_path = from.path(db);
        let repository = try_opt!(self.repository_for_path(from_path)?);
        if !self.is_ready() && self.label_needs_bazel_context(label, &repository) {
            return Ok(None);
        }
        self.ensure_repository(&repository)?;
        if self.bzlmod_enabled
            && label.kind() == RepoKind::Apparent
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

    pub(crate) fn is_editable(&self, path: &Path, source: &Path) -> anyhow::Result<bool> {
        let workspace = &self.workspace;
        if !path.starts_with(workspace) {
            return Ok(false);
        }
        let physical_workspace = workspace.canonicalize()?;
        if !source.starts_with(&physical_workspace) {
            return Ok(false);
        }
        for (file, root) in [(path, workspace), (source, &physical_workspace)] {
            if file
                .parent()
                .into_iter()
                .flat_map(Path::ancestors)
                .take_while(|parent| *parent != root)
                .any(is_repository_root)
            {
                return Ok(false);
            }
        }
        Ok(self
            .repository_for_path(path)?
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
        let result = if self.bzlmod_enabled {
            self.bazel_client.fetch_repo(&repository.name)
        } else {
            self.bazel_client
                .null_query_external_repo_targets(&repository.name)
        };
        let outcome = match &result {
            Ok(()) => Ok(()),
            Err(error) => Err(format!("{error:#}")),
        };
        self.finish_fetch([repository.name.clone()], outcome);
        result
    }

    pub(crate) fn fetch_repositories(
        &self,
        repositories: &[String],
        progress: impl FnMut(&str),
    ) -> Vec<RepositoryFetchResult> {
        let results = fetch_repositories(
            &*self.bazel_client,
            repositories,
            self.bzlmod_enabled,
            progress,
        );
        for RepositoryFetchResult { name, result } in &results {
            self.finish_fetch([name.clone()], result.clone());
        }
        results
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
        if !self.is_ready() && self.label_needs_bazel_context(label, from) {
            bail!("Bazel configuration is loading");
        }
        let name = match label.kind() {
            RepoKind::Current => return Ok(from.clone()),
            RepoKind::Canonical => label.repo().to_owned(),
            RepoKind::Apparent => {
                if self.bzlmod_enabled {
                    let mapping = self
                        .repository_mapping(&from.name)?
                        .context("Bazel repository mapping is loading")?;
                    let name = mapping.get(label.repo()).cloned();
                    name.with_context(|| {
                        format!(
                            "cannot resolve repository @{}// from @@{}//",
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
        self.canonical_repository(name)
    }

    pub(crate) fn canonical_repository(&self, name: String) -> anyhow::Result<Repository> {
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

    /// Repository context belongs to the installed path, including symlink mounts.
    pub(crate) fn register_path(
        &self,
        path: &Path,
        repository: &Repository,
    ) -> anyhow::Result<PathBuf> {
        let path = starpls_common::absolute_path(path)?;
        self.record_repository(&path, Some(repository.clone()))?;
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
        let path = starpls_common::absolute_path(path)?;
        let path = path.as_path();
        match self.repositories.read().get(path) {
            Some(RepositoryContext::Unknown) => return Ok(None),
            Some(RepositoryContext::Displaced) => return Ok(None),
            Some(RepositoryContext::Resolved(repository)) => return Ok(Some(repository.clone())),
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
            let nested = path
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
        self.record_repository(path, repository.clone())?;
        Ok(repository)
    }

    fn ensure_repository(&self, repository: &Repository) -> anyhow::Result<()> {
        let fetches = self.repository_fetches.read();
        if let Some(RepositoryFetch::Failed(error)) = fetches.get(&repository.name) {
            // Legacy query --keep_going can materialize the repository despite
            // errors in unrelated packages. The editor still requires a ready
            // repository after configuration changes.
            if !self.bzlmod_enabled
                && self.configuration_revision.is_none()
                && repository.root.is_dir()
            {
                return Ok(());
            }
            bail!("failed to fetch repository @@{}: {error}", repository.name);
        }
        if !repository.name.is_empty()
            && self.configuration_revision.is_some()
            && fetches.get(&repository.name) != Some(&RepositoryFetch::Ready)
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
            if let Some(repository) = &repository {
                self.record_repository(&path, Some(repository.clone()))?;
            }
            let info = self.file_info(&path, dialect, info)?;
            // Ruff records missing inputs so creating a dependency invalidates loads.
            File::from_path(db, &path, dialect, info)
        })();
        if result.is_err() {
            if let Some(Repository {
                name: canonical_repo,
                root: _,
            }) = repository
            {
                if !canonical_repo.is_empty()
                    && self.external_output_base.as_ref().is_some_and(|base| {
                        matches!(base.join(&canonical_repo).try_exists(), Ok(false))
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
        dialect: Dialect,
        info: Option<FileInfo>,
    ) -> anyhow::Result<Option<FileInfo>> {
        if dialect != Dialect::Bazel {
            return Ok(info);
        }
        let repository = self.repository_for_path(path)?;
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
        if let Some(requests) = &self.load_requests {
            requests.write().insert((from, path.to_owned()));
        }
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
                let repository = try_opt!(self.repository_for_path(&from_path)?);
                if !self.is_ready() && self.label_needs_bazel_context(&label, &repository) {
                    return Ok(None);
                }
                self.ensure_repository(&repository)?;
                if self.bzlmod_enabled
                    && label.kind() == RepoKind::Apparent
                    && self.repository_mapping(&repository.name)?.is_none()
                {
                    return Ok(None);
                }

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
        pub(crate) fetch_requests: std::sync::Mutex<Vec<String>>,
        pub(crate) fetch_batches: std::sync::Mutex<Vec<Vec<String>>>,
        pub(crate) fetch_files:
            std::sync::Mutex<std::collections::HashMap<String, (std::path::PathBuf, String)>>,
        pub(crate) fetch_failures: std::sync::Mutex<std::collections::HashMap<String, String>>,
    }

    impl TestBazelClient {
        fn materialize_repository(&self, repo: &str) -> anyhow::Result<()> {
            self.fetch_requests.lock().unwrap().push(repo.to_owned());
            if let Some((path, contents)) = self.fetch_files.lock().unwrap().remove(repo) {
                std::fs::create_dir_all(path.parent().unwrap())?;
                std::fs::write(path, contents)?;
            }
            if let Some(error) = self.fetch_failures.lock().unwrap().get(repo) {
                anyhow::bail!("{error}");
            }
            if repo == "rules+" {
                if let Some((link, target)) = self.retarget.lock().unwrap().take() {
                    #[cfg(unix)]
                    {
                        if let Err(error) = std::fs::remove_file(&link) {
                            if error.kind() != std::io::ErrorKind::NotFound {
                                return Err(error.into());
                            }
                        }
                        std::os::unix::fs::symlink(target, link)?;
                    }
                    #[cfg(not(unix))]
                    unreachable!("Unix fixture: {link:?} {target:?}");
                }
            }
            Ok(())
        }
    }

    #[test]
    fn fetch_batches_bound_arguments_and_skip_empty_requests() {
        let client = TestBazelClient::default();
        assert!(super::fetch_repositories(&client, &[], true, |_| {}).is_empty());
        assert!(client.fetch_batches.lock().unwrap().is_empty());
        let repositories: Vec<_> = (0..600)
            .map(|index| format!("{}+{index}", "long_repo".repeat(120)))
            .collect();
        let results = super::fetch_repositories(&client, &repositories, true, |_| {});
        assert_eq!(results.len(), repositories.len());
        assert!(results.iter().all(|result| result.result.is_ok()));
        let batches = client.fetch_batches.lock().unwrap();
        assert!(batches.len() > 1);
        assert!(batches.iter().all(|batch| batch
            .iter()
            .map(|name| name.len() + "--repo=@@".len() + 1)
            .sum::<usize>()
            <= 256 * 1024));
        assert_eq!(batches.concat(), repositories);
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
        fn fetch_repos(&self, repos: &[&str]) -> anyhow::Result<()> {
            self.fetch_batches
                .lock()
                .unwrap()
                .push(repos.iter().map(|repo| (*repo).to_owned()).collect());
            let mut result = Ok(());
            for repo in repos {
                if let Err(error) = self.materialize_repository(repo) {
                    result = Err(error);
                }
            }
            result
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
        for repository in ["short+", "missing+"] {
            assert!(loader.repository_mapping(repository).unwrap().is_none());
        }
        let pending = loader.pending_repository_mappings();
        assert!(loader
            .finish_repository_mappings(&pending, Ok(vec![Default::default()]))
            .is_err());
        for repository in pending {
            assert!(loader
                .repository_mapping(&repository)
                .unwrap_err()
                .to_string()
                .contains("unexpected result count"));
        }
        assert!(loader.repository_mapping("stubs+").unwrap().is_some());
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
    fn empty_apparent_repositories_follow_the_callers_mapping() {
        use starpls_bazel::Label;
        use starpls_ide::LoadDependency;
        use starpls_ide::LoadResolution;
        use starpls_ide::LocationLink;

        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("empty-apparent-repository");
        let workspace = root.join("workspace");
        let external = root.join("external");
        let current = workspace.join("local-override");
        let mapped = external.join("mapped+");
        for directory in [&workspace, &external, &current, &mapped] {
            std::fs::create_dir_all(directory).unwrap();
        }
        for (directory, marker) in [
            (&workspace, "main.bzl"),
            (&current, "current.bzl"),
            (&mapped, "mapped.bzl"),
        ] {
            std::fs::write(directory.join("same.bzl"), "value = 1\n").unwrap();
            std::fs::write(directory.join(marker), "").unwrap();
        }
        std::os::unix::fs::symlink(&current, external.join("current+")).unwrap();
        let source_path = external.join("current+/source.bzl");
        let client = Arc::new(TestBazelClient::default());
        for bzlmod in [true, false] {
            let (sender, _) = crossbeam_channel::unbounded();
            let loader = Arc::new(DefaultFileLoader::new(
                client.clone(),
                workspace.clone(),
                None,
                external.clone(),
                sender,
                bzlmod,
            ));
            let mut analysis = Analysis::new(loader.clone(), Default::default()).unwrap();
            // Test absent, main, and external empty-name mappings. Even a local
            // override retains its installed path and caller context.
            for mapping in [None, Some(""), Some("mapped+")] {
                analysis.invalidate_loads();
                loader.finish_mapping(
                    "current+".to_owned(),
                    Ok(Arc::new(
                        mapping
                            .map(|name| (String::new(), name.to_owned()))
                            .into_iter()
                            .collect(),
                    )),
                );
                let apparent = if !bzlmod {
                    Some((&workspace, "main.bzl"))
                } else {
                    mapping.map(|name| {
                        if name.is_empty() {
                            (&workspace, "main.bzl")
                        } else {
                            (&mapped, "mapped.bzl")
                        }
                    })
                };
                for (label, expected) in [
                    (
                        "//:same.bzl",
                        Some((&external.join("current+"), "current.bzl")),
                    ),
                    ("@@//:same.bzl", Some((&workspace, "main.bzl"))),
                    ("@//:same.bzl", apparent),
                ] {
                    let text = format!(
                        "load(\"{label}\", \"value\")\nreference = \"{label}\"\nresult = value\n"
                    );
                    let file = analysis
                        .open_document(&source_path, Dialect::Bazel, None, text.clone(), 1)
                        .unwrap();
                    let snapshot = analysis.snapshot();
                    assert_eq!(snapshot.path(file), source_path);
                    let dependencies = snapshot.load_dependencies(file).unwrap();
                    let [LoadDependency {
                        module: _,
                        range: _,
                        resolution,
                    }] = dependencies.as_slice()
                    else {
                        panic!("{dependencies:?}");
                    };
                    let navigation = snapshot
                        .goto_definition(
                            FilePosition {
                                file_id: file,
                                pos: (text.rfind(label).unwrap() as u32 + 1).into(),
                            },
                            false,
                        )
                        .unwrap()
                        .unwrap_or_default();
                    let completions = snapshot
                        .completions(
                            FilePosition {
                                file_id: file,
                                pos: (text.find(':').unwrap() as u32 + 1).into(),
                            },
                            None,
                        )
                        .unwrap()
                        .unwrap_or_default();
                    match expected {
                        Some((directory, marker)) => {
                            let LoadResolution::Resolved(target) = resolution else {
                                panic!("{dependencies:?}")
                            };
                            assert_eq!(snapshot.path(*target), directory.join("same.bzl"));
                            let [LocationLink::External {
                                origin_selection_range: _,
                                target_path,
                            }] = navigation.as_slice()
                            else {
                                panic!("{navigation:?}")
                            };
                            assert_eq!(*target_path, directory.join("same.bzl"));
                            let markers: Vec<_> = completions
                                .iter()
                                .map(|item| item.label.as_str())
                                .filter(|label| {
                                    ["main.bzl", "current.bzl", "mapped.bzl"].contains(label)
                                })
                                .collect();
                            assert_eq!(markers, [marker]);
                        }
                        None => {
                            let LoadResolution::Failed(error) = resolution else {
                                panic!("{dependencies:?}")
                            };
                            assert!(
                                error.contains("cannot resolve repository @// from @@current+//"),
                                "{error}"
                            );
                            assert!(navigation.is_empty(), "{navigation:?}");
                            assert!(completions.is_empty(), "{completions:?}");
                        }
                    }
                }
            }
            // Root-module @// is also an explicit mapping entry.
            loader.finish_mapping(
                String::new(),
                Ok(Arc::new([(String::new(), String::new())].into())),
            );
            assert_eq!(
                loader
                    .resolve_repository(
                        &Label::parse("@//:same.bzl").unwrap(),
                        &loader.main_repository()
                    )
                    .unwrap(),
                loader.main_repository()
            );
        }
        assert!(client.mapping_requests.lock().unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_apparent_repositories_wait_for_configuration_and_mapping() {
        use starpls_bazel::Label;
        use starpls_ide::LoadDependency;
        use starpls_ide::LoadResolution;

        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("pending-empty-repository");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("same.bzl"), "value = 1\n").unwrap();
        let client = Arc::new(TestBazelClient::default());
        // The initial editor loader has no context and does not yet know whether
        // Bzlmod is enabled. False here must not imply confirmed legacy semantics.
        for context in [None, Some(root.join("external"))] {
            let configured = context.is_some();
            let (sender, receiver) = crossbeam_channel::unbounded();
            let loader = Arc::new(
                DefaultFileLoader::new(
                    client.clone(),
                    workspace.clone(),
                    None,
                    context,
                    sender,
                    configured,
                )
                .for_editor(1),
            );
            let mut analysis = Analysis::new(loader.clone(), Default::default()).unwrap();
            let text =
                "load(\"@//:same.bzl\", \"value\")\nreference = \"@//:same.bzl\"\nresult = value\n";
            let file = analysis
                .open_document(
                    &workspace.join("source.bzl"),
                    Dialect::Bazel,
                    None,
                    text.into(),
                    1,
                )
                .unwrap();
            for paused in [true, false] {
                loader.pause(paused);
                analysis.invalidate_loads();
                let snapshot = analysis.snapshot();
                let dependencies = snapshot.load_dependencies(file).unwrap();
                assert!(
                    matches!(
                        dependencies.as_slice(),
                        [LoadDependency {
                            module: _,
                            range: _,
                            resolution: LoadResolution::Pending
                        }]
                    ),
                    "{dependencies:?}"
                );
                assert!(snapshot
                    .goto_definition(
                        FilePosition {
                            file_id: file,
                            pos: (text.rfind("@//").unwrap() as u32 + 1).into()
                        },
                        false
                    )
                    .unwrap()
                    .unwrap_or_default()
                    .is_empty());
                assert!(snapshot
                    .completions(
                        FilePosition {
                            file_id: file,
                            pos: (text.find(':').unwrap() as u32 + 1).into()
                        },
                        None
                    )
                    .unwrap()
                    .unwrap_or_default()
                    .is_empty());
                for spelling in ["//:same.bzl", "@@//:same.bzl"] {
                    assert_eq!(
                        loader
                            .resolve_repository(
                                &Label::parse(spelling).unwrap(),
                                &loader.main_repository()
                            )
                            .unwrap(),
                        loader.main_repository()
                    );
                }
            }
            assert!(client.mapping_requests.lock().unwrap().is_empty());
            if configured {
                assert_eq!(loader.pending_repository_mappings(), [""]);
                assert!(matches!(
                    receiver.try_recv().unwrap(),
                    crate::event_loop::Task::ResolveRepoMappings
                ));
                assert!(receiver.try_recv().is_err());
                analysis.invalidate_loads();
                loader.finish_mapping(
                    String::new(),
                    Ok(Arc::new([(String::new(), String::new())].into())),
                );
                let snapshot = analysis.snapshot();
                let dependencies = snapshot.load_dependencies(file).unwrap();
                let [LoadDependency {
                    module: _,
                    range: _,
                    resolution: LoadResolution::Resolved(target),
                }] = dependencies.as_slice()
                else {
                    panic!("{dependencies:?}")
                };
                assert_eq!(snapshot.path(*target), workspace.join("same.bzl"));
                assert!(snapshot.diagnostics(file).unwrap().is_empty());
            } else {
                assert!(loader.pending_repository_mappings().is_empty());
                assert!(receiver.try_recv().is_err());
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn overlays_share_contents_and_keep_installed_packages() {
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("overlay-packages");
        let workspace = root.join("workspace");
        let external = root.join("external");
        let backing = workspace.join("overlay");
        let packages = [
            external.join("a+/one"),
            external.join("a+/two"),
            external.join("b+"),
        ];
        std::fs::create_dir_all(&backing).unwrap();
        let text = "load(\":dep.bzl\", \"value\")\nload(\"//:root.bzl\", \"root\")\nresult = value\nroot_result = root\n";
        let physical = backing.join("defs.bzl");
        std::fs::write(&physical, text).unwrap();
        for (package, value) in packages.iter().zip(["1", "'two'", "False"]) {
            std::fs::create_dir_all(package).unwrap();
            std::fs::write(package.join("BUILD"), "").unwrap();
            std::fs::write(package.join("dep.bzl"), format!("value = {value}\n")).unwrap();
            // Include symlinked directories as well as individual overlay files.
            std::os::unix::fs::symlink(&backing, package.join("sub")).unwrap();
            std::os::unix::fs::symlink(&physical, package.join("defs.bzl")).unwrap();
        }
        for (repository, value) in [("a+", "100"), ("b+", "'root'")] {
            std::fs::write(
                external.join(repository).join("root.bzl"),
                format!("root = {value}\n"),
            )
            .unwrap();
        }
        let (sender, _) = crossbeam_channel::unbounded();
        let loader = Arc::new(DefaultFileLoader::new(
            Arc::new(TestBazelClient::default()),
            workspace,
            None,
            external,
            sender,
            false,
        ));
        let mut analysis = Analysis::new(loader, Default::default()).unwrap();
        let info = Some(starpls_common::FileInfo::Bazel {
            api_context: starpls_bazel::APIContext::Bzl,
            is_external: true,
        });
        let paths = [
            packages[0].join("defs.bzl"),
            packages[1].join("sub/defs.bzl"),
            packages[2].join("defs.bzl"),
        ];
        let files: Vec<_> = paths
            .iter()
            .map(|path| analysis.file(path, Dialect::Bazel, info).unwrap())
            .collect();
        let snapshot = analysis.snapshot();
        for ((file, package), expected) in
            files
                .iter()
                .zip(&packages)
                .zip(["Literal[1]", "Literal[\"two\"]", "Literal[False]"])
        {
            let diagnostics = snapshot.diagnostics(*file).unwrap();
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
            let hover = snapshot
                .hover(FilePosition {
                    file_id: *file,
                    pos: (text.rfind("value").unwrap() as u32).into(),
                })
                .unwrap()
                .unwrap();
            assert!(
                hover.contents.value.contains(expected),
                "{}",
                hover.contents.value
            );
            let root_hover = snapshot
                .hover(FilePosition {
                    file_id: *file,
                    pos: (text.rfind("root").unwrap() as u32).into(),
                })
                .unwrap()
                .unwrap();
            let expected_root = if package == &packages[2] {
                "Literal[\"root\"]"
            } else {
                "Literal[100]"
            };
            assert!(
                root_hover.contents.value.contains(expected_root),
                "{}",
                root_hover.contents.value
            );
            let navigation = snapshot
                .goto_definition(
                    FilePosition {
                        file_id: *file,
                        pos: (text.find(":dep").unwrap() as u32 + 1).into(),
                    },
                    false,
                )
                .unwrap()
                .unwrap();
            let [starpls_ide::LocationLink::Local {
                origin_selection_range: _,
                target_range: _,
                target_selection_range: _,
                target_file_id,
            }] = navigation.as_slice()
            else {
                panic!("{navigation:?}")
            };
            assert_eq!(snapshot.path(*target_file_id), package.join("dep.bzl"));
            let completions = snapshot
                .completions(
                    FilePosition {
                        file_id: *file,
                        pos: (text.find(":dep").unwrap() as u32 + 1).into(),
                    },
                    None,
                )
                .unwrap()
                .unwrap();
            assert!(
                completions.iter().any(|item| item.label == "dep.bzl"),
                "{completions:?}"
            );
        }
        drop(snapshot);
        // Opening the backing source refreshes every admitted semantic file.
        for (version, contents, errors) in [
            (1, "bad: int = 'wrong'\n", true),
            (2, "value = 42\n", false),
        ] {
            analysis
                .open_document(&physical, Dialect::Bazel, info, contents.into(), version)
                .unwrap();
            let snapshot = analysis.snapshot();
            for file in &files {
                assert_eq!(snapshot.source(*file).unwrap().text.as_str(), contents);
                let diagnostics = snapshot.diagnostics(*file).unwrap();
                assert_eq!(!diagnostics.is_empty(), errors, "{diagnostics:?}");
            }
        }
        analysis.close_document(&physical).unwrap();
        for file in &files {
            assert_eq!(
                analysis.snapshot().source(*file).unwrap().text.as_str(),
                text
            );
        }
        std::fs::write(&physical, "value = 43\n").unwrap();
        analysis
            .sync_files(std::slice::from_ref(&paths[0]))
            .unwrap();
        for file in &files {
            assert_eq!(
                analysis.snapshot().source(*file).unwrap().text.as_str(),
                "value = 43\n"
            );
        }
        // Archive members can have different bytes and identical timestamps.
        let replacement = backing.join("replacement.bzl");
        std::fs::write(&replacement, "value = 44\n").unwrap();
        let modified = std::fs::metadata(&physical).unwrap().modified().unwrap();
        std::fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        std::fs::remove_file(&paths[0]).unwrap();
        std::os::unix::fs::symlink(&replacement, &paths[0]).unwrap();
        analysis
            .sync_files(std::slice::from_ref(&paths[0]))
            .unwrap();
        assert_eq!(
            analysis.snapshot().source(files[0]).unwrap().text.as_str(),
            "value = 44\n"
        );
        // Retargeted aliases no longer follow edits to the old backing source.
        analysis
            .open_document(&physical, Dialect::Bazel, info, "value = 45\n".into(), 3)
            .unwrap();
        std::fs::remove_file(&paths[2]).unwrap();
        std::os::unix::fs::symlink(&replacement, &paths[2]).unwrap();
        analysis
            .sync_files(std::slice::from_ref(&paths[2]))
            .unwrap();
        analysis.validate_document(&physical).unwrap();
        assert_eq!(
            analysis.snapshot().source(files[0]).unwrap().text.as_str(),
            "value = 44\n"
        );
        assert_eq!(
            analysis.snapshot().source(files[1]).unwrap().text.as_str(),
            "value = 45\n"
        );
        assert_eq!(
            analysis.snapshot().source(files[2]).unwrap().text.as_str(),
            "value = 44\n"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn logical_files_keep_their_repository_context() {
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
        let installed = loader
            .register_path(&external.join("stubs+/defs.bzli"), &repository)
            .unwrap();
        let info = Some(starpls_common::FileInfo::Bazel {
            api_context: starpls_bazel::APIContext::Bzl,
            is_external: false,
        });
        let file = analysis.file(&installed, Dialect::Bazel, info).unwrap();
        assert_eq!(file.is_external(), Some(true));
        let alias = external.join("stubs+/defs.bzli");
        let opened = analysis
            .open_document(&alias, Dialect::Bazel, info, text.into(), 1)
            .unwrap();
        assert_eq!(file, opened);
        assert_eq!(
            analysis.document(&installed).unwrap().path.as_std_path(),
            alias
        );
        let snapshot = analysis.snapshot();
        let diagnostics = snapshot.diagnostics(file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        // Inspect resolution through the same implementation used by load queries.
        let repository = loader.repository_for_path(&installed).unwrap().unwrap();
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
            external.join("stubs+")
        );
        assert_eq!(
            super::package_for_path(&installed, &repository.root).unwrap(),
            external.join("stubs+")
        );
        let error = loader
            .register_path(&installed, &loader.main_repository())
            .unwrap_err();
        assert!(
            error.to_string().contains("conflicting Bazel repositories"),
            "{error}"
        );
        assert_eq!(
            loader.repository_for_path(&installed).unwrap().unwrap(),
            repository
        );
        assert_eq!(
            loader.file_info(&installed, Dialect::Bazel, info).unwrap(),
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
        // Another installation shares bytes but owns its repository context.
        std::os::unix::fs::symlink(&stubs, external.join("other+")).unwrap();
        let other = analysis
            .file(&external.join("other+/defs.bzli"), Dialect::Bazel, info)
            .unwrap();
        assert_ne!(other.source, file.source);
        assert_eq!(
            loader
                .repository_for_path(&external.join("other+/defs.bzli"))
                .unwrap()
                .unwrap()
                .name,
            "other+"
        );
        let new_alias = external.join("stubs+/new.bzl");
        let new_physical = stubs.join("new.bzl");
        let new_text = "load(\"@dep//:defs.bzl\", \"identity\")\nvalue = 1\n";
        let new_file = analysis
            .open_document(&new_alias, Dialect::Bazel, info, new_text.into(), 1)
            .unwrap();
        let snapshot = analysis.snapshot();
        assert_eq!(snapshot.path(new_file), new_alias);
        let diagnostics = snapshot.diagnostics(new_file).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        drop(snapshot);
        std::fs::write(&new_physical, "value = 0\n").unwrap();
        analysis
            .sync_files(std::slice::from_ref(&new_physical))
            .unwrap();
        let physical_file = analysis.file(&new_physical, Dialect::Bazel, info).unwrap();
        assert_ne!(physical_file.source, new_file.source);
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
            .register_path(&installed, &fresh.main_repository())
            .unwrap();
        assert!(fresh
            .restore_document_context(&loader, &installed, Ok(()))
            .is_err());
        assert_eq!(fresh.repository_for_path(&installed).unwrap(), None);
        let unassociated = root.join("unassociated.bzl");
        std::fs::write(&unassociated, "value = 1\n").unwrap();
        assert_eq!(loader.repository_for_path(&unassociated).unwrap(), None);
        fresh
            .restore_document_context(&loader, &unassociated, Ok(()))
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
