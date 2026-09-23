use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use anyhow::bail;
use anyhow::Context;
use serde::Deserialize;
use starpls_bazel::Label;

use crate::document::DefaultFileLoader;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct ProjectConfig {
    #[serde(default)]
    stub_packages: Vec<StubPackage>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct StubPackage {
    manifest: String,
    #[serde(default)]
    allow_unversioned: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct Manifest {
    format_version: u32,
    source: Source,
    files: BTreeMap<PathBuf, PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    repository: String,
    module: String,
    versions: Vec<String>,
}

#[derive(Debug)]
pub(super) struct Registration {
    pub(super) source: PathBuf,
    pub(super) interface: PathBuf,
    pub(super) origin: String,
}

pub(super) fn load(
    loader: &DefaultFileLoader,
    workspace: &Path,
) -> anyhow::Result<Vec<Registration>> {
    let path = workspace.join("starpls.toml");
    loader.watch_manifest(&path);
    let contents = match loader.read_manifest(&path) {
        Ok(contents) => contents,
        Err(error) => {
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(Vec::new());
            }
            return Err(error).with_context(|| format!("cannot read {}", path.display()));
        }
    };
    let ProjectConfig { stub_packages } = toml::from_str(&contents)
        .with_context(|| format!("invalid configuration {}", path.display()))?;
    let mut files = Vec::new();
    for package in stub_packages {
        let manifest = package.manifest.clone();
        let registrations = load_package(loader, workspace, package)
            .with_context(|| format!("stub package {manifest:?} in {}", path.display()))?;
        files.extend(registrations);
    }
    Ok(files)
}

fn load_package(
    loader: &DefaultFileLoader,
    workspace: &Path,
    package: StubPackage,
) -> anyhow::Result<Vec<Registration>> {
    let StubPackage {
        manifest,
        allow_unversioned,
    } = package;
    let main = loader.main_repository();
    let (path, repository) =
        if manifest.starts_with('@') || manifest.starts_with("//") || manifest.starts_with(':') {
            let label = Label::parse(&manifest)
                .map_err(|error| error.err)
                .context("invalid manifest label")?;
            let repository = loader.resolve_repository(&label, &main)?;
            loader.fetch_repository(&repository)?;
            let path = repository.root.join(label.package()).join(label.target());
            (path, repository)
        } else {
            if Path::new(&manifest).is_absolute() {
                bail!("manifest path must be relative to starpls.toml");
            }
            (workspace.join(&manifest), main)
        };
    loader.watch_manifest(&path);
    let path = contained_file(&path, &repository.root)?;
    loader.watch_manifest(&path);
    let contents = loader
        .read_manifest(&path)
        .with_context(|| format!("cannot read manifest {}", path.display()))?;
    let Manifest {
        format_version,
        source,
        files,
    } = toml::from_str(&contents)
        .with_context(|| format!("invalid manifest {}", path.display()))?;
    if format_version != 1 {
        bail!("unsupported stub manifest format-version {format_version}; expected 1");
    }
    let Source {
        repository: source_repository,
        module,
        versions,
    } = source;
    if module.is_empty() || versions.is_empty() || versions.iter().any(String::is_empty) {
        bail!("source.module and source.versions must contain nonempty names and versions");
    }
    if !source_repository.starts_with('@') || source_repository.contains(['/', ':']) {
        bail!("source.repository must name a Bazel repository, such as @rules_foo");
    }
    let source_label = format!("{source_repository}//:__stub_source__");
    let source_label = Label::parse(&source_label)
        .map_err(|error| error.err)
        .context("invalid source.repository")?;
    let source_repository = loader.resolve_repository(&source_label, &repository)?;
    let selected = loader.selected_module(&source_repository)?;
    if let Some(selected) = &selected {
        if selected.name != module {
            bail!(
                "repository @@{} selects module {:?}, but the stub package requires {:?}",
                source_repository.name,
                selected.name,
                module
            );
        }
    }
    match selected.and_then(|module| module.version) {
        Some(version) => {
            if !versions.contains(&version) {
                bail!(
                    "module {module} selected version {version}, but the stub package accepts {}",
                    versions.join(", ")
                );
            }
        }
        None => {
            if !allow_unversioned {
                bail!("repository @@{} has no selected module version; set allow-unversioned = true for this stub package to accept it", source_repository.name);
            }
        }
    }
    loader.fetch_repository(&source_repository)?;
    let directory = path.parent().context("manifest has no parent directory")?;
    let mut registrations = Vec::with_capacity(files.len());
    for (source, interface) in files {
        if source.is_absolute() || interface.is_absolute() {
            bail!("stub file mappings must use relative paths");
        }
        let origin = format!("{} [{:?}]", path.display(), source);
        let source = contained_file(
            &source_repository.root.join(source),
            &source_repository.root,
        )?;
        let interface = contained_file(&directory.join(interface), &repository.root)?;
        let source = loader.register_path(&source, &source_repository)?;
        let interface = loader.register_path(&interface, &repository)?;
        registrations.push(Registration {
            source,
            interface,
            origin,
        });
    }
    Ok(registrations)
}

fn contained_file(path: &Path, root: &Path) -> anyhow::Result<PathBuf> {
    let path = starpls_common::absolute_path(path)?;
    let root = starpls_common::absolute_path(root)?;
    if !path.starts_with(&root) {
        bail!(
            "{} is outside repository {}",
            path.display(),
            root.display()
        );
    }
    let metadata = path
        .metadata()
        .with_context(|| format!("cannot resolve {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{} must be a file", path.display());
    }
    Ok(path)
}
