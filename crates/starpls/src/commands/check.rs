use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use annotate_snippets::Level;
use annotate_snippets::Renderer;
use anyhow::anyhow;
use anyhow::bail;
use clap::Args;
use ruff_db::diagnostic::DisplayDiagnosticConfig;
use starpls_bazel::client::BazelCLI;
use starpls_bazel::client::BazelInfo;
use starpls_common::Dialect;
use starpls_common::File;
use starpls_common::FileInfo;
use starpls_common::Severity;
use starpls_ide::Analysis;
use starpls_ide::AnalysisSnapshot;
use walkdir::DirEntry;
use walkdir::WalkDir;

use crate::bazel::BazelContext;
use crate::commands::InferenceOptions;
use crate::document::DefaultFileLoader;
use crate::document::{self};
use crate::server::load_bazel_builtins;

#[derive(Args, Default)]
pub(crate) struct CheckCommand {
    /// Paths to typecheck.
    pub(crate) paths: Vec<String>,

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
        let loader = DefaultFileLoader::new(
            bazel_client,
            bazel_cx.info.workspace.clone(),
            bazel_cx.info.workspace_name.clone(),
            bazel_cx.info.output_base.join("external"),
            fetch_repo_sender,
            bazel_cx.bzlmod_enabled,
        );

        let mut analysis = Analysis::new(
            Arc::new(loader),
            starpls_ide::InferenceOptions {
                infer_ctx_attributes: self.inference_options.infer_ctx_attributes,
                use_code_flow_analysis: self.inference_options.use_code_flow_analysis,
                ..Default::default()
            },
        )?;

        analysis.set_builtin_defs(builtins, bazel_cx.rules)?;
        self.type_interfaces
            .install(&mut analysis, &bazel_cx.info.workspace)?;

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

        let checker = Checker::new(
            analysis,
            bazel_cx.info,
            self.paths,
            self.ignore_patterns,
            &extensions,
        )?;
        checker.report_diagnostics()
    }
}

struct Checker {
    analysis: Analysis,
    bazel_info: BazelInfo,
    files: indexmap::IndexSet<File>,
    ignored_files: HashSet<PathBuf>,
}

fn is_hidden(entry: &DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|s| {
            // Don't consider lone "." as a hidden entry.
            s.starts_with('.') && s != "."
        })
        .unwrap_or(false)
}

impl Checker {
    fn new(
        analysis: Analysis,
        bazel_info: BazelInfo,
        paths: Vec<String>,
        ignore_patterns: Vec<String>,
        extensions: &[&str],
    ) -> anyhow::Result<Self> {
        let mut checker = Self {
            analysis,
            bazel_info,
            files: Default::default(),
            ignored_files: Default::default(),
        };

        checker
            .files
            .extend(checker.analysis.type_interface_files());
        for path in paths {
            for entry in WalkDir::new(&path).into_iter().filter_entry(|e| {
                !is_hidden(e)
                    && !ignore_patterns
                        .iter()
                        .any(|pat| e.file_name().to_str().map(|s| s == pat).unwrap_or(false))
            }) {
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
        let canonical_path = PathBuf::from(&path).canonicalize()?;

        let (dialect, api_context) = match document::dialect_and_api_context_for_workspace_path(
            &self.bazel_info.workspace,
            &canonical_path,
        ) {
            Some(res) => res,
            None => bail!("Failed to determine Starlark dialect for file: {:?}", path),
        };

        // Only process files that match any of the file extensions passed via the command line.
        // This always includes ".star" and ".sky" files.
        if dialect == Dialect::Standard
            && !path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| extensions.contains(&ext))
                .unwrap_or(false)
        {
            if is_explicit {
                self.ignored_files.insert(path.to_path_buf());
            }
            return Ok(());
        }

        let info = api_context.map(|api_context| FileInfo::Bazel {
            api_context,
            is_external: canonical_path.starts_with(&self.bazel_info.output_base),
        });

        let file = self.analysis.file(&canonical_path, dialect, info)?;
        self.files.insert(file);

        Ok(())
    }

    fn report_diagnostics_for_file(
        &self,
        snapshot: &AnalysisSnapshot,
        file_id: File,
        num_errors: &mut usize,
        num_warnings: &mut usize,
        num_infos: &mut usize,
    ) -> anyhow::Result<()> {
        let diagnostics = snapshot.diagnostics(file_id)?;
        for diagnostic in &diagnostics {
            match diagnostic.severity() {
                Severity::Info => *num_infos += 1,
                Severity::Warning => *num_warnings += 1,
                Severity::Error => *num_errors += 1,
                Severity::Fatal => *num_errors += 1,
            }
        }
        let config = DisplayDiagnosticConfig::new("starpls").color(true);
        anstream::print!("{}", snapshot.render_diagnostics(&diagnostics, &config)?);
        Ok(())
    }

    fn report_diagnostics(&self) -> anyhow::Result<()> {
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
            self.report_diagnostics_for_file(
                &snapshot,
                *file_id,
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
