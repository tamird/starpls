use std::collections::BTreeMap;
use std::collections::HashSet;
use std::io::BufRead;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use annotate_snippets::Level;
use annotate_snippets::Renderer;
use anyhow::anyhow;
use anyhow::Context;
use clap::Args;
use ruff_db::diagnostic::Diagnostic;
use ruff_db::diagnostic::DisplayDiagnosticConfig;
use serde::Serialize;
use starpls_bazel::client::BazelCLI;
use starpls_bazel::client::BazelInfo;
use starpls_common::Dialect;
use starpls_common::File;
use starpls_common::FileInfo;
use starpls_common::Severity;
use starpls_ide::Analysis;
use starpls_ide::AnalysisSnapshot;
use starpls_ide::LoadDependency;
use starpls_ide::LoadResolution;
use walkdir::WalkDir;

use crate::bazel::BazelContext;
use crate::commands::InferenceOptions;
use crate::document::is_ignored_name;
use crate::document::DefaultFileLoader;
use crate::document::{self};
use crate::server::load_bazel_builtins;

#[derive(Args, Default)]
pub(crate) struct CheckCommand {
    /// Paths to typecheck.
    pub(crate) paths: Vec<String>,

    /// Read newline-delimited paths from a file, or '-' for standard input.
    #[clap(long, value_name = "PATH")]
    files_from: Option<PathBuf>,

    /// Select Bazel sources and .bzli interfaces.
    #[clap(long)]
    bazel_only: bool,

    /// Report dependency discovery and checked-file progress on stderr.
    #[clap(long)]
    progress: bool,

    /// Write a JSON coverage and diagnostics summary.
    #[clap(long, value_name = "PATH")]
    report: Option<PathBuf>,

    /// Check registered implementations against their stubs, including function bodies.
    #[clap(long)]
    pub(crate) validate_stubs: bool,

    /// Path to the Bazel output base.
    #[clap(long = "output_base")]
    pub(crate) output_base: Option<String>,

    /// Specify patterns of files/directories to ignore.
    #[clap(long = "ignore_pattern")]
    pub(crate) ignore_patterns: Vec<String>,

    #[clap(long = "ext")]
    pub(crate) extensions: Vec<String>,

    #[command(flatten)]
    pub(crate) inference_options: InferenceOptions,

    #[command(flatten)]
    pub(crate) type_interfaces: super::type_interface::TypeInterfaceOptions,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::CheckCommand;
    use super::Checker;
    use crate::document::source_tests::TestBazelClient;
    use crate::document::DefaultFileLoader;

    #[test]
    fn file_inventories_preserve_spaces_and_accept_crlf() {
        let mut paths = vec!["BUILD".to_owned()];
        super::read_file_list(
            b"dir with spaces/rules.bzl\r\n\nlast.bzl\n".as_slice(),
            &mut paths,
        )
        .unwrap();
        assert_eq!(paths, ["BUILD", "dir with spaces/rules.bzl", "last.bzl"]);
    }

    #[test]
    fn coverage_reports_transitive_failures_and_completed_roots() {
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("checker-coverage");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("BUILD"), "load(':dep.bzl')\n").unwrap();
        std::fs::write(root.join("dep.bzl"), "load(':missing.bzl')\n").unwrap();
        std::fs::write(root.join("deploy.star"), "fail('excluded')\n").unwrap();
        std::fs::write(root.join("template.bzl.in"), "@@template@@\n").unwrap();
        let (sender, _) = crossbeam_channel::unbounded();
        let loader = DefaultFileLoader::new(
            Arc::new(TestBazelClient::default()),
            root.clone(),
            None,
            None,
            sender,
            false,
        );
        let info = starpls_bazel::client::BazelInfo {
            workspace: root.clone(),
            ..Default::default()
        };
        let options = CheckCommand {
            bazel_only: true,
            ..Default::default()
        };
        let (analysis, loader) = options
            .prepare_analysis(loader, &info, Default::default())
            .unwrap();
        let paths = [
            "BUILD",
            "BUILD",
            "deploy.star",
            "template.bzl.in",
            "absent.bzl",
        ]
        .map(|path| root.join(path).to_str().unwrap().to_owned())
        .to_vec();
        let mut checker = Checker::new(analysis, info, paths, &["star"], loader, &options).unwrap();
        let report_path = root.join("coverage.json");
        assert!(checker
            .report_diagnostics(false, &[], Some(&report_path))
            .is_err());
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
        assert_eq!(report["complete"], false);
        assert_eq!(report["selected_files"].as_array().unwrap().len(), 1);
        assert_eq!(report["checked_files"].as_array().unwrap().len(), 1);
        assert_eq!(report["loaded_dependencies"].as_array().unwrap().len(), 1);
        assert_eq!(report["input_errors"].as_array().unwrap().len(), 1);
        let loads = report["unresolved_loads"].as_array().unwrap();
        let [load] = loads.as_slice() else {
            panic!("expected transitive failure: {loads:?}");
        };
        assert_eq!(load["module"], ":missing.bzl");
        assert!(load["source"]["path"]
            .as_str()
            .unwrap()
            .ends_with("/dep.bzl"));
        assert_eq!(
            report["excluded_inputs"][root.join("deploy.star").to_str().unwrap()],
            "not_bazel"
        );
        assert_eq!(
            report["excluded_inputs"][root.join("template.bzl.in").to_str().unwrap()],
            "unsupported_file"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recursive_selection_records_nested_repository_boundaries() {
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("checker-selection");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("BUILD"), "").unwrap();
        std::fs::write(root.join("nested/MODULE.bazel"), "").unwrap();
        std::fs::write(root.join("nested/defs.bzl"), "").unwrap();
        let (sender, _) = crossbeam_channel::unbounded();
        let loader = DefaultFileLoader::new(
            Arc::new(TestBazelClient::default()),
            root.clone(),
            None,
            None,
            sender,
            false,
        );
        let info = starpls_bazel::client::BazelInfo {
            workspace: root.clone(),
            ..Default::default()
        };
        let options = CheckCommand::default();
        let (analysis, loader) = options
            .prepare_analysis(loader, &info, Default::default())
            .unwrap();
        let checker = Checker::new(
            analysis,
            info,
            vec![root.to_str().unwrap().to_owned()],
            &[],
            loader,
            &options,
        )
        .unwrap();
        assert_eq!(checker.files.len(), 1);
        assert!(matches!(
            checker.exclusions.get(&root.join("nested")),
            Some(super::Exclusion::NestedRepository)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn explicit_source_symlinks_are_checked_and_unsupported_paths_fail() {
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("checker-explicit-paths");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("source.bzl"), "value = 1\n").unwrap();
        std::fs::write(root.join("unsupported.py"), "value = 1\n").unwrap();
        std::os::unix::fs::symlink(root.join("source.bzl"), root.join("alias.bzl")).unwrap();
        let (sender, _) = crossbeam_channel::unbounded();
        let loader = DefaultFileLoader::new(
            Arc::new(TestBazelClient::default()),
            root.clone(),
            None,
            None,
            sender,
            false,
        );
        let info = starpls_bazel::client::BazelInfo {
            workspace: root.clone(),
            ..Default::default()
        };
        let options = CheckCommand::default();
        let (analysis, loader) = options
            .prepare_analysis(loader, &info, Default::default())
            .unwrap();
        let paths = ["alias.bzl", "unsupported.py"]
            .map(|name| root.join(name).to_str().unwrap().to_owned())
            .to_vec();
        let mut checker = Checker::new(analysis, info, paths, &[], loader, &options).unwrap();
        assert_eq!(checker.files.len(), 1);
        assert_eq!(checker.input_errors.len(), 1);
        let graph = checker.prepare_loads().unwrap();
        let snapshot = checker.analysis.snapshot();
        let report = checker
            .coverage_report(&snapshot, &graph, &checker.files, Default::default())
            .unwrap();
        assert!(!report.complete);
        assert_eq!(
            report.checked_files.first().unwrap().path,
            root.join("source.bzl").canonicalize().unwrap()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unavailable_repository_context_remains_incomplete_without_queued_work() {
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("checker-pending");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("BUILD"), "load('@unknown//:defs.bzl')\n").unwrap();
        let (sender, _) = crossbeam_channel::unbounded();
        let loader = DefaultFileLoader::new(
            Arc::new(TestBazelClient::default()),
            root.clone(),
            None,
            None,
            sender,
            true,
        );
        let info = starpls_bazel::client::BazelInfo {
            workspace: root.clone(),
            ..Default::default()
        };
        let options = CheckCommand::default();
        let (analysis, loader) = options
            .prepare_analysis(loader, &info, Default::default())
            .unwrap();
        let mut checker = Checker::new(
            analysis,
            info,
            vec![root.join("BUILD").to_str().unwrap().to_owned()],
            &[],
            loader,
            &options,
        )
        .unwrap();
        let graph = checker.prepare_loads().unwrap();
        assert!(checker.loader.pending_repository_mappings().is_empty());
        let report = checker
            .coverage_report(
                &checker.analysis.snapshot(),
                &graph,
                &checker.files,
                Default::default(),
            )
            .unwrap();
        assert!(!report.complete);
        let [load] = report.unresolved_loads.as_slice() else {
            panic!("expected unresolved load");
        };
        assert_eq!(load.module.as_ref(), "@unknown//:defs.bzl");
        assert_eq!(load.message, "load resolution is still pending");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn external_stub_packages_prepare_their_source_dependency_graph() {
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("checker-stub-graph");
        let workspace = root.join("workspace");
        let external = root.join("external");
        for path in [
            &workspace,
            &external.join("stubs+"),
            &external.join("rules+"),
            &external.join("wrong+"),
        ] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::fs::write(
            workspace.join("starpls.toml"),
            "[[stub-packages]]\nmanifest='@stubs//:package.toml'\nallow-unversioned=true\n",
        )
        .unwrap();
        std::fs::write(external.join("stubs+/package.toml"),
            "format-version=1\n[source]\nrepository='@dep'\nmodule='rules'\nversions=['1']\n[files]\n'defs.bzl'='defs.bzli'\n").unwrap();
        std::fs::write(
            external.join("stubs+/defs.bzli"),
            "def value() -> int: ...\n",
        )
        .unwrap();
        std::fs::write(
            external.join("rules+/defs.bzl"),
            "load('@dep//:value.bzl', 'helper')\ndef value(): return helper\n",
        )
        .unwrap();
        std::fs::write(external.join("wrong+/value.bzl"), "helper = 42\n").unwrap();
        let client = Arc::new(TestBazelClient::default());
        let (sender, _) = crossbeam_channel::unbounded();
        let loader = DefaultFileLoader::new(
            client.clone(),
            workspace.clone(),
            None,
            external,
            sender,
            true,
        );
        let info = starpls_bazel::client::BazelInfo {
            workspace: workspace.clone(),
            ..Default::default()
        };
        let (analysis, loader) = CheckCommand::default()
            .prepare_analysis(loader, &info, Default::default())
            .unwrap();
        assert_eq!(
            client.mapping_requests.lock().unwrap().as_slice(),
            &[vec![String::new()], vec!["stubs+".to_owned()]]
        );
        let source = analysis.type_interface_sources();
        let mut checker = Checker::new(
            analysis,
            info,
            Vec::new(),
            &[],
            loader,
            &CheckCommand::default(),
        )
        .unwrap();
        let graph = checker.prepare_loads().unwrap();
        assert!(graph.unresolved.values().all(Vec::is_empty));
        assert!(source.iter().all(|source| graph.files.contains(source)));
        assert_eq!(graph.files.len(), 3);
        assert_eq!(
            client.mapping_requests.lock().unwrap().last().unwrap(),
            &["rules+"]
        );
        let diagnostics = checker.analysis.validate_stubs(|_| true).unwrap();
        assert!(diagnostics
            .iter()
            .flat_map(|(_, diagnostics)| diagnostics)
            .all(|diagnostic| diagnostic.id().as_str() != "load-error"));
        let report_path = root.join("coverage.json");
        checker
            .report_diagnostics(true, &[], Some(&report_path))
            .unwrap();
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
        assert_eq!(report["complete"], true);
        assert_eq!(report["selected_files"].as_array().unwrap().len(), 1);
        assert_eq!(report["checked_files"].as_array().unwrap().len(), 2);
        assert_eq!(report["loaded_dependencies"].as_array().unwrap().len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}

impl CheckCommand {
    pub(crate) fn run(self) -> anyhow::Result<()> {
        let mut paths = self.paths.clone();
        if let Some(path) = &self.files_from {
            if path == Path::new("-") {
                read_file_list(std::io::stdin().lock(), &mut paths)?;
            } else {
                let file = std::fs::File::open(path)
                    .with_context(|| format!("cannot open file inventory {}", path.display()))?;
                read_file_list(std::io::BufReader::new(file), &mut paths)?;
            }
        }
        if self.progress {
            eprintln!("Initializing Bazel context");
        }
        let bazel_client = Arc::new(BazelCLI::default());
        let bazel_cx = BazelContext::new(&*bazel_client)
            .map_err(|err| anyhow!("failed to initialize Bazel context: {}", err))?;
        let (fetch_repo_sender, _) = crossbeam_channel::unbounded();
        let loader = DefaultFileLoader::new(
            bazel_client,
            bazel_cx.info.workspace.clone(),
            bazel_cx.info.workspace_name.clone(),
            bazel_cx.info.output_base.join("external"),
            fetch_repo_sender,
            bazel_cx.bzlmod_enabled,
        );
        loader.finish_mapping(String::new(), Ok(bazel_cx.main_repo_mapping));
        let (analysis, loader) = self.prepare_analysis(loader, &bazel_cx.info, bazel_cx.rules)?;

        // Strip off the leading "." from each of the specified extensions.
        // This works better when filtering against files with .extension().
        let extensions = self
            .extensions
            .iter()
            .map(|ext| match ext.strip_prefix('.') {
                Some(ext) => ext,
                None => ext,
            })
            .chain(["star", "sky"])
            .collect::<Vec<_>>();

        let mut checker = Checker::new(analysis, bazel_cx.info, paths, &extensions, loader, &self)?;
        checker.report_diagnostics(
            self.validate_stubs,
            &self.ignore_patterns,
            self.report.as_deref(),
        )
    }

    fn prepare_analysis(
        &self,
        loader: DefaultFileLoader,
        info: &BazelInfo,
        rules: starpls_bazel::build::BuildLanguage,
    ) -> anyhow::Result<(Analysis, Arc<DefaultFileLoader>)> {
        // Package manifests can resolve source repositories relative to an
        // external annotation repository. Finish those synchronous queries
        // before deferring mappings discovered through the source graph.
        let prepared = self.type_interfaces.prepare(&loader, &info.workspace)?;
        let loader = Arc::new(loader.with_deferred_mappings());
        let mut analysis = Analysis::new(
            loader.clone(),
            starpls_ide::InferenceOptions {
                infer_ctx_attributes: self.inference_options.infer_ctx_attributes,
                use_code_flow_analysis: self.inference_options.use_code_flow_analysis,
                ..Default::default()
            },
        )?;
        analysis.set_builtin_defs(load_bazel_builtins(), rules)?;
        prepared.install(&mut analysis, &info.workspace)?;
        Ok((analysis, loader))
    }
}

fn read_file_list(reader: impl BufRead, paths: &mut Vec<String>) -> anyhow::Result<()> {
    for line in reader.lines() {
        let line = line?;
        if !line.is_empty() {
            paths.push(line);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum Exclusion {
    UnsupportedFile,
    NotBazel,
    Ignored,
    NestedRepository,
}

#[derive(Serialize)]
struct InputError {
    path: PathBuf,
    message: String,
}

#[derive(Serialize)]
struct ReportFile {
    path: PathBuf,
    repository: Option<String>,
}

#[derive(Serialize)]
struct UnresolvedLoad {
    source: ReportFile,
    module: Box<str>,
    start: u32,
    end: u32,
    message: String,
}

#[derive(Default, Serialize)]
struct DiagnosticCounts {
    errors: usize,
    warnings: usize,
    infos: usize,
}

#[derive(Serialize)]
struct CoverageReport<'a> {
    version: u32,
    workspace: &'a Path,
    bazel_release: &'a str,
    selected_files: Vec<ReportFile>,
    checked_files: Vec<ReportFile>,
    loaded_dependencies: Vec<ReportFile>,
    excluded_inputs: &'a BTreeMap<PathBuf, Exclusion>,
    input_errors: &'a [InputError],
    unresolved_loads: Vec<UnresolvedLoad>,
    diagnostics: DiagnosticCounts,
    complete: bool,
}

struct Checker {
    analysis: Analysis,
    bazel_info: BazelInfo,
    files: indexmap::IndexSet<File>,
    exclusions: BTreeMap<PathBuf, Exclusion>,
    input_errors: Vec<InputError>,
    loader: Arc<DefaultFileLoader>,
    progress: bool,
}

#[derive(Default)]
struct LoadGraph {
    files: indexmap::IndexSet<File>,
    unresolved: indexmap::IndexMap<File, Vec<LoadDependency>>,
}

impl Checker {
    fn new(
        analysis: Analysis,
        bazel_info: BazelInfo,
        paths: Vec<String>,
        extensions: &[&str],
        loader: Arc<DefaultFileLoader>,
        options: &CheckCommand,
    ) -> anyhow::Result<Self> {
        let mut checker = Self {
            analysis,
            bazel_info,
            files: Default::default(),
            exclusions: Default::default(),
            input_errors: Vec::new(),
            loader,
            progress: options.progress,
        };

        checker
            .files
            .extend(checker.analysis.type_interface_files());
        if paths.is_empty() && checker.files.is_empty() {
            checker.input_errors.push(InputError {
                path: checker.bazel_info.workspace.clone(),
                message: "no input paths or configured interfaces were selected".to_owned(),
            });
        }
        for path in paths {
            let mut walk = WalkDir::new(&path).into_iter();
            while let Some(entry) = walk.next() {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        checker.input_errors.push(InputError {
                            path: error
                                .path()
                                .unwrap_or_else(|| Path::new(&path))
                                .to_path_buf(),
                            message: error.to_string(),
                        });
                        continue;
                    }
                };
                if !document::visit_source_entry(&entry, &options.ignore_patterns) {
                    checker
                        .exclusions
                        .insert(entry.path().to_path_buf(), Exclusion::Ignored);
                    if entry.file_type().is_dir() {
                        walk.skip_current_dir();
                    }
                    continue;
                }
                if entry.depth() > 0
                    && entry.file_type().is_dir()
                    && document::is_repository_root(entry.path())
                {
                    checker
                        .exclusions
                        .insert(entry.path().to_path_buf(), Exclusion::NestedRepository);
                    walk.skip_current_dir();
                    continue;
                }
                if entry.file_type().is_file() || (entry.depth() == 0 && entry.path().is_file()) {
                    if let Err(error) = checker.load_file(
                        entry.path(),
                        entry.depth() == 0,
                        extensions,
                        options.bazel_only,
                    ) {
                        checker.input_errors.push(InputError {
                            path: entry.path().to_path_buf(),
                            message: format!("{error:#}"),
                        });
                    }
                } else if entry.depth() == 0 && !entry.file_type().is_dir() {
                    checker.input_errors.push(InputError {
                        path: entry.path().to_path_buf(),
                        message: "input is not a regular source file or directory".to_owned(),
                    });
                }
            }
        }

        Ok(checker)
    }

    fn load_file(
        &mut self,
        path: &Path,
        is_explicit: bool,
        extensions: &[&str],
        bazel_only: bool,
    ) -> anyhow::Result<()> {
        let path = std::path::absolute(path)?;
        let canonical_path = path.canonicalize()?;

        let Some((dialect, api_context)) =
            document::source_kind(&self.bazel_info.workspace, &canonical_path, extensions)
        else {
            if is_explicit {
                if !bazel_only {
                    anyhow::bail!("unsupported Starlark source file {}", path.display());
                }
                self.exclusions.insert(path, Exclusion::UnsupportedFile);
            }
            return Ok(());
        };

        if bazel_only && dialect != Dialect::Bazel {
            self.exclusions.insert(path, Exclusion::NotBazel);
            return Ok(());
        }
        self.loader.repository_for_path(&path)?;

        let info = api_context.map(|api_context| FileInfo::Bazel {
            api_context,
            is_external: canonical_path.starts_with(&self.bazel_info.output_base),
        });

        let file = self.analysis.file(&canonical_path, dialect, info)?;
        self.files.insert(file);

        Ok(())
    }

    fn report_diagnostics_for_file(
        snapshot: &AnalysisSnapshot,
        diagnostics: &[Diagnostic],
        counts: &mut DiagnosticCounts,
    ) -> anyhow::Result<()> {
        for diagnostic in diagnostics {
            match diagnostic.severity() {
                Severity::Info => counts.infos += 1,
                Severity::Warning => counts.warnings += 1,
                Severity::Error => counts.errors += 1,
                Severity::Fatal => counts.errors += 1,
            }
        }
        let config = DisplayDiagnosticConfig::new("starpls").color(true);
        anstream::print!("{}", snapshot.render_diagnostics(diagnostics, &config)?);
        Ok(())
    }

    fn prepare_loads(&mut self) -> anyhow::Result<LoadGraph> {
        let mut graph = LoadGraph {
            files: self.files.clone(),
            unresolved: Default::default(),
        };
        graph.files.extend(self.analysis.type_interface_sources());
        let mut frontier: Vec<_> = graph.files.iter().copied().collect();
        if self.progress {
            eprintln!("Discovering loads from {} source files", frontier.len());
        }
        let mut visited = 0;
        loop {
            let snapshot = self.analysis.snapshot();
            let mut pending = indexmap::IndexSet::new();
            while let Some(file) = frontier.pop() {
                visited += 1;
                if self.progress && (visited == 1 || visited % 100 == 0) {
                    eprintln!(
                        "Discovering loads: {visited} files visited, {} discovered; {}",
                        graph.files.len(),
                        snapshot.path(file).display()
                    );
                }
                let mut unresolved = Vec::new();
                for edge in snapshot.load_dependencies(file)? {
                    match &edge.resolution {
                        LoadResolution::Resolved(loaded) => {
                            if graph.files.insert(*loaded) {
                                frontier.push(*loaded);
                            }
                        }
                        LoadResolution::Pending => {
                            pending.insert(file);
                            unresolved.push(edge);
                        }
                        LoadResolution::Failed(_) => unresolved.push(edge),
                    }
                }
                if unresolved.is_empty() {
                    graph.unresolved.shift_remove(&file);
                } else {
                    graph.unresolved.insert(file, unresolved);
                }
            }
            drop(snapshot);
            let mut repositories = self.loader.pending_repository_mappings();
            if repositories.is_empty() {
                return Ok(graph);
            }
            self.analysis.invalidate_loads();
            while !repositories.is_empty() {
                if self.progress {
                    eprintln!(
                        "Resolving repository mappings: {} repositories",
                        repositories.len()
                    );
                }
                self.loader.resolve_repository_mappings(&repositories);
                repositories = self.loader.pending_repository_mappings();
            }
            frontier.extend(pending);
        }
    }

    fn report_diagnostics(
        &mut self,
        validate_stubs: bool,
        ignore_patterns: &[String],
        report_path: Option<&Path>,
    ) -> anyhow::Result<()> {
        let graph = self.prepare_loads()?;
        if self.progress {
            eprintln!(
                "Load discovery finished: {} selected files, {} dependency files",
                self.files.len(),
                graph.files.len() - self.files.len()
            );
        }
        let validated = if validate_stubs {
            if self.progress {
                eprintln!("Validating configured stub implementations");
            }
            self.analysis.validate_stubs(|path| {
                !path
                    .components()
                    .any(|component| is_ignored_name(component.as_os_str(), ignore_patterns))
            })?
        } else {
            Vec::new()
        };
        let validated_files: HashSet<_> = validated.iter().map(|(file, _)| *file).collect();
        let snapshot = self.analysis.snapshot();
        let mut counts = DiagnosticCounts::default();
        let mut checked = indexmap::IndexSet::new();
        for InputError { path, message } in &self.input_errors {
            eprintln!("Cannot select {}: {message}", path.display());
        }
        let total = self.files.len()
            + validated_files
                .iter()
                .filter(|file| !self.files.contains(*file))
                .count();
        for file_id in &self.files {
            if validated_files.contains(file_id) {
                continue;
            }
            if self.progress {
                eprintln!(
                    "Checking {}/{}: {}",
                    checked.len() + 1,
                    total,
                    snapshot.path(*file_id).display()
                );
            }
            let diagnostics = snapshot.diagnostics(*file_id)?;
            Self::report_diagnostics_for_file(&snapshot, &diagnostics, &mut counts)?;
            checked.insert(*file_id);
        }

        for (file, diagnostics) in validated {
            Self::report_diagnostics_for_file(&snapshot, &diagnostics, &mut counts)?;
            checked.insert(file);
            if self.progress {
                eprintln!(
                    "Checked {}/{}: {}",
                    checked.len(),
                    total,
                    snapshot.path(file).display()
                );
            }
        }

        let report = self.coverage_report(&snapshot, &graph, &checked, counts)?;
        for load in &report.unresolved_loads {
            eprintln!(
                "Unresolved load in {}: {:?}: {}",
                load.source.path.display(),
                load.module,
                load.message
            );
        }
        if let Some(path) = report_path {
            use std::io::Write;
            let file = std::fs::File::create(path)
                .with_context(|| format!("cannot write coverage report {}", path.display()))?;
            let mut writer = std::io::BufWriter::new(file);
            serde_json::to_writer_pretty(&mut writer, &report)?;
            writeln!(writer)?;
            writer.flush()?;
        }
        anstream::println!(
            "Checked {} files; discovered {} dependencies; excluded {} inputs",
            report.checked_files.len(),
            report.loaded_dependencies.len(),
            report.excluded_inputs.len()
        );
        if !report.complete {
            anyhow::bail!(
                "coverage incomplete: {} input failures and {} unresolved loads",
                report.input_errors.len(),
                report.unresolved_loads.len()
            );
        }
        if report.diagnostics.errors > 0 {
            anyhow::bail!(
                "failed with {} errors and {} warnings",
                report.diagnostics.errors,
                report.diagnostics.warnings
            );
        }
        if report.diagnostics.warnings > 0 {
            anstream::println!(
                "{}",
                Renderer::styled().render(Level::Warning.title(&format!(
                    "passed with {} warnings",
                    report.diagnostics.warnings
                )))
            );
        }
        Ok(())
    }

    fn report_file(&self, snapshot: &AnalysisSnapshot, file: File) -> anyhow::Result<ReportFile> {
        let path = snapshot.path(file).to_path_buf();
        let repository = match file.dialect {
            Dialect::Standard => None,
            Dialect::Bazel => self
                .loader
                .repository_for_path(&path)?
                .map(|repository| repository.name),
        };
        Ok(ReportFile { path, repository })
    }

    fn coverage_report<'a>(
        &'a self,
        snapshot: &AnalysisSnapshot,
        graph: &LoadGraph,
        checked: &indexmap::IndexSet<File>,
        diagnostics: DiagnosticCounts,
    ) -> anyhow::Result<CoverageReport<'a>> {
        let mut unresolved_loads = Vec::new();
        for (file, edges) in &graph.unresolved {
            for LoadDependency {
                module,
                range,
                resolution,
            } in edges
            {
                let message = match resolution {
                    LoadResolution::Resolved(_) => {
                        unreachable!("resolved loads have no coverage failure")
                    }
                    LoadResolution::Pending => "load resolution is still pending".to_owned(),
                    LoadResolution::Failed(error) => error.clone(),
                };
                unresolved_loads.push(UnresolvedLoad {
                    source: self.report_file(snapshot, *file)?,
                    module: module.clone(),
                    start: range.start().to_u32(),
                    end: range.end().to_u32(),
                    message,
                });
            }
        }
        Ok(CoverageReport {
            version: 1,
            workspace: &self.bazel_info.workspace,
            bazel_release: &self.bazel_info.release,
            selected_files: self
                .files
                .iter()
                .map(|file| self.report_file(snapshot, *file))
                .collect::<anyhow::Result<_>>()?,
            checked_files: checked
                .iter()
                .map(|file| self.report_file(snapshot, *file))
                .collect::<anyhow::Result<_>>()?,
            loaded_dependencies: graph
                .files
                .difference(checked)
                .filter(|file| !self.files.contains(*file))
                .map(|file| self.report_file(snapshot, *file))
                .collect::<anyhow::Result<_>>()?,
            excluded_inputs: &self.exclusions,
            input_errors: &self.input_errors,
            diagnostics,
            complete: self.input_errors.is_empty()
                && unresolved_loads.is_empty()
                && self.files.is_subset(checked),
            unresolved_loads,
        })
    }
}
