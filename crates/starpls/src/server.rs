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
    RegisterFileWatchers,
}

pub(crate) struct Server {
    pub(crate) config: Arc<ServerConfig>,
    pub(crate) connection: Connection,
    pub(crate) req_queue: ReqQueue<(), OutgoingRequest>,
    pub(crate) task_pool_handle: TaskPoolHandle<Task>,
    pub(crate) workspace: PathBuf,
    pub(crate) analysis_changed: bool,
    pub(crate) diagnostics_manager: DiagnosticsManager,
    pub(crate) analysis: Analysis,
    pub(crate) analysis_debouncer: AnalysisDebouncer,
    pub(crate) analysis_requested_for_files: Option<Vec<File>>,
    pub(crate) bazel_client: Arc<dyn BazelClient>,
    pub(crate) pending_repos: FxHashSet<String>,
    pub(crate) fetched_repos: FxHashSet<String>,
    pub(crate) is_fetching_repos: bool,
    pub(crate) is_refreshing_all_workspace_targets: bool,
    pub(crate) bzlmod_enabled: bool,
}

pub(crate) struct ServerSnapshot {
    pub(crate) config: Arc<ServerConfig>,
    pub(crate) analysis_snapshot: AnalysisSnapshot,
}

impl Server {
    pub(crate) fn new(connection: Connection, config: ServerConfig) -> anyhow::Result<Self> {
        // Create the task pool for processing incoming requests.
        let (task_pool_sender, task_pool_receiver) = crossbeam_channel::unbounded();
        let task_pool = TaskPool::with_num_threads(task_pool_sender.clone(), 4)?;
        let task_pool_handle = TaskPoolHandle::new(task_pool_receiver, task_pool);

        // Check if the user specified a path to the Bazel executable.
        let bazel_path = config
            .args
            .bazel_path
            .clone()
            .unwrap_or("bazel".to_string());

        debug!(
            "fetching Bazel configuration using Bazel executable at {:?}",
            bazel_path
        );

        // Determine Bazel configuration.
        let mut has_bazel_init_err = false;
        let bazel_client = Arc::new(BazelCLI::new(&bazel_path));
        let bazel_cx = match BazelContext::new(&*bazel_client) {
            Ok(cx) => cx,
            Err(err) => {
                has_bazel_init_err = true;
                error!("failed to initialize Bazel context: {}", err);
                Default::default()
            }
        };

        // Query for all targets in the current workspace, to use for label completion.
        let targets = if config.args.enable_label_completions {
            debug!("querying for all targets in the current workspace");
            match bazel_client.query_all_workspace_targets() {
                Ok(targets) => {
                    debug!("successfully queried for all targets");
                    targets
                }
                Err(err) => {
                    error!("failed to query all workspace targets: {}", err);
                    has_bazel_init_err = true;
                    Default::default()
                }
            }
        } else {
            Default::default()
        };

        let loader = Arc::new(DefaultFileLoader::new(
            bazel_client.clone(),
            bazel_cx.info.workspace.clone(),
            bazel_cx.info.workspace_name,
            bazel_cx.info.output_base.join("external"),
            task_pool_sender.clone(),
            bazel_cx.bzlmod_enabled,
        ));
        let mut analysis = Analysis::new(
            loader.clone(),
            InferenceOptions {
                infer_ctx_attributes: config.args.inference_options.infer_ctx_attributes,
                use_code_flow_analysis: config.args.inference_options.use_code_flow_analysis,
                ..Default::default()
            },
        )?;

        if let Err(error) =
            config
                .args
                .type_interfaces
                .install(&mut analysis, &loader, &bazel_cx.info.workspace)
        {
            connection.sender.send(
                lsp_server::Notification::new(
                    "window/showMessage".to_owned(),
                    lsp_types::ShowMessageParams {
                        typ: lsp_types::MessageType::ERROR,
                        message: format!("{error:#}"),
                    },
                )
                .into(),
            )?;
            return Err(error);
        }
        analysis.set_all_workspace_targets(targets);
        analysis.set_builtin_defs(load_bazel_builtins(), bazel_cx.rules)?;

        // Check for a prelude file. We skip verifying that `//tools/build_tools` is actually a package (i.e.
        // that it actually contains a `BUILD.bazel`) file for simplicity.
        let prelude = bazel_cx
            .info
            .workspace
            .join("tools/build_rules/prelude_bazel");
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
        let server = Server {
            config: Arc::new(config),
            connection,
            req_queue: Default::default(),
            task_pool_handle,
            workspace: bazel_cx.info.workspace,
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
            fetched_repos: Default::default(),
            is_fetching_repos: false,
            is_refreshing_all_workspace_targets: false,
            bzlmod_enabled: bazel_cx.bzlmod_enabled,
        };

        if has_bazel_init_err {
            server.send_error_message(BAZEL_INIT_ERR_MESSAGE);
        }

        Ok(server)
    }

    pub(crate) fn snapshot(&self) -> ServerSnapshot {
        ServerSnapshot {
            config: self.config.clone(),
            analysis_snapshot: self.analysis.snapshot(),
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

    pub(crate) fn complete_request(&mut self, resp: lsp_server::Response) {
        if let Some(OutgoingRequest::RegisterFileWatchers) =
            self.req_queue.outgoing.complete(resp.id)
        {
            if let Some(error) = resp.error {
                self.send_error_message(&format!("Cannot watch type interfaces: {}. Changes to closed files will not be reported.", error.message));
            } else {
                // Reconcile changes made between initial reads and watcher activation.
                self.analysis.invalidate_loads();
                self.invalidate_diagnostics();
            }
        }
    }

    pub(crate) fn watch_type_interfaces(&mut self) -> anyhow::Result<()> {
        let interfaces = self.analysis.type_interface_files();
        if interfaces.is_empty() {
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
            self.send_error_message("The client cannot watch type interfaces. Changes to closed files require restarting the server.");
            return Ok(());
        }
        let relative_patterns = capability
            .and_then(|watching| watching.relative_pattern_support)
            .unwrap_or(false);
        let snapshot = self.analysis.snapshot();
        let mut watchers = vec![lsp_types::FileSystemWatcher {
            glob_pattern: lsp_types::GlobPattern::String("**/*.{bzl,bzli}".to_owned()),
            kind: None,
        }];
        let mut external_paths = FxHashSet::default();
        for file in interfaces
            .into_iter()
            .chain(self.analysis.type_interface_sources())
        {
            let path = snapshot.path(file);
            if path.starts_with(&self.workspace) || !external_paths.insert(path) {
                continue;
            }
            if !relative_patterns {
                self.send_error_message("The client cannot watch type interfaces outside the workspace without relative-pattern support. Changes to closed files require restarting the server.");
                return Ok(());
            }
            let name = path
                .file_name()
                .expect("configured file has a name")
                .to_string_lossy();
            let pattern: String = name
                .chars()
                .flat_map(|character| match character {
                    '*' | '?' | '[' | ']' | '{' | '}' => vec!['[', character, ']'],
                    character => vec![character],
                })
                .collect();
            let parent = path.parent().expect("configured file is absolute");
            let base_uri =
                lsp_types::Url::from_directory_path(parent).expect("absolute directory URI");
            watchers.push(lsp_types::FileSystemWatcher {
                glob_pattern: lsp_types::GlobPattern::Relative(lsp_types::RelativePattern {
                    base_uri: lsp_types::OneOf::Right(base_uri),
                    pattern,
                }),
                kind: None,
            });
        }
        let params = lsp_types::RegistrationParams {
            registrations: vec![lsp_types::Registration {
                id: "starpls-type-interfaces".to_owned(),
                method: lsp_types::notification::DidChangeWatchedFiles::METHOD.to_owned(),
                register_options: Some(serde_json::to_value(
                    lsp_types::DidChangeWatchedFilesRegistrationOptions { watchers },
                )?),
            }],
        };
        let request = self.req_queue.outgoing.register(
            lsp_types::request::RegisterCapability::METHOD.to_owned(),
            params,
            OutgoingRequest::RegisterFileWatchers,
        );
        self.send(request.into());
        Ok(())
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

        self.is_fetching_repos = true;
        self.fetched_repos.extend(repos.clone());
        self.task_pool_handle.spawn_with_sender(move |sender| {
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
                .send(Task::FetchExternalRepos(FetchExternalReposProgress::End(
                    failed_repos,
                )))
                .unwrap();
        });
    }

    pub(crate) fn refresh_all_workspace_targets(&mut self) {
        if self.is_refreshing_all_workspace_targets || !self.config.args.enable_label_completions {
            return;
        }

        let bazel_client = self.bazel_client.clone();

        self.is_refreshing_all_workspace_targets = true;
        self.task_pool_handle.spawn_with_sender(move |sender| {
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
                    RefreshAllWorkspaceTargetsProgress::End(targets),
                ))
                .unwrap();
        });
    }
}

impl panic::RefUnwindSafe for ServerSnapshot {}

pub(crate) fn load_bazel_builtins() -> Builtins {
    let data = include_bytes!("builtin/builtin.pb");

    // We want to crash if the bundled protobuf file is ever invalid.
    decode_builtins(&data[..]).expect("bug: invalid builtin.pb")
}

pub(crate) fn load_bazel_build_language(client: &dyn BazelClient) -> anyhow::Result<Builtins> {
    let build_language_output = client.build_language()?;
    decode_rules(&build_language_output)
}
