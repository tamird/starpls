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

impl CheckCommand {
    pub(crate) fn run(self) -> anyhow::Result<()> {
        let bazel_client = Arc::new(BazelCLI::default());
        let bazel_cx = BazelContext::new(&*bazel_client)
            .map_err(|err| anyhow!("failed to initialize Bazel context: {}", err))?;
        let builtins = load_bazel_builtins();
        let (fetch_repo_sender, _) = crossbeam_channel::unbounded();
        let loader = Arc::new(DefaultFileLoader::new(
            bazel_client,
            bazel_cx.info.workspace.clone(),
            bazel_cx.info.workspace_name.clone(),
            bazel_cx.info.output_base.join("external"),
            fetch_repo_sender,
            bazel_cx.bzlmod_enabled,
        ));

        let mut analysis = Analysis::new(
            loader.clone(),
            starpls_ide::InferenceOptions {
                infer_ctx_attributes: self.inference_options.infer_ctx_attributes,
                use_code_flow_analysis: self.inference_options.use_code_flow_analysis,
                ..Default::default()
            },
        )?;

        analysis.set_builtin_defs(builtins, bazel_cx.rules)?;
        self.type_interfaces
            .install(&mut analysis, &loader, &bazel_cx.info.workspace)?;

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
}

struct Checker {
    analysis: Analysis,
    bazel_info: BazelInfo,
    files: indexmap::IndexSet<File>,
    ignored_files: HashSet<PathBuf>,
    loader: Arc<DefaultFileLoader>,
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

    fn report_diagnostics(
        &mut self,
        validate_stubs: bool,
        ignore_patterns: &[String],
    ) -> anyhow::Result<()> {
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
