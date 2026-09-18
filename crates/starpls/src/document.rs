use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::path::MAIN_SEPARATOR;
use std::sync::Arc;

use anyhow::anyhow;
use anyhow::bail;
use crossbeam_channel::Sender;
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
        }
    }
}

struct ResolvedLabel {
    resolved_path: PathBuf,
    canonical_repo: Option<String>,
}

impl DefaultFileLoader {
    fn resolve_label(
        &self,
        db: &dyn Db,
        label: &Label,
        from: File,
    ) -> anyhow::Result<Option<ResolvedLabel>> {
        let repo_kind = label.kind();
        let mut canonical_repo_res = None;
        let (root, package) = match &repo_kind {
            RepoKind::Apparent if self.bzlmod_enabled => {
                let from_path = from.path(db).to_path_buf();
                let from_repo = try_opt!(self.repo_for_path(&from_path));
                let canonical_repo = self
                    .bazel_client
                    .resolve_repo_from_mapping(label.repo(), from_repo)?;
                match canonical_repo {
                    Some(canonical_repo) => (
                        if canonical_repo.is_empty() {
                            self.workspace.clone()
                        } else {
                            canonical_repo_res = Some(canonical_repo.clone());
                            self.external_output_base.join(canonical_repo)
                        },
                        PathBuf::new(),
                    ),
                    None => {
                        bail!(
                            "Could not resolve repository \"{}{}\" from current repository mapping",
                            match label.kind() {
                                RepoKind::Canonical => "@@",
                                _ => "@",
                            },
                            label.repo()
                        )
                    }
                }
            }
            RepoKind::Canonical | RepoKind::Apparent => {
                if !label.repo().is_empty() {
                    canonical_repo_res = Some(label.repo().to_string());
                }

                if self.workspace_name.as_deref() == Some(label.repo()) || label.repo().is_empty() {
                    (self.workspace.clone(), PathBuf::new())
                } else {
                    (self.external_output_base.join(label.repo()), PathBuf::new())
                }
            }
            RepoKind::Current => {
                // Find the Bazel workspace root.
                let from_path = from.path(db).to_path_buf();
                match starpls_bazel::resolve_workspace(from_path)? {
                    Some(root) => root,
                    None => {
                        bail!("not in a Bazel workspace")
                    }
                }
            }
        };

        // Loading targets using a relative label causes them to be resolved from the closest package to the importing file.
        let resolved_path = if label.is_relative() {
            package
        } else {
            root.join(label.package())
        }
        .join(label.target());

        Ok(Some(ResolvedLabel {
            resolved_path,
            canonical_repo: canonical_repo_res,
        }))
    }

    fn read_file(
        &self,
        db: &dyn Db,
        path: PathBuf,
        dialect: Dialect,
        info: Option<FileInfo>,
        fetch_repo_on_err: Option<String>,
    ) -> anyhow::Result<File> {
        match File::from_path(db, &path, dialect, info) {
            Ok(file) => Ok(file),
            Err(error) => {
                if let Some(canonical_repo) = fetch_repo_on_err {
                    if !self
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
                Err(error)
            }
        }
    }

    fn repo_for_path<'a>(&'a self, path: &'a Path) -> Option<&'a str> {
        match path.strip_prefix(&self.external_output_base) {
            Ok(stripped) => stripped
                .components()
                .next()
                .as_ref()
                .and_then(|component| component.as_os_str().to_str()),
            Err(_) => {
                if path.starts_with(&self.workspace) {
                    Some("")
                } else {
                    None
                }
            }
        }
    }
}

impl FileLoader for DefaultFileLoader {
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
                resolved_label.canonical_repo,
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
        let (path, info, canonical_repo) = match dialect {
            Dialect::Standard => {
                // Find the importing file's directory.
                let mut from_path = from.path(db).to_path_buf();
                assert!(from_path.pop());

                // Resolve the given path relative to the importing file's directory.
                let candidate = from_path.join(path);
                // Track missing paths before canonicalization can fail outside Salsa.
                File::from_path(db, &candidate, dialect, None)?;
                let candidate = starpls_common::system_path(&candidate)?;
                let canonical = db.system().canonicalize_path(candidate)?;
                (canonical.as_std_path().to_path_buf(), None, None)
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
                    canonical_repo,
                } = try_opt!(self.resolve_label(db, &label, from)?);

                let is_external = !resolved_path.starts_with(&self.workspace);
                (
                    resolved_path,
                    Some(FileInfo::Bazel {
                        api_context: APIContext::Bzl,
                        is_external,
                    }),
                    canonical_repo,
                )
            }
        };

        let file = self.read_file(db, path, dialect, info, canonical_repo)?;
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
                // Determine the loading file's workspace root and package.
                let (mut root, package) =
                    try_opt!(starpls_bazel::resolve_workspace(from.path(db),)?);
                let (label, err) = match Label::parse(path) {
                    Ok(label) => (label, None),
                    Err(PartialParse { partial, err }) => (partial, Some(err)),
                };

                if !label.has_leading_slashes()
                    && !label.is_relative()
                    && err != Some(ParseError::InvalidRepo)
                {
                    return Ok(match label.kind() {
                        RepoKind::Apparent if self.bzlmod_enabled => Some(
                            self.bazel_client
                                .repo_mapping_keys("")?
                                .into_iter()
                                .map(|repo| LoadItemCandidate {
                                    kind: LoadItemCandidateKind::Directory,
                                    path: repo.to_string(),
                                    replace_trailing_slash: false,
                                })
                                .collect(),
                        ),
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

                match label.kind() {
                    RepoKind::Apparent | RepoKind::Canonical => {
                        root = if self.bzlmod_enabled {
                            let from_repo = try_opt!(self.repo_for_path(&from_path));
                            let canonical_repo = try_opt!(self
                                .bazel_client
                                .resolve_repo_from_mapping(label.repo(), from_repo)?);
                            if canonical_repo.is_empty() {
                                self.workspace.clone()
                            } else {
                                self.external_output_base.join(canonical_repo)
                            }
                        } else if self.workspace_name.as_deref() == Some(label.repo())
                            || label.repo().is_empty()
                        {
                            self.workspace.clone()
                        } else {
                            self.external_output_base.join(label.repo())
                        };
                    }
                    RepoKind::Current => {}
                }

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
            Some("bzl") => (Dialect::Bazel, Some(APIContext::Bzl)),
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
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.starts_with("Could not resolve module")),
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
