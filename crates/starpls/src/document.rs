use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::path::MAIN_SEPARATOR;
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

pub(crate) struct DefaultFileLoader {
    bazel_client: Arc<dyn BazelClient>,
    workspace: PathBuf,
    workspace_name: Option<String>,
    external_output_base: PathBuf,
    fetch_repo_sender: Sender<Task>,
    bzlmod_enabled: bool,
    repositories: RwLock<HashMap<PathBuf, Option<Repository>>>,
}

impl DefaultFileLoader {
    pub(crate) fn new(
        bazel_client: Arc<dyn BazelClient>,
        workspace: PathBuf,
        workspace_name: Option<String>,
        external_output_base: PathBuf,
        fetch_repo_sender: Sender<Task>,
        bzlmod_enabled: bool,
    ) -> Self {
        Self {
            bazel_client,
            workspace,
            workspace_name,
            external_output_base,
            fetch_repo_sender,
            bzlmod_enabled,
            repositories: Default::default(),
        }
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

    pub(crate) fn main_repository(&self) -> Repository {
        Repository {
            name: String::new(),
            root: self.workspace.clone(),
        }
    }

    pub(crate) fn resolve_repository(
        &self,
        label: &Label,
        from: &Repository,
    ) -> anyhow::Result<Repository> {
        let name = match label.kind() {
            RepoKind::Current => return Ok(from.clone()),
            RepoKind::Canonical => label.repo().to_owned(),
            RepoKind::Apparent => {
                if label.repo().is_empty() {
                    String::new()
                } else if self.bzlmod_enabled {
                    let name = self
                        .bazel_client
                        .resolve_repo_from_mapping(label.repo(), &from.name)?;
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
            self.external_output_base.join(&name)
        };
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
        if !path.starts_with(&self.external_output_base) {
            if let Some(repository) = self.repositories.read().get(canonical) {
                return Ok(repository.clone());
            }
        }
        let repository = if let Ok(relative) = path.strip_prefix(&self.external_output_base) {
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
                root: self.external_output_base.join(name),
            })
        } else if path.starts_with(&self.workspace)
            || path.extension().is_some_and(|ext| ext == "bzli")
        {
            Some(self.main_repository())
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

    fn read_file(
        &self,
        db: &dyn Db,
        path: PathBuf,
        dialect: Dialect,
        info: Option<FileInfo>,
        repository: Option<Repository>,
    ) -> anyhow::Result<File> {
        let result = (|| {
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
                    && !self
                        .external_output_base
                        .join(&canonical_repo)
                        .try_exists()
                        .unwrap_or(false)
                {
                    let _ = self.fetch_repo_sender.send(Task::FetchExternalRepoRequest(
                        FetchExternalRepoRequest {
                            repo: canonical_repo,
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

                if !label.has_leading_slashes()
                    && !label.is_relative()
                    && err != Some(ParseError::InvalidRepo)
                {
                    if label.kind() == RepoKind::Apparent && self.bzlmod_enabled {
                        let names = self.bazel_client.repo_mapping_keys(&repository.name)?;
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
                            fs::read_dir(&self.external_output_base)?
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
mod source_tests {
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
    pub(crate) struct TestBazelClient;

    impl starpls_bazel::client::BazelClient for TestBazelClient {
        fn build_language(&self) -> anyhow::Result<Vec<u8>> {
            unimplemented!()
        }
        fn info(&self) -> anyhow::Result<starpls_bazel::client::BazelInfo> {
            unimplemented!()
        }
        fn resolve_repo_from_mapping(
            &self,
            apparent: &str,
            from: &str,
        ) -> anyhow::Result<Option<String>> {
            Ok(Some(
                match apparent {
                    "dep" => {
                        if from == "stubs+" {
                            "rules+"
                        } else {
                            "wrong+"
                        }
                    }
                    "stubs" => "stubs+",
                    "rules" => "rules+",
                    _ => return Ok(None),
                }
                .to_owned(),
            ))
        }
        fn clear_repo_mappings(&self) {}
        fn null_query_external_repo_targets(&self, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn repo_mapping_keys(&self, from: &str) -> anyhow::Result<Vec<String>> {
            Ok(vec![from.to_owned()])
        }
        fn query_all_workspace_targets(&self) -> anyhow::Result<Vec<String>> {
            unimplemented!()
        }
        fn fetch_repo(&self, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn dump_repo_mapping(
            &self,
            _: &str,
        ) -> anyhow::Result<std::collections::HashMap<String, String>> {
            unimplemented!()
        }
        fn selected_module(
            &self,
            _: &str,
        ) -> anyhow::Result<Option<starpls_bazel::client::SelectedModule>> {
            unimplemented!()
        }
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
            Arc::new(TestBazelClient),
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
            Default::default(),
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
