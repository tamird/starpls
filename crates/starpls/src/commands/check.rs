use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use annotate_snippets::Level;
use annotate_snippets::Renderer;
use anyhow::anyhow;
use clap::Args;
use ruff_db::diagnostic::Diagnostic;
use ruff_db::diagnostic::DisplayDiagnosticConfig;
use starpls_bazel::client::BazelCLI;
use starpls_bazel::client::BazelInfo;
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
        let mut checker =
            Checker::new(analysis, info, Vec::new(), Vec::new(), &[], loader).unwrap();
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
        std::fs::remove_dir_all(root).unwrap();
    }
}

impl CheckCommand {
    pub(crate) fn run(self) -> anyhow::Result<()> {
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

        let ignore_patterns = self.ignore_patterns.clone();

        let mut checker = Checker::new(
            analysis,
            bazel_cx.info,
            self.paths,
            self.ignore_patterns,
            &extensions,
            loader,
        )?;
        checker.report_diagnostics(self.validate_stubs, &ignore_patterns)
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

struct Checker {
    analysis: Analysis,
    bazel_info: BazelInfo,
    files: indexmap::IndexSet<File>,
    ignored_files: HashSet<PathBuf>,
    loader: Arc<DefaultFileLoader>,
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
        ignore_patterns: Vec<String>,
        extensions: &[&str],
        loader: Arc<DefaultFileLoader>,
    ) -> anyhow::Result<Self> {
        let mut checker = Self {
            analysis,
            bazel_info,
            files: Default::default(),
            ignored_files: Default::default(),
            loader,
        };

        checker
            .files
            .extend(checker.analysis.type_interface_files());
        for path in paths {
            for entry in WalkDir::new(&path)
                .into_iter()
                .filter_entry(|e| document::visit_source_entry(e, &ignore_patterns))
            {
                let entry = entry?;
                if entry.file_type().is_file() {
                    let is_explicit = entry.path().as_os_str().to_str() == Some(path.as_str());
                    checker.load_file(entry.path(), is_explicit, extensions)?;
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
    ) -> anyhow::Result<()> {
        let path = std::path::absolute(path)?;
        self.loader.repository_for_path(&path)?;
        let canonical_path = path.canonicalize()?;

        let Some((dialect, api_context)) =
            document::source_kind(&self.bazel_info.workspace, &canonical_path, extensions)
        else {
            if is_explicit {
                self.ignored_files.insert(path.to_path_buf());
            }
            return Ok(());
        };

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
        num_errors: &mut usize,
        num_warnings: &mut usize,
        num_infos: &mut usize,
    ) -> anyhow::Result<()> {
        for diagnostic in diagnostics {
            match diagnostic.severity() {
                Severity::Info => *num_infos += 1,
                Severity::Warning => *num_warnings += 1,
                Severity::Error => *num_errors += 1,
                Severity::Fatal => *num_errors += 1,
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
        loop {
            let snapshot = self.analysis.snapshot();
            let mut pending = indexmap::IndexSet::new();
            while let Some(file) = frontier.pop() {
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
                graph.unresolved.insert(file, unresolved);
            }
            drop(snapshot);
            let mut repositories = self.loader.pending_repository_mappings();
            if repositories.is_empty() {
                return Ok(graph);
            }
            self.analysis.invalidate_loads();
            while !repositories.is_empty() {
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
    ) -> anyhow::Result<()> {
        let _graph = self.prepare_loads()?;
        let validated = if validate_stubs {
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
        let mut num_errors = 0;
        let mut num_warnings = 0;
        let mut num_infos = 0;

        let mut ignored_files = self.ignored_files.iter().collect::<Vec<_>>();
        ignored_files.sort();

        for path in ignored_files {
            anstream::print!(
                "{}\n\n",
                Renderer::styled().render(
                    Level::Warning.title(&format!("non-Starlark file {:?} was ignored", path))
                )
            );
            num_warnings += 1;
        }

        for file_id in &self.files {
            if validated_files.contains(file_id) {
                continue;
            }
            let diagnostics = snapshot.diagnostics(*file_id)?;
            Self::report_diagnostics_for_file(
                &snapshot,
                &diagnostics,
                &mut num_errors,
                &mut num_warnings,
                &mut num_infos,
            )?;
        }

        for (_, diagnostics) in validated {
            Self::report_diagnostics_for_file(
                &snapshot,
                &diagnostics,
                &mut num_errors,
                &mut num_warnings,
                &mut num_infos,
            )?;
        }

        if num_errors > 0 {
            if num_warnings > 0 {
                anstream::println!(
                    "{}",
                    Renderer::styled().render(Level::Error.title(&format!(
                        "failed with {} errors and {} warnings",
                        num_errors, num_warnings
                    )))
                );
            } else {
                anstream::println!(
                    "{}",
                    Renderer::styled()
                        .render(Level::Error.title(&format!("failed with {} errors", num_errors)))
                );
            }
            std::process::exit(1);
        }
        if num_warnings > 0 {
            anstream::println!(
                "{}",
                Renderer::styled().render(
                    Level::Warning.title(&format!("passed with {} warnings", num_warnings))
                )
            );
        }

        Ok(())
    }
}
