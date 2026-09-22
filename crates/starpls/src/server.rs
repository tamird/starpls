use std::collections::HashMap;
use std::mem;
use std::panic;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use log::debug;
use log::error;
use log::info;
use lsp_server::Connection;
use lsp_server::ReqQueue;
use lsp_types::notification::Notification;
use lsp_types::request::Request;
use rustc_hash::FxHashSet;
use starpls_bazel::build_language::decode_rules;
use starpls_bazel::client::BazelCLI;
use starpls_bazel::client::BazelClient;
use starpls_bazel::decode_builtins;
use starpls_bazel::APIContext;
use starpls_bazel::Builtins;
use starpls_common::Dialect;
use starpls_common::File;
use starpls_common::FileInfo;
use starpls_ide::Analysis;
use starpls_ide::AnalysisSnapshot;
use starpls_ide::InferenceOptions;

use crate::bazel::BazelContext;
use crate::config::ServerConfig;
use crate::debouncer::AnalysisDebouncer;
use crate::diagnostics::DiagnosticsManager;
use crate::document::DefaultFileLoader;
use crate::event_loop::FetchExternalReposProgress;
use crate::event_loop::RefreshAllWorkspaceTargetsProgress;
use crate::event_loop::Task;
use crate::task_pool::TaskPool;
use crate::task_pool::TaskPoolHandle;

const BAZEL_INIT_ERR_MESSAGE: &str = "Failed to fetch Bazel configuration! Please check the language server logs for more details. Certain features may not work correctly until the underlying issue is fixed.";

pub(crate) enum OutgoingRequest {
    Other,
    RegisterFileWatchers(u64),
}

pub(crate) struct Server {
    pub(crate) config: Arc<ServerConfig>,
    pub(crate) connection: Connection,
    pub(crate) req_queue: ReqQueue<(), OutgoingRequest>,
    pub(crate) task_pool_handle: TaskPoolHandle<Task>,
    pub(crate) bazel_task_pool: TaskPool<Task>,
    pub(crate) workspace: PathBuf,
    pub(crate) analysis_changed: bool,
    pub(crate) diagnostics_manager: DiagnosticsManager,
    pub(crate) analysis: Analysis,
    pub(crate) analysis_debouncer: AnalysisDebouncer,
    pub(crate) analysis_requested_for_files: Option<Vec<File>>,
    pub(crate) bazel_client: Arc<dyn BazelClient>,
    pub(crate) pending_repos: FxHashSet<String>,
    pub(crate) is_fetching_repos: bool,
    pub(crate) is_refreshing_all_workspace_targets: bool,
    pub(crate) bzlmod_enabled: bool,
    pub(crate) loader: Arc<DefaultFileLoader>,
    pub(crate) configuration: ConfigurationState,
}

pub(crate) struct ServerSnapshot {
    pub(crate) config: Arc<ServerConfig>,
    pub(crate) analysis_snapshot: AnalysisSnapshot,
    pub(crate) configuration_revision: u64,
    pub(crate) configuration_ready: bool,
    pub(crate) workspace: PathBuf,
    pub(crate) loader: Arc<DefaultFileLoader>,
}

#[derive(Default)]
pub(crate) struct ConfigurationState {
    pub(crate) revision: u64,
    pub(crate) complete: bool,
    pub(crate) refreshing: bool,
    pub(crate) pending: bool,
    pub(crate) needs_reopen: bool,
    restart_required: bool,
    inputs: HashMap<PathBuf, Option<Vec<u8>>>,
    watched: FxHashSet<PathBuf>,
    watcher_id: u64,
}

pub(crate) struct ConfigurationReady {
    pub(crate) revision: u64,
    pub(crate) result: anyhow::Result<PreparedConfiguration>,
}

pub(crate) struct PreparedConfiguration {
    loader: Box<DefaultFileLoader>,
    rules: Option<starpls_bazel::build::BuildLanguage>,
    interfaces: anyhow::Result<crate::commands::type_interface::PreparedInterfaces>,
}

impl std::fmt::Debug for ConfigurationReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigurationReady")
            .field("revision", &self.revision)
            .field("error", &self.result.as_ref().err())
            .finish_non_exhaustive()
    }
}

impl Server {
    pub(crate) fn new(connection: Connection, config: ServerConfig) -> anyhow::Result<Self> {
        let bazel_path = config.args.bazel_path.as_deref().unwrap_or("bazel");
        let client = BazelCLI::new(bazel_path).with_working_directory(config.workspace.clone())?;
        Self::with_client(connection, config, Arc::new(client))
    }

    pub(crate) fn with_client(
        connection: Connection,
        config: ServerConfig,
        bazel_client: Arc<dyn BazelClient>,
    ) -> anyhow::Result<Self> {
        let (task_pool_sender, task_pool_receiver) = crossbeam_channel::unbounded();
        let task_pool = TaskPool::with_num_threads(task_pool_sender.clone(), 4)?;
        let task_pool_handle = TaskPoolHandle::new(task_pool_receiver, task_pool);
        // Bazel serializes commands; keep its waits away from editor workers.
        let bazel_task_pool = TaskPool::with_num_threads(task_pool_sender.clone(), 1)?;
        let workspace = config.workspace.clone();
        let loader = Arc::new(
            DefaultFileLoader::new(
                bazel_client.clone(),
                workspace.clone(),
                None,
                None,
                task_pool_sender.clone(),
                false,
            )
            .for_editor(0),
        );
        let mut analysis = Analysis::new(
            loader.clone(),
            InferenceOptions {
                infer_ctx_attributes: config.args.inference_options.infer_ctx_attributes,
                use_code_flow_analysis: config.args.inference_options.use_code_flow_analysis,
                ..Default::default()
            },
        )?;
        analysis.set_builtin_defs(load_bazel_builtins(), Default::default())?;
        let prelude = workspace.join("tools/build_rules/prelude_bazel");
        if let Ok(file) = analysis.file(
            &prelude,
            Dialect::Bazel,
            Some(FileInfo::Bazel {
                api_context: APIContext::Prelude,
                is_external: false,
            }),
        ) {
            info!("found prelude file at {:?}", prelude);
            analysis.set_bazel_prelude_file(file);
        }
        let analysis_debounce_interval = config.args.analysis_debounce_interval;
        let mut server = Server {
            config: Arc::new(config),
            connection,
            req_queue: Default::default(),
            task_pool_handle,
            bazel_task_pool,
            workspace,
            analysis_changed: !analysis.type_interface_files().is_empty(),
            diagnostics_manager: Default::default(),
            analysis,
            analysis_debouncer: AnalysisDebouncer::new(
                Duration::from_millis(analysis_debounce_interval),
                task_pool_sender,
            ),
            analysis_requested_for_files: None,
            bazel_client,
            pending_repos: Default::default(),
            is_fetching_repos: false,
            is_refreshing_all_workspace_targets: false,
            bzlmod_enabled: false,
            loader,
            configuration: Default::default(),
        };

        server.configuration.inputs = server.loader.configuration_inputs();

        server.configuration.pending = true;
        server.start_configuration_refresh();
        Ok(server)
    }

    pub(crate) fn snapshot(&self) -> ServerSnapshot {
        ServerSnapshot {
            config: self.config.clone(),
            analysis_snapshot: self.analysis.snapshot(),
            configuration_revision: self.configuration.revision,
            configuration_ready: self.configuration.complete
                && !self.configuration.refreshing
                && !self.configuration.pending
                && self.loader.is_ready(),
            workspace: self.workspace.clone(),
            loader: self.loader.clone(),
        }
    }

    pub(crate) fn invalidate_diagnostics(&mut self) {
        // Unchanged editor buffers may depend on the changed source or host
        // resolution. Reject old jobs before another queued result is handled.
        self.diagnostics_manager.cancel_all();
        self.analysis_changed = true;
    }

    pub(crate) fn open_document(
        &mut self,
        path: &Path,
        contents: String,
        version: i32,
    ) -> anyhow::Result<()> {
        let Some((dialect, api_context)) =
            crate::document::dialect_and_api_context_for_workspace_path(&self.workspace, path)
        else {
            return Ok(());
        };
        let info = api_context.map(|api_context| FileInfo::Bazel {
            api_context,
            is_external: !path.starts_with(&self.workspace),
        });
        let file = self
            .analysis
            .open_document(path, dialect, info, contents, version)?;
        if api_context == Some(APIContext::Prelude) {
            self.analysis.set_bazel_prelude_file(file);
        }
        self.invalidate_diagnostics();
        Ok(())
    }

    pub(crate) fn send_request<R: lsp_types::request::Request>(&mut self, params: R::Params) {
        let req =
            self.req_queue
                .outgoing
                .register(R::METHOD.to_string(), params, OutgoingRequest::Other);
        self.send(req.into());
    }

    pub(crate) fn refresh_editor_semantics(&mut self) {
        let Some(workspace) = &self.config.caps.workspace else {
            return;
        };
        let tokens = workspace
            .semantic_tokens
            .as_ref()
            .is_some_and(|capability| capability.refresh_support == Some(true));
        let hints = workspace
            .inlay_hint
            .as_ref()
            .is_some_and(|capability| capability.refresh_support == Some(true));
        if tokens {
            self.send_request::<lsp_types::request::SemanticTokensRefresh>(());
        }
        if hints {
            self.send_request::<lsp_types::request::InlayHintRefreshRequest>(());
        }
    }

    pub(crate) fn complete_request(&mut self, resp: lsp_server::Response) {
        if let Some(OutgoingRequest::RegisterFileWatchers(id)) =
            self.req_queue.outgoing.complete(resp.id)
        {
            if id != self.configuration.watcher_id {
                return;
            }
            if let Some(error) = resp.error {
                self.send_error_message(&format!("Cannot watch type interfaces: {}. Changes to closed files will not be reported.", error.message));
            } else {
                let paths: Vec<_> = self.configuration.inputs.keys().cloned().collect();
                if let Err(error) = self.configuration_changed(&paths) {
                    self.send_error_message(&format!("{error:#}"));
                }
                // Reconcile changes made between initial reads and watcher activation.
                self.analysis.invalidate_loads();
                self.invalidate_diagnostics();
            }
        }
    }

    pub(crate) fn watch_type_interfaces(&mut self) -> anyhow::Result<()> {
        let mut paths: FxHashSet<_> = self.loader.repository_roots()?.into_iter().collect();
        paths.insert(self.workspace.clone());
        for (path, contents) in self.loader.configuration_inputs() {
            self.configuration.inputs.entry(path).or_insert(contents);
        }
        let manifests = self.loader.configuration_paths();
        paths.extend(manifests.iter().cloned());
        let snapshot = self.analysis.snapshot();
        for file in self
            .analysis
            .type_interface_files()
            .into_iter()
            .chain(self.analysis.type_interface_sources())
        {
            let path = snapshot.path(file);
            if !paths.iter().any(|root| path.starts_with(root)) {
                paths.insert(
                    path.parent()
                        .expect("absolute configured path")
                        .to_path_buf(),
                );
            }
        }
        if paths == self.configuration.watched {
            return Ok(());
        }
        let capability = self
            .config
            .caps
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.did_change_watched_files.as_ref());
        if !capability
            .and_then(|watching| watching.dynamic_registration)
            .unwrap_or(false)
        {
            if self.configuration.watched.is_empty() {
                self.send_error_message("The client cannot watch Starpls dependencies. Changes to closed files require restarting the server.");
            }
            self.configuration.watched = paths;
            return Ok(());
        }
        let relative_patterns = capability
            .and_then(|watching| watching.relative_pattern_support)
            .unwrap_or(false);
        let mut watchers = Vec::new();
        for path in &paths {
            let pattern = if manifests.contains(path) {
                let name = path
                    .file_name()
                    .expect("manifest filename")
                    .to_string_lossy();
                escape_glob(&name)
            } else {
                "**/{*.bzl,*.bzli,BUILD,BUILD.bazel,MODULE.bazel,MODULE.bazel.lock,*.MODULE.bazel,WORKSPACE,WORKSPACE.bazel,WORKSPACE.bzlmod,starpls.toml,.bazelrc,.bazelversion}".to_owned()
            };
            let base = if manifests.contains(path) {
                path.parent().expect("absolute manifest")
            } else {
                path.as_path()
            };
            let glob_pattern = if base == self.workspace {
                lsp_types::GlobPattern::String(pattern)
            } else if relative_patterns {
                lsp_types::GlobPattern::Relative(lsp_types::RelativePattern {
                    base_uri: lsp_types::OneOf::Right(
                        lsp_types::Url::from_directory_path(base).expect("absolute directory"),
                    ),
                    pattern,
                })
            } else {
                self.send_error_message("The client cannot watch external Starpls dependencies without relative-pattern support. Changes to closed files require restarting the server.");
                continue;
            };
            watchers.push(lsp_types::FileSystemWatcher {
                glob_pattern,
                kind: None,
            });
        }
        if self.configuration.watcher_id != 0 {
            self.send_request::<lsp_types::request::UnregisterCapability>(
                lsp_types::UnregistrationParams {
                    unregisterations: vec![lsp_types::Unregistration {
                        id: format!("starpls-dependencies-{}", self.configuration.watcher_id),
                        method: lsp_types::notification::DidChangeWatchedFiles::METHOD.to_owned(),
                    }],
                },
            );
        }
        self.configuration.watcher_id += 1;
        self.configuration.watched = paths;
        let params = lsp_types::RegistrationParams {
            registrations: vec![lsp_types::Registration {
                id: format!("starpls-dependencies-{}", self.configuration.watcher_id),
                method: lsp_types::notification::DidChangeWatchedFiles::METHOD.to_owned(),
                register_options: Some(serde_json::to_value(
                    lsp_types::DidChangeWatchedFilesRegistrationOptions { watchers },
                )?),
            }],
        };
        let request = self.req_queue.outgoing.register(
            lsp_types::request::RegisterCapability::METHOD.to_owned(),
            params,
            OutgoingRequest::RegisterFileWatchers(self.configuration.watcher_id),
        );
        self.send(request.into());
        Ok(())
    }

    pub(crate) fn configuration_changed(&mut self, paths: &[PathBuf]) -> anyhow::Result<()> {
        let mut changed = false;
        let inputs = self.loader.configuration_inputs();
        let configured = self.config.args.type_interfaces.is_configured()
            || inputs
                .get(&self.workspace.join("starpls.toml"))
                .is_some_and(Option::is_some);
        for path in paths {
            let metadata = is_bazel_dependency(path)
                || (configured && path.extension().is_some_and(|extension| extension == "bzl"))
                || path == &self.workspace.join("starpls.toml")
                || inputs.contains_key(path);
            if !metadata {
                continue;
            }
            let contents = std::fs::read(path).ok();
            if self.configuration.inputs.get(path) != Some(&contents) {
                self.configuration.inputs.insert(path.clone(), contents);
                if path == &self.workspace.join(".bazelrc")
                    || path == &self.workspace.join(".bazelversion")
                {
                    self.configuration.restart_required = true;
                }
                changed = true;
            }
        }
        if changed {
            self.reload_configuration()?;
        }
        Ok(())
    }

    pub(crate) fn reload_configuration(&mut self) -> anyhow::Result<()> {
        self.configuration.revision += 1;
        self.configuration.complete = false;
        self.configuration.pending = true;
        self.loader.pause(true);
        let snapshot = self.analysis.snapshot();
        for file in self.analysis.type_interface_files() {
            let path = snapshot.path(file);
            if snapshot.document(path).is_none() {
                self.send_notification::<lsp_types::notification::PublishDiagnostics>(
                    lsp_types::PublishDiagnosticsParams {
                        uri: lsp_types::Url::from_file_path(path).expect("absolute interface"),
                        diagnostics: Vec::new(),
                        version: None,
                    },
                );
            }
        }
        drop(snapshot);
        self.analysis.set_type_interfaces(Vec::new())?;
        self.analysis.set_all_workspace_targets(Vec::new());
        self.invalidate_diagnostics();
        self.refresh_editor_semantics();
        self.pending_repos.clear();
        if self.configuration.restart_required {
            self.configuration.pending = false;
            self.send_error_message("Bazel startup configuration changed. Restart Starpls to reload the Bazel environment; trusted stub registrations have been removed.");
        } else {
            self.start_configuration_refresh();
        }
        Ok(())
    }

    pub(crate) fn start_configuration_refresh(&mut self) {
        if self.configuration.refreshing || !self.configuration.pending {
            return;
        }
        self.configuration.pending = false;
        self.configuration.refreshing = true;
        let revision = self.configuration.revision;
        let template = self.loader.clone();
        template.pause(true);
        self.analysis.invalidate_loads();
        let workspace = self.workspace.clone();
        let options = self.config.args.type_interfaces.clone();
        let client = self.bazel_client.clone();
        self.bazel_task_pool.spawn(move || {
            let result = (|| {
                let (loader, rules) = if template.has_bazel_context() {
                    (template.fresh(), None)
                } else {
                    let context = BazelContext::new(&*client)?;
                    if context.info.workspace.canonicalize()? != workspace.canonicalize()? {
                        anyhow::bail!(
                            "Bazel reported workspace {}, expected {}",
                            context.info.workspace.display(),
                            workspace.display()
                        );
                    }
                    let loader = template.with_context(
                        context.info.workspace_name,
                        context.info.output_base.join("external"),
                        context.bzlmod_enabled,
                    );
                    loader.finish_mapping(String::new(), Ok(context.main_repo_mapping));
                    (loader, Some(context.rules))
                };
                let interfaces = options.prepare(&loader, &workspace);
                Ok(PreparedConfiguration {
                    loader: Box::new(loader),
                    rules,
                    interfaces,
                })
            })();
            Task::ConfigurationReady(ConfigurationReady { revision, result })
        });
    }

    pub(crate) fn finish_configuration_refresh(&mut self, ready: ConfigurationReady) {
        let ConfigurationReady { revision, result } = ready;
        self.configuration.refreshing = false;
        if revision != self.configuration.revision {
            self.start_configuration_refresh();
            return;
        }
        let PreparedConfiguration {
            loader,
            rules,
            interfaces,
        } = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                self.loader.pause(false);
                self.send_error_message(&format!("{BAZEL_INIT_ERR_MESSAGE} {error:#}"));
                self.invalidate_diagnostics();
                return;
            }
        };
        let changed = loader
            .configuration_inputs()
            .into_iter()
            .any(|(path, contents)| std::fs::read(&path).ok() != contents);
        if changed {
            self.configuration.pending = true;
            self.start_configuration_refresh();
            return;
        }
        // Requests evaluated before the new environment arrived must retry.
        self.configuration.revision += 1;
        let loader = Arc::new((*loader).for_editor(self.configuration.revision));
        let snapshot = self.analysis.snapshot();
        let mut admission = Ok(());
        self.configuration.needs_reopen = false;
        for file in self.analysis.open_files() {
            let source = snapshot.path(file);
            let document = snapshot.document(source).expect("open file");
            if let Err(error) =
                loader.restore_document_context(&self.loader, document.path.as_std_path(), source)
            {
                self.send_error_message(&format!("{error:#}"));
                admission = Err(error);
                self.configuration.needs_reopen = true;
            }
        }
        drop(snapshot);
        self.bzlmod_enabled = loader.bzlmod_enabled();
        self.loader = loader;
        let replacement = self.analysis.replace_loader(self.loader.clone());
        let native = match rules {
            Some(rules) => self.analysis.set_builtin_defs(load_bazel_builtins(), rules),
            None => Ok(()),
        };
        let result = admission
            .and(replacement)
            .and(native)
            .and_then(|()| interfaces?.install(&mut self.analysis, &self.workspace));
        self.configuration.complete = result.is_ok();
        if let Err(error) = result {
            self.send_error_message(&format!("Cannot reload Starpls configuration: {error:#}"));
        }
        self.configuration
            .inputs
            .extend(self.loader.configuration_inputs());
        self.refresh_all_workspace_targets();
        self.invalidate_diagnostics();
        self.refresh_editor_semantics();
    }

    pub(crate) fn open_repository_changed(&self) -> bool {
        let snapshot = self.analysis.snapshot();
        self.analysis.open_files().into_iter().any(|file| {
            let source = snapshot.path(file);
            let document = snapshot.document(source).expect("open file");
            self.loader
                .open_repository_changed(document.path.as_std_path(), source)
        })
    }

    pub(crate) fn send_notification<N: lsp_types::notification::Notification>(
        &self,
        params: N::Params,
    ) {
        let not = lsp_server::Notification::new(N::METHOD.to_string(), params);
        self.send(not.into());
    }

    pub(crate) fn send(&self, message: lsp_server::Message) {
        self.connection.sender.send(message).unwrap();
    }

    pub(crate) fn send_error_message(&self, message: &str) {
        self.send_notification::<lsp_types::notification::ShowMessage>(
            lsp_types::ShowMessageParams {
                message: message.to_string(),
                typ: lsp_types::MessageType::ERROR,
            },
        )
    }

    pub(crate) fn fetch_bazel_external_repos(&mut self) {
        let repos = mem::take(&mut self.pending_repos);
        let bazel_client = self.bazel_client.clone();
        let bzlmod_enabled = self.bzlmod_enabled;
        let revision = self.configuration.revision;

        self.is_fetching_repos = true;
        self.bazel_task_pool.spawn_with_sender(move |sender| {
            sender
                .send(Task::FetchExternalRepos(FetchExternalReposProgress::Begin(
                    repos.clone(),
                )))
                .unwrap();

            let mut failed_repos = vec![];

            for repo in &repos {
                debug!("fetching external repository \"@@{}\"", repo);
                if let Err(err) = if bzlmod_enabled {
                    bazel_client.fetch_repo(repo)
                } else {
                    bazel_client.null_query_external_repo_targets(repo)
                } {
                    failed_repos.push(repo.clone());
                    error!(
                        "failed to fetch external repository \"@@{}\": {}",
                        repo, err
                    );
                }
            }

            sender
                .send(Task::FetchExternalRepos(FetchExternalReposProgress::End {
                    revision,
                    fetched: repos
                        .into_iter()
                        .filter(|repo| !failed_repos.contains(repo))
                        .collect(),
                    failed: failed_repos,
                }))
                .unwrap();
        });
    }

    pub(crate) fn refresh_all_workspace_targets(&mut self) {
        if self.is_refreshing_all_workspace_targets || !self.config.args.enable_label_completions {
            return;
        }

        let bazel_client = self.bazel_client.clone();
        let revision = self.configuration.revision;

        self.is_refreshing_all_workspace_targets = true;
        self.bazel_task_pool.spawn_with_sender(move |sender| {
            sender
                .send(Task::RefreshAllWorkspaceTargets(
                    RefreshAllWorkspaceTargetsProgress::Begin,
                ))
                .unwrap();

            let targets = match bazel_client.query_all_workspace_targets() {
                Ok(targets) => Some(targets),
                Err(err) => {
                    error!("failed to query all workspace targets: {}", err);
                    None
                }
            };

            sender
                .send(Task::RefreshAllWorkspaceTargets(
                    RefreshAllWorkspaceTargetsProgress::End { revision, targets },
                ))
                .unwrap();
        });
    }
}

impl ServerSnapshot {
    pub(crate) fn ensure_workspace_ready(&self) -> anyhow::Result<()> {
        if !self.configuration_ready || !self.loader.mappings_ready() {
            anyhow::bail!("Bazel configuration or repository mappings are not ready for workspace references or rename");
        }
        Ok(())
    }

    pub(crate) fn reference_files(&self, name: &str) -> anyhow::Result<Vec<File>> {
        self.ensure_workspace_ready()?;
        let mut files = self.analysis_snapshot.reference_files()?;
        let mut paths = self.loader.loaded_paths();
        let mut walk = walkdir::WalkDir::new(&self.workspace).into_iter();
        while let Some(entry) = walk.next() {
            self.analysis_snapshot.check_cancelled()?;
            let entry = entry?;
            if !crate::document::visit_source_entry(&entry, &self.config.args.ignore_patterns) {
                if entry.file_type().is_dir() {
                    walk.skip_current_dir();
                }
                continue;
            }
            // Nested repositories have their own load context. Loaded files and editor
            // buffers enter below through the loader's established repository identity.
            if entry.depth() > 0
                && entry.file_type().is_dir()
                && crate::document::is_repository_root(entry.path())
            {
                walk.skip_current_dir();
                continue;
            }
            if entry.file_type().is_file()
                && crate::document::source_kind(&self.workspace, entry.path(), &["star", "sky"])
                    .is_some()
            {
                paths.push(entry.into_path());
            }
        }
        paths.sort_unstable();
        paths.dedup();
        for path in paths {
            self.analysis_snapshot.check_cancelled()?;
            let Some((dialect, context)) =
                crate::document::source_kind(&self.workspace, &path, &["star", "sky"])
            else {
                continue;
            };
            let open = self.analysis_snapshot.open_file(&path)?;
            let contents = match open {
                Some(file) => self.analysis_snapshot.source(file)?.text.to_string(),
                None => match std::fs::read_to_string(&path) {
                    Ok(contents) => contents,
                    Err(error) => {
                        if error.kind() == std::io::ErrorKind::NotFound {
                            continue;
                        }
                        return Err(error.into());
                    }
                },
            };
            // Escaped load strings may spell an export without its literal name.
            if !contents.contains(name) && !contents.contains('\\') {
                continue;
            }
            let file = match open {
                Some(file) => file,
                None => self.analysis_snapshot.file(
                    &path,
                    dialect,
                    context.map(|api_context| FileInfo::Bazel {
                        api_context,
                        is_external: false,
                    }),
                )??,
            };
            files.push(file);
        }
        files.sort_by(|left, right| {
            self.analysis_snapshot
                .path(*left)
                .cmp(self.analysis_snapshot.path(*right))
        });
        files.dedup_by_key(|file| file.source);
        Ok(files)
    }
}

impl panic::RefUnwindSafe for ServerSnapshot {}

pub(crate) fn load_bazel_builtins() -> Builtins {
    let data = include_bytes!("builtin/builtin.pb");

    // We want to crash if the bundled protobuf file is ever invalid.
    decode_builtins(&data[..]).expect("bug: invalid builtin.pb")
}

pub(crate) fn load_bazel_build_language(
    client: &dyn BazelClient,
) -> anyhow::Result<starpls_bazel::build::BuildLanguage> {
    let build_language_output = client.build_language()?;
    decode_rules(&build_language_output)
}

pub(crate) fn is_bazel_dependency(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            crate::document::DEPENDENCY_FILES.contains(&name) || name.ends_with(".MODULE.bazel")
        })
}

fn escape_glob(name: &str) -> String {
    name.chars()
        .flat_map(|character| match character {
            '*' | '?' | '[' | ']' | '{' | '}' => vec!['[', character, ']'],
            character => vec![character],
        })
        .collect()
}
