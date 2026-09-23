use crossbeam_channel::select;
use log::debug;
use lsp_server::Connection;
use lsp_types::InitializeParams;
use lsp_types::WorkDoneProgressCreateParams;
use rustc_hash::FxHashSet;
use starpls_common::File;

use crate::commands::server::ServerCommand;
use crate::config::ServerConfig;
use crate::convert;
use crate::dispatcher::RequestDispatcher;
use crate::extensions;
use crate::handlers::notifications;
use crate::handlers::requests;
use crate::server::Server;
use crate::server::ServerSnapshot;

#[macro_export]
macro_rules! match_notification {
    (match $node:ident { $($tt:tt)* }) => { $crate::match_notification!(match ($node) { $($tt)* }) };

    (match ($node:expr) {
        $( if $path:path as $it:pat => $res:expr, )*
        _ => $catch_all:expr $(,)?
    }) => {{
        $( if let Some($it) = cast_notification::<$path>(&$node) { $res } else )*
        { $catch_all }
    }};
}

#[derive(Debug)]
pub(crate) enum FetchExternalReposProgress {
    Begin(FxHashSet<String>),
    End {
        revision: u64,
        results: Vec<crate::document::RepositoryFetchResult>,
    },
}

#[derive(Debug)]
pub(crate) struct FetchExternalRepoRequest {
    pub(crate) repo: String,
    pub(crate) revision: u64,
}

#[derive(Debug)]
pub(crate) enum RefreshAllWorkspaceTargetsProgress {
    Begin,
    End {
        revision: u64,
        targets: Option<Vec<String>>,
    },
}

#[derive(Debug)]
pub(crate) enum Task {
    AnalysisRequested(Vec<File>),
    /// A new set of diagnostics has been processed and is ready for forwarding.
    DiagnosticsReady(
        Vec<(
            File,
            crate::diagnostics::DiagnosticTicket,
            Vec<lsp_types::Diagnostic>,
        )>,
    ),
    /// A request has been evaluated and its response is ready.
    ResponseReady {
        revision: u64,
        request: lsp_server::Request,
        response: lsp_server::Response,
    },
    ConfigurationReady(crate::server::ConfigurationReady),
    /// Wake the event loop to batch pending mappings after draining its tasks.
    ResolveRepoMappings,
    RepoMappingsReady {
        repositories: Vec<String>,
        revision: u64,
        result: anyhow::Result<Vec<starpls_bazel::client::RepoMapping>>,
    },
    /// Retry a previously failed request (e.g. due to Salsa cancellation).
    Retry(lsp_server::Request),
    /// Events from fetching external repositories.
    FetchExternalRepos(FetchExternalReposProgress),
    /// A request to fetch an external repository.
    FetchExternalRepoRequest(FetchExternalRepoRequest),
    /// Events from refreshing targets for the current workspace.
    RefreshAllWorkspaceTargets(RefreshAllWorkspaceTargetsProgress),
}

#[derive(Debug)]
pub(crate) enum Event {
    Message(lsp_server::Message),
    Task(Task),
}

pub fn process_connection(
    connection: Connection,
    args: ServerCommand,
    initialize_params: InitializeParams,
) -> anyhow::Result<()> {
    debug!("initializing state and starting event loop");
    #[allow(deprecated)]
    let root = initialize_params
        .workspace_folders
        .as_ref()
        .and_then(|folders| folders.first())
        .map(|folder| &folder.uri)
        .or(initialize_params.root_uri.as_ref());
    let root = match root {
        Some(uri) => crate::convert::path_buf_from_url(uri)?,
        None => std::env::current_dir()?,
    };
    let root = root.canonicalize()?;
    let workspace = root
        .ancestors()
        .find(|path| crate::document::is_repository_root(path))
        .unwrap_or(&root)
        .to_path_buf();
    let config = ServerConfig {
        args,
        caps: initialize_params.capabilities,
        workspace,
    };
    let server = Server::new(connection, config)?;
    server.run()
}

impl Server {
    fn run(mut self) -> anyhow::Result<()> {
        // Connection::initialize has already consumed the initialized notification.
        self.watch_type_interfaces()?;
        if std::mem::take(&mut self.analysis_changed) {
            self.analysis_debouncer
                .sender
                .send(self.analysis.diagnostic_files())?;
        }
        while let Some(event) = self.next_event() {
            if let Event::Message(lsp_server::Message::Request(ref req)) = event {
                if self.connection.handle_shutdown(req)? {
                    return Ok(());
                }
            }

            self.handle_event(event)?;
        }
        Ok(())
    }

    fn next_event(&self) -> Option<Event> {
        let event = select! {
            recv(self.connection.receiver) -> req => req.ok().map(Event::Message),
            recv(self.task_pool_handle.receiver) -> task => Some(Event::Task(task.unwrap())),
        };
        event
    }

    fn handle_event(&mut self, event: Event) -> anyhow::Result<()> {
        match event {
            Event::Message(lsp_server::Message::Request(req)) => {
                self.register_and_handle_request(req);
            }
            Event::Message(lsp_server::Message::Notification(not)) => {
                self.handle_notification(not)?;
            }
            Event::Message(lsp_server::Message::Response(resp)) => {
                self.complete_request(resp);
            }
            Event::Task(task) => {
                self.handle_task(task);

                while let Ok(task) = self.task_pool_handle.receiver.try_recv() {
                    self.handle_task(task);
                }
            }
        };

        if let Err(error) = self.watch_type_interfaces() {
            self.send_error_message(&format!("{error:#}"));
        }
        if self.configuration.refreshing {
            return Ok(());
        }
        self.resolve_repository_mappings();
        if !self.pending_repos.is_empty() && !self.is_fetching_repos {
            self.fetch_bazel_external_repos();
        }

        // Update our diagnostics if a triggering event (e.g. document open/close/change) occured.
        // This is done asynchronously, so any new diagnostics resulting from this won't be seen until the next turn
        // of the event loop.
        if std::mem::take(&mut self.analysis_changed) {
            self.analysis_requested_for_files = None;
            self.analysis_debouncer
                .sender
                .send(self.analysis.diagnostic_files())
                .unwrap();
        } else if let Some(file_ids) = self.analysis_requested_for_files.take() {
            self.update_diagnostics(file_ids);
        }

        Ok(())
    }

    fn resolve_repository_mappings(&mut self) {
        if self.is_resolving_repo_mappings || self.configuration.refreshing {
            return;
        }
        let repositories = self.loader.pending_repository_mappings();
        if repositories.is_empty() {
            return;
        }
        self.is_resolving_repo_mappings = true;
        let revision = self.configuration.revision;
        let client = self.bazel_client.clone();
        self.bazel_task_pool.spawn(move || {
            let names: Vec<_> = repositories.iter().map(String::as_str).collect();
            let result = client.dump_repo_mappings(&names);
            Task::RepoMappingsReady {
                repositories,
                revision,
                result,
            }
        });
    }

    fn update_diagnostics(&mut self, file_ids: Vec<File>) {
        let snapshot = self.snapshot();
        let jobs: Vec<_> = file_ids
            .into_iter()
            .filter_map(|file| {
                self.diagnostics_manager
                    .request(&snapshot.analysis_snapshot, file)
            })
            .collect();
        self.task_pool_handle.spawn(move || {
            let results = jobs
                .into_iter()
                .filter_map(|(file, ticket)| {
                    let diagnostics = collect_diagnostics(&snapshot, file)?;
                    Some((file, ticket, diagnostics))
                })
                .collect();
            Task::DiagnosticsReady(results)
        });
    }

    fn register_and_handle_request(&mut self, req: lsp_server::Request) {
        self.req_queue.incoming.register(req.id.clone(), ());
        self.handle_request(req);
    }

    fn handle_request(&mut self, req: lsp_server::Request) {
        RequestDispatcher::new(req, self)
            .on::<extensions::ShowSyntaxTree>(requests::show_syntax_tree)
            .on::<extensions::ShowHir>(requests::show_hir)
            .on::<lsp_types::request::Completion>(requests::completion)
            .on::<lsp_types::request::DocumentSymbolRequest>(requests::document_symbols)
            .on::<lsp_types::request::GotoDefinition>(requests::goto_definition)
            .on::<lsp_types::request::GotoDeclaration>(requests::goto_declaration)
            .on::<lsp_types::request::HoverRequest>(requests::hover)
            .on::<lsp_types::request::DocumentHighlightRequest>(requests::document_highlights)
            .on::<lsp_types::request::SelectionRangeRequest>(requests::selection_ranges)
            .on::<lsp_types::request::FoldingRangeRequest>(requests::folding_ranges)
            .on::<lsp_types::request::InlayHintRequest>(requests::inlay_hints)
            .on::<lsp_types::request::References>(requests::find_references)
            .on::<lsp_types::request::PrepareRenameRequest>(requests::prepare_rename)
            .on::<lsp_types::request::Rename>(requests::rename)
            .on::<lsp_types::request::SemanticTokensFullRequest>(requests::semantic_tokens)
            .on::<lsp_types::request::SemanticTokensRangeRequest>(requests::semantic_tokens_range)
            .on::<lsp_types::request::SignatureHelpRequest>(requests::signature_help)
            .finish();
    }

    fn handle_notification(&mut self, not: lsp_server::Notification) -> anyhow::Result<()> {
        match_notification! {
            match not {
                if lsp_types::notification::DidOpenTextDocument as params => notifications::did_open_text_document(self, params),
                if lsp_types::notification::DidCloseTextDocument as params => notifications::did_close_text_document(self, params),
                if lsp_types::notification::DidChangeTextDocument as params => notifications::did_change_text_document(self, params),
                if lsp_types::notification::DidSaveTextDocument as params => notifications::did_save_text_document(self, params),
                if lsp_types::notification::DidChangeWatchedFiles as params => notifications::did_change_watched_files(self, params),
                _ => Ok(())
            }
        }
    }

    fn handle_task(&mut self, task: Task) {
        match task {
            Task::AnalysisRequested(file_ids) => self.analysis_requested_for_files = Some(file_ids),
            Task::DiagnosticsReady(results) => {
                let snapshot = self.analysis.snapshot();
                for (file, ticket, diagnostics) in results {
                    let path = snapshot.path(file);
                    if self.diagnostics_manager.complete(&snapshot, file, ticket) {
                        let uri = lsp_types::Url::from_file_path(path).expect("absolute file path");
                        self.send_notification::<lsp_types::notification::PublishDiagnostics>(
                            lsp_types::PublishDiagnosticsParams {
                                uri,
                                diagnostics,
                                version: snapshot
                                    .document(path)
                                    .filter(|document| document.path.as_std_path() == path)
                                    .map(|document| document.version),
                            },
                        );
                    }
                }
            }
            Task::ResponseReady {
                revision,
                request,
                response,
            } => {
                if revision == self.configuration.revision {
                    self.respond(response);
                } else {
                    self.handle_request(request);
                }
            }
            Task::ConfigurationReady(ready) => self.finish_configuration_refresh(ready),
            Task::ResolveRepoMappings => {}
            Task::RepoMappingsReady {
                repositories,
                revision,
                result,
            } => {
                self.is_resolving_repo_mappings = false;
                if revision == self.configuration.revision && !self.configuration.refreshing {
                    // Drain readers while the lookups are still pending, so a
                    // workspace edit cannot accept references from that state.
                    self.analysis.invalidate_loads();
                    if let Err(error) = self
                        .loader
                        .finish_repository_mappings(&repositories, result)
                    {
                        self.send_error_message(&format!(
                            "Cannot load repository mapping batch: {error:#}"
                        ));
                    }
                    self.invalidate_diagnostics();
                    self.refresh_editor_semantics();
                }
            }
            Task::Retry(req) => self.handle_request(req),
            Task::FetchExternalRepos(progress) => {
                let token = "FetchExternalRepos".to_string();
                let work_done = match progress {
                    FetchExternalReposProgress::Begin(repos) => {
                        self.send_request::<lsp_types::request::WorkDoneProgressCreate>(
                            WorkDoneProgressCreateParams {
                                token: lsp_types::NumberOrString::String(token.clone()),
                            },
                        );

                        let mut repos = repos.into_iter().collect::<Vec<_>>();
                        repos.sort();

                        let mut title = "Fetching external repositories: ".to_string();
                        for (i, repo) in repos.into_iter().enumerate() {
                            if i > 0 {
                                title.push_str(", ");
                            }
                            title.push('"');
                            title.push_str(&repo);
                            title.push('"');
                        }

                        lsp_types::WorkDoneProgress::Begin(lsp_types::WorkDoneProgressBegin {
                            title,
                            ..Default::default()
                        })
                    }
                    FetchExternalReposProgress::End { revision, results } => {
                        self.is_fetching_repos = false;
                        let mut failed_repos = Vec::new();
                        if revision == self.configuration.revision {
                            self.analysis.invalidate_loads();
                            for crate::document::RepositoryFetchResult { name, result } in results {
                                if result.is_err() {
                                    failed_repos.push(name.clone());
                                }
                                self.loader.finish_fetch([name], result);
                            }
                            self.invalidate_diagnostics();
                            if self.open_repository_changed() {
                                if let Err(error) = self.reload_configuration() {
                                    self.send_error_message(&format!("{error:#}"));
                                }
                            } else {
                                self.refresh_editor_semantics();
                            }
                        }

                        // Fetching external repositories with `bazel query`, as in the case when bzlmod is disabled, often
                        // results in a non-zero exit code because of errors that we don't really care about. Therefore, to
                        // avoid noise, we only send an error message when fetching with `bazel fetch`, which is the case
                        // when bzlmod is enabled.
                        if !failed_repos.is_empty() && self.bzlmod_enabled {
                            self.send_error_message(&format!(
                                "Failed to fetch external repositories: {}. Please check the server logs for more details.",
                                failed_repos.join(", ")
                            ));
                        }

                        lsp_types::WorkDoneProgress::End(lsp_types::WorkDoneProgressEnd {
                            message: None,
                        })
                    }
                };

                self.send_notification::<lsp_types::notification::Progress>(
                    lsp_types::ProgressParams {
                        token: lsp_types::NumberOrString::String(token),
                        value: lsp_types::ProgressParamsValue::WorkDone(work_done),
                    },
                );
            }
            Task::FetchExternalRepoRequest(FetchExternalRepoRequest { repo, revision }) => {
                if revision == self.configuration.revision && self.loader.begin_fetch(repo.clone())
                {
                    self.pending_repos.insert(repo);
                }
            }
            Task::RefreshAllWorkspaceTargets(progress) => {
                let token = "RefreshAllWorkspaceTargets";
                let work_done = match progress {
                    RefreshAllWorkspaceTargetsProgress::Begin => {
                        self.send_request::<lsp_types::request::WorkDoneProgressCreate>(
                            WorkDoneProgressCreateParams {
                                token: lsp_types::NumberOrString::String(token.to_string()),
                            },
                        );

                        lsp_types::WorkDoneProgress::Begin(lsp_types::WorkDoneProgressBegin {
                            title: "Refreshing all workspace targets".to_string(),
                            ..Default::default()
                        })
                    }
                    RefreshAllWorkspaceTargetsProgress::End { revision, targets } => {
                        self.is_refreshing_all_workspace_targets = false;
                        if revision == self.configuration.revision {
                            if self.open_repository_changed() {
                                if let Err(error) = self.reload_configuration() {
                                    self.send_error_message(&format!("{error:#}"));
                                }
                            } else if let Some(targets) = targets {
                                self.analysis.set_all_workspace_targets(targets);
                                self.analysis.invalidate_loads();
                                self.invalidate_diagnostics();
                                self.refresh_editor_semantics();
                            }
                        } else {
                            self.refresh_all_workspace_targets();
                        }

                        lsp_types::WorkDoneProgress::End(lsp_types::WorkDoneProgressEnd {
                            message: None,
                        })
                    }
                };

                self.send_notification::<lsp_types::notification::Progress>(
                    lsp_types::ProgressParams {
                        token: lsp_types::NumberOrString::String(token.to_string()),
                        value: lsp_types::ProgressParamsValue::WorkDone(work_done),
                    },
                );
            }
        }
    }

    fn respond(&mut self, resp: lsp_server::Response) {
        if self.req_queue.incoming.complete(resp.id.clone()).is_some() {
            self.connection.sender.send(resp.into()).unwrap();
        }
    }
}

fn cast_notification<R>(not: &lsp_server::Notification) -> Option<R::Params>
where
    R: lsp_types::notification::Notification,
    R::Params: serde::de::DeserializeOwned,
{
    if not.method == R::METHOD {
        let params = serde_json::from_value(not.params.clone()).expect("invalid JSON");
        Some(params)
    } else {
        None
    }
}

fn collect_diagnostics(
    snapshot: &ServerSnapshot,
    file_id: File,
) -> Option<Vec<lsp_types::Diagnostic>> {
    let source = snapshot.analysis_snapshot.source(file_id).ok()?;

    // Get the diagnostics for the current path. If the operation was cancelled, simply continue to the next file.
    let diagnostics = snapshot.analysis_snapshot.diagnostics(file_id).ok()?;

    // Convert the diagnostics. This includes translating text offsets into `(line, column)` format.
    Some(
        diagnostics
            .into_iter()
            .flat_map(|diagnostic| convert::lsp_diagnostic_from_native(diagnostic, &source))
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use crossbeam_channel::Receiver;
    use crossbeam_channel::Sender;
    use lsp_server::Connection;
    use lsp_types::notification::Notification;
    use ruff_db::system::InMemorySystem;
    use ruff_db::system::SystemPath;
    use ruff_db::system::WritableSystem;
    use starpls_bazel::client::BazelCLI;
    use starpls_common::File;
    use starpls_ide::Analysis;

    use super::collect_diagnostics;
    use super::Event;
    use super::FetchExternalReposProgress;
    use super::RefreshAllWorkspaceTargetsProgress;
    use super::Task;
    use crate::config::ServerConfig;
    use crate::debouncer::AnalysisDebouncer;
    use crate::document::DefaultFileLoader;
    use crate::server::Server;
    use crate::task_pool::TaskPool;
    use crate::task_pool::TaskPoolHandle;

    struct TestServer {
        server: Server,
        client: Connection,
        disk: InMemorySystem,
        tasks: Sender<Task>,
        debounced: Receiver<Vec<File>>,
    }

    fn server() -> TestServer {
        let (connection, client) = Connection::memory();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let (debounce_sender, debounced) = crossbeam_channel::unbounded();
        let pool = TaskPool::with_num_threads(sender.clone(), 1).unwrap();
        let bazel_client = Arc::new(BazelCLI::default());
        let loader = DefaultFileLoader::new(
            bazel_client.clone(),
            "/workspace".into(),
            None,
            std::path::PathBuf::from("/external"),
            sender.clone(),
            false,
        );
        let disk = InMemorySystem::default();
        disk.create_directory_all(SystemPath::new("/workspace"))
            .unwrap();
        let loader = Arc::new(loader);
        let analysis = Analysis::with_system(loader.clone(), Default::default(), disk.clone());
        let server = Server {
            config: Arc::new(ServerConfig {
                args: Default::default(),
                caps: Default::default(),
                workspace: "/workspace".into(),
            }),
            connection,
            req_queue: Default::default(),
            task_pool_handle: TaskPoolHandle::new(receiver, pool),
            bazel_task_pool: TaskPool::with_num_threads(sender.clone(), 1).unwrap(),
            workspace: "/workspace".into(),
            analysis_changed: false,
            diagnostics_manager: Default::default(),
            analysis,
            // Capture the actual roots chosen by the event loop, then deliver
            // them without depending on a timer or worker scheduling order.
            analysis_debouncer: AnalysisDebouncer {
                sender: debounce_sender,
            },
            analysis_requested_for_files: None,
            bazel_client,
            pending_repos: Default::default(),
            is_fetching_repos: false,
            is_resolving_repo_mappings: false,
            is_refreshing_all_workspace_targets: false,
            bzlmod_enabled: false,
            loader,
            configuration: Default::default(),
        };
        TestServer {
            server,
            client,
            disk,
            tasks: sender,
            debounced,
        }
    }

    #[test]
    fn semantic_refresh_follows_capabilities_and_accepted_results() {
        {
            let TestServer {
                mut server, client, ..
            } = server();
            server.refresh_editor_semantics();
            assert!(client.receiver.try_recv().is_err());
            server.is_fetching_repos = true;
            server.handle_task(Task::FetchExternalRepos(FetchExternalReposProgress::End {
                revision: 1,
                results: vec![
                    crate::document::RepositoryFetchResult {
                        name: "old-success+".to_owned(),
                        result: Ok(()),
                    },
                    crate::document::RepositoryFetchResult {
                        name: "old-failure+".to_owned(),
                        result: Err("old configuration failure".to_owned()),
                    },
                ],
            }));
            assert!(!server.is_fetching_repos);
            assert!(server.loader.begin_fetch("old-success+".to_owned()));
            assert!(server.loader.begin_fetch("old-failure+".to_owned()));
        }
        for (tokens, hints) in [
            (None, None),
            (Some(false), Some(false)),
            (Some(true), None),
            (None, Some(true)),
            (Some(true), Some(true)),
        ] {
            let TestServer {
                mut server, client, ..
            } = server();
            Arc::get_mut(&mut server.config).unwrap().caps =
                serde_json::from_value(serde_json::json!({"workspace": {
                    "semanticTokens": {"refreshSupport": tokens},
                    "inlayHint": {"refreshSupport": hints},
                }}))
                .unwrap();
            let requests = || {
                client
                    .receiver
                    .try_iter()
                    .filter_map(|message| {
                        let lsp_server::Message::Request(request) = message else {
                            return None;
                        };
                        Some(request.method)
                    })
                    .collect::<Vec<_>>()
            };
            server.handle_task(Task::RepoMappingsReady {
                repositories: vec![String::new()],
                revision: 1,
                result: Ok(vec![Default::default()]),
            });
            assert!(requests().is_empty(), "stale mapping must not refresh");
            server.handle_task(Task::RepoMappingsReady {
                repositories: vec![String::new()],
                revision: 0,
                result: Ok(vec![Default::default()]),
            });
            let expected = [
                (tokens, "workspace/semanticTokens/refresh"),
                (hints, "workspace/inlayHint/refresh"),
            ]
            .into_iter()
            .filter_map(|(enabled, method)| (enabled == Some(true)).then_some(method))
            .collect::<Vec<_>>();
            assert_eq!(requests(), expected);
            server.invalidate_diagnostics();
            assert!(
                requests().is_empty(),
                "ordinary edits use document requests"
            );
        }
    }

    struct BlockedClient {
        workspace: std::path::PathBuf,
        operation: &'static str,
        gate: std::sync::Mutex<Option<(Sender<()>, Receiver<()>)>>,
        fail: bool,
        mapping_requests: std::sync::Mutex<Vec<Vec<String>>>,
    }

    impl BlockedClient {
        fn block(&self, operation: &str) -> anyhow::Result<()> {
            if operation == self.operation {
                if let Some((entered, resume)) = self.gate.lock().unwrap().take() {
                    entered.send(())?;
                    resume.recv()?;
                }
                if self.fail {
                    anyhow::bail!("blocked Bazel command failed");
                }
            }
            Ok(())
        }
    }

    impl starpls_bazel::client::BazelClient for BlockedClient {
        fn info(&self) -> anyhow::Result<starpls_bazel::client::BazelInfo> {
            self.block("info")?;
            Ok(starpls_bazel::client::BazelInfo {
                workspace: self.workspace.clone(),
                output_base: self.workspace.join("output"),
                release: "release 9".to_owned(),
                starlark_semantics: String::new(),
                workspace_name: None,
            })
        }
        fn build_language(&self) -> anyhow::Result<Vec<u8>> {
            Ok(Vec::new())
        }
        fn dump_repo_mappings(
            &self,
            repositories: &[&str],
        ) -> anyhow::Result<Vec<starpls_bazel::client::RepoMapping>> {
            self.mapping_requests
                .lock()
                .unwrap()
                .push(repositories.iter().map(|name| (*name).to_owned()).collect());
            repositories
                .iter()
                .map(|repository| {
                    self.block(repository)?;
                    Ok(Arc::new([("child".to_owned(), "child+".to_owned())].into()))
                })
                .collect()
        }
        fn fetch_repos(&self, _: &[&str]) -> anyhow::Result<()> {
            Ok(())
        }
        fn null_query_external_repo_targets(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn query_all_workspace_targets(&self) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
        fn selected_module(
            &self,
            _: &str,
        ) -> anyhow::Result<Option<starpls_bazel::client::SelectedModule>> {
            Ok(None)
        }
    }

    fn hover(
        server: &mut Server,
        client: &Connection,
        path: &Path,
        line: u32,
        id: i32,
    ) -> serde_json::Value {
        server.register_and_handle_request(lsp_server::Request::new(
            id.into(),
            "textDocument/hover".into(),
            serde_json::json!({
                "textDocument": {"uri": lsp_types::Url::from_file_path(path).unwrap()},
                "position": {"line": line, "character": 1},
            }),
        ));
        let response = response(server, client, id);
        assert!(response.error.is_none(), "{response:?}");
        response.result.unwrap()
    }

    fn change(server: &mut Server, path: &Path, text: &str) {
        server
            .handle_event(
                notification::<lsp_types::notification::DidChangeTextDocument>(
                    lsp_types::DidChangeTextDocumentParams {
                        text_document: lsp_types::VersionedTextDocumentIdentifier {
                            uri: lsp_types::Url::from_file_path(path).unwrap(),
                            version: 2,
                        },
                        content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                            range: None,
                            range_length: None,
                            text: text.to_owned(),
                        }],
                    },
                ),
            )
            .unwrap();
    }

    #[test]
    fn local_requests_and_edits_run_while_startup_is_blocked() {
        use clap::Parser;
        for fail in [false, true] {
            let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
                .join(format!("blocked-startup-{fail}"));
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("MODULE.bazel"), "").unwrap();
            std::fs::write(
                root.join("source.bzl"),
                "def compute(value): return value\n",
            )
            .unwrap();
            let stub = root.join("source.bzli");
            std::fs::write(&stub, "def compute(value: int) -> int: ...\n").unwrap();
            let (entered, entering) = crossbeam_channel::bounded(1);
            let (resume, resumed) = crossbeam_channel::bounded(1);
            let bazel = Arc::new(BlockedClient {
                workspace: root.clone(),
                operation: "info",
                gate: std::sync::Mutex::new(Some((entered, resumed))),
                fail,
                mapping_requests: Default::default(),
            });
            let cli = crate::Cli::try_parse_from([
                "starpls",
                "server",
                "--type_interface",
                "source.bzl=source.bzli",
            ])
            .unwrap();
            let Some(crate::Commands::Server(args)) = cli.command else {
                panic!("server command");
            };
            let (connection, client) = Connection::memory();
            let mut server = Server::with_client(
                connection,
                ServerConfig {
                    args,
                    caps: Default::default(),
                    workspace: root.clone(),
                },
                bazel,
            )
            .unwrap();
            entering.recv_timeout(Duration::from_secs(10)).unwrap();
            let external = root.join("output/external/dependency+/open.bzl");
            std::fs::create_dir_all(external.parent().unwrap()).unwrap();
            std::fs::write(&external, "answer = 7\n").unwrap();
            server
                .open_document(&external, "answer = 'unsaved'\n".into(), 4)
                .unwrap();
            let local = root.join("local.bzl");
            server
                .open_document(&local, "value = 1\nrule\n".into(), 1)
                .unwrap();
            server
                .open_document(&stub, "def compute(value: str) -> str: ...\n".into(), 3)
                .unwrap();
            assert!(hover(&mut server, &client, &local, 0, 100)
                .to_string()
                .contains("Literal[1]"));
            assert!(hover(&mut server, &client, &local, 1, 101)
                .to_string()
                .contains("rule("));
            change(&mut server, &local, "value = 'updated'\nrule\n");
            assert!(hover(&mut server, &client, &local, 0, 102)
                .to_string()
                .contains("updated"));
            assert!(server.snapshot().reference_files("value").is_err());
            assert!(server.analysis.type_interface_files().is_empty());
            std::fs::write(root.join("MODULE.bazel"), "# changed during startup\n").unwrap();
            server
                .configuration_changed(&[root.join("MODULE.bazel")])
                .unwrap();
            resume.send(()).unwrap();
            finish_reload(&mut server);
            assert!(
                server.configuration.refreshing,
                "stale startup result must restart"
            );
            finish_reload(&mut server);
            assert_eq!(server.configuration.complete, !fail);
            assert_eq!(
                server.analysis.type_interface_files().len(),
                usize::from(!fail)
            );
            assert!(server
                .analysis
                .document(&stub)
                .unwrap()
                .contents
                .contains("str"));
            assert!(hover(&mut server, &client, &local, 0, 103)
                .to_string()
                .contains("updated"));
            assert_eq!(server.analysis.document(&local).unwrap().version, 2);
            if !fail {
                let snapshot = server.analysis.snapshot();
                let file = snapshot.open_file(&external).unwrap().unwrap();
                assert_eq!(file.is_external(), Some(true));
                assert!(snapshot
                    .document(snapshot.path(file))
                    .unwrap()
                    .contents
                    .contains("unsaved"));
                drop(snapshot);
                std::fs::write(root.join("starpls.toml"), "invalid configuration").unwrap();
                server
                    .configuration_changed(&[root.join("starpls.toml")])
                    .unwrap();
                finish_reload(&mut server);
                assert!(!server.snapshot().configuration_ready);
                assert!(server.analysis.type_interface_files().is_empty());
            }
            drop(server);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn mapping_waits_release_snapshots_and_report_failure() {
        for fail in [false, true] {
            let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
                .join(format!("blocked-mapping-{fail}"));
            let external = root.join("output/external");
            let dependency = external.join("dep+");
            let child = external.join("child+");
            std::fs::create_dir_all(&dependency).unwrap();
            std::fs::create_dir_all(&child).unwrap();
            std::fs::write(root.join("MODULE.bazel"), "").unwrap();
            std::fs::write(child.join("child.bzl"), "value = 42\n").unwrap();
            let source = dependency.join("dep.bzl");
            std::fs::write(&source, "load('@child//:child.bzl', 'value')\nvalue\n").unwrap();
            let (entered, entering) = crossbeam_channel::bounded(1);
            let (resume, resumed) = crossbeam_channel::bounded(1);
            let bazel = Arc::new(BlockedClient {
                workspace: root.clone(),
                operation: "dep+",
                gate: std::sync::Mutex::new(Some((entered, resumed))),
                fail,
                mapping_requests: Default::default(),
            });
            let (connection, client) = Connection::memory();
            let mut server = Server::with_client(
                connection,
                ServerConfig {
                    workspace: root.clone(),
                    ..Default::default()
                },
                bazel,
            )
            .unwrap();
            finish_reload(&mut server);
            server
                .loader
                .finish_fetch(["dep+".to_owned(), "child+".to_owned()], Ok(()));
            server
                .open_document(&source, std::fs::read_to_string(&source).unwrap(), 1)
                .unwrap();
            assert!(hover(&mut server, &client, &source, 1, 110)
                .to_string()
                .contains("Unknown"));
            loop {
                crossbeam_channel::select! {
                    recv(entering) -> entered => { entered.unwrap(); break; }
                    recv(server.task_pool_handle.receiver) -> task => server.handle_event(Event::Task(task.unwrap())).unwrap(),
                    default(Duration::from_secs(10)) => panic!("mapping did not start"),
                }
            }
            assert!(server.snapshot().reference_files("value").is_err());
            let local = root.join("local.bzl");
            server
                .open_document(&local, "value = 1\n".into(), 1)
                .unwrap();
            assert!(hover(&mut server, &client, &local, 0, 111)
                .to_string()
                .contains("Literal[1]"));
            change(&mut server, &local, "value = 'updated'\n");
            assert!(hover(&mut server, &client, &local, 0, 112)
                .to_string()
                .contains("updated"));
            resume.send(()).unwrap();
            loop {
                let task = server
                    .task_pool_handle
                    .receiver
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
                let mapping = matches!(
                    task,
                    Task::RepoMappingsReady {
                        repositories: _,
                        revision: _,
                        result: _
                    }
                );
                server.handle_task(task);
                if mapping {
                    break;
                }
            }
            if fail {
                assert!(client
                    .receiver
                    .try_iter()
                    .any(|message| serde_json::to_string(&message)
                        .unwrap()
                        .contains("blocked Bazel command failed")));
                let snapshot = server.snapshot();
                let file = snapshot
                    .analysis_snapshot
                    .open_file(&source)
                    .unwrap()
                    .unwrap();
                assert!(collect_diagnostics(&snapshot, file)
                    .unwrap()
                    .iter()
                    .any(|diagnostic| diagnostic.message.contains("blocked Bazel command failed")));
            } else {
                assert!(hover(&mut server, &client, &source, 1, 113)
                    .to_string()
                    .contains("Literal[42]"));
            }
            drop(server);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn mapping_batches_accumulate_requests_and_discard_stale_results() {
        for (stale, fail) in [(false, false), (false, true), (true, false)] {
            let TestServer {
                mut server,
                client,
                disk,
                tasks,
                debounced,
            } = server();
            let (entered, entering) = crossbeam_channel::bounded(1);
            let (resume, resumed) = crossbeam_channel::bounded(1);
            let bazel = Arc::new(BlockedClient {
                workspace: "/workspace".into(),
                operation: "a+",
                gate: std::sync::Mutex::new(Some((entered, resumed))),
                fail,
                mapping_requests: Default::default(),
            });
            server.bazel_client = bazel.clone();
            server.loader = Arc::new(
                DefaultFileLoader::new(
                    bazel.clone(),
                    "/workspace".into(),
                    None,
                    std::path::PathBuf::from("/external"),
                    tasks.clone(),
                    true,
                )
                .for_editor(0),
            );
            server.analysis =
                Analysis::with_system(server.loader.clone(), Default::default(), disk);
            let request = |loader: &DefaultFileLoader, name: &str| {
                loader.resolve_repository(
                    &starpls_bazel::Label::parse("@child//:defs.bzl").unwrap(),
                    &crate::document::Repository {
                        name: name.to_owned(),
                        root: std::path::Path::new("/external").join(name),
                    },
                )
            };
            for name in ["b+", "a+", "a+"] {
                assert!(request(&server.loader, name)
                    .unwrap_err()
                    .to_string()
                    .contains("loading"));
            }
            server
                .handle_event(Event::Task(Task::ResolveRepoMappings))
                .unwrap();
            entering.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(server.is_resolving_repo_mappings);
            assert_eq!(
                bazel.mapping_requests.lock().unwrap().as_slice(),
                &[vec!["a+".to_owned(), "b+".to_owned()]]
            );

            if stale {
                server.configuration.revision = 1;
                server.loader = Arc::new(server.loader.fresh().for_editor(1));
            }
            for name in ["c+", "d+", "c+"] {
                assert!(request(&server.loader, name).is_err());
            }
            server
                .handle_event(Event::Task(Task::ResolveRepoMappings))
                .unwrap();
            assert!(server.is_resolving_repo_mappings);
            assert_eq!(bazel.mapping_requests.lock().unwrap().len(), 1);
            resume.send(()).unwrap();
            let first = server
                .task_pool_handle
                .receiver
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
            assert!(
                matches!(
                    first,
                    Task::RepoMappingsReady {
                        repositories: _,
                        revision: _,
                        result: _
                    }
                ),
                "{first:?}"
            );
            // Handle completion separately to observe its publication before
            // the event boundary dispatches the next accumulated batch.
            server.handle_task(first);
            assert!(!server.is_resolving_repo_mappings);
            assert_eq!(server.analysis_changed, !stale);
            let errors = client
                .receiver
                .try_iter()
                .filter(|message| {
                    serde_json::to_string(message)
                        .unwrap()
                        .contains("blocked Bazel command failed")
                })
                .count();
            assert_eq!(errors, usize::from(fail && !stale));
            if !stale {
                for name in ["a+", "b+"] {
                    let result = request(&server.loader, name);
                    assert_eq!(result.is_err(), fail);
                    if let Err(error) = result {
                        assert!(error.to_string().contains("blocked Bazel command failed"));
                    }
                }
            } else {
                assert_eq!(server.loader.pending_repository_mappings(), ["c+", "d+"]);
                assert!(debounced.is_empty());
            }
            server
                .handle_event(Event::Task(Task::ResolveRepoMappings))
                .unwrap();
            let second = server
                .task_pool_handle
                .receiver
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
            assert!(
                matches!(
                    second,
                    Task::RepoMappingsReady {
                        repositories: _,
                        revision: _,
                        result: _
                    }
                ),
                "{second:?}"
            );
            server.handle_event(Event::Task(second)).unwrap();
            assert!(!server.is_resolving_repo_mappings);
            assert!(server.loader.pending_repository_mappings().is_empty());
            assert_eq!(
                bazel.mapping_requests.lock().unwrap().as_slice(),
                &[
                    vec!["a+".to_owned(), "b+".to_owned()],
                    vec!["c+".to_owned(), "d+".to_owned()],
                ]
            );
            assert!(request(&server.loader, "c+").is_ok());
            assert!(request(&server.loader, "d+").is_ok());
        }
    }

    fn notification<N: Notification>(params: N::Params) -> Event {
        Event::Message(lsp_server::Notification::new(N::METHOD.into(), params).into())
    }

    fn captured_diagnostics(server: &mut Server, file: File) -> Task {
        let snapshot = server.snapshot();
        let (file, ticket) = server
            .diagnostics_manager
            .request(&snapshot.analysis_snapshot, file)
            .unwrap();
        let diagnostics = collect_diagnostics(&snapshot, file).unwrap();
        Task::DiagnosticsReady(vec![(file, ticket, diagnostics)])
    }

    fn published(client: &Connection) -> Vec<lsp_types::PublishDiagnosticsParams> {
        client
            .receiver
            .try_iter()
            .filter_map(|message| {
                let lsp_server::Message::Notification(notification) = message else {
                    return None;
                };
                if notification.method != lsp_types::notification::PublishDiagnostics::METHOD {
                    return None;
                }
                Some(serde_json::from_value(notification.params).unwrap())
            })
            .collect()
    }

    fn analyze_requested_files(server: &mut Server, debounced: &Receiver<Vec<File>>) {
        let roots = debounced.try_recv().expect("event loop requested analysis");
        assert!(debounced.is_empty());
        server
            .handle_event(Event::Task(Task::AnalysisRequested(roots)))
            .unwrap();
        let task = server
            .task_pool_handle
            .receiver
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        assert!(matches!(task, Task::DiagnosticsReady(_)), "{task:?}");
        server.handle_event(Event::Task(task)).unwrap();
    }

    fn next_configuration(server: &mut Server) -> Task {
        loop {
            let task = server
                .task_pool_handle
                .receiver
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
            if matches!(task, Task::ConfigurationReady(_)) {
                return task;
            }
            server.handle_task(task);
        }
    }

    fn finish_reload(server: &mut Server) {
        let task = next_configuration(server);
        server.handle_task(task);
    }

    fn response(server: &mut Server, client: &Connection, id: i32) -> lsp_server::Response {
        loop {
            for message in client.receiver.try_iter() {
                if let lsp_server::Message::Response(response) = message {
                    if response.id == id.into() {
                        return response;
                    }
                }
            }
            let task = server
                .task_pool_handle
                .receiver
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
            server.handle_task(task);
            server.resolve_repository_mappings();
        }
    }

    fn finish_fetch(server: &mut Server) {
        while let Ok(task) = server.task_pool_handle.receiver.try_recv() {
            server.handle_task(task);
        }
        assert!(
            server.pending_repos.contains("rules+"),
            "{}: {:?}",
            server.workspace.display(),
            server.pending_repos
        );
        server.fetch_bazel_external_repos();
        loop {
            let task = server
                .task_pool_handle
                .receiver
                .recv_timeout(Duration::from_secs(10))
                .unwrap();
            let done = matches!(
                task,
                Task::FetchExternalRepos(FetchExternalReposProgress::End { .. })
            );
            server.handle_task(task);
            if done {
                break;
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn transitive_watches_and_retargeted_open_repositories() {
        for (during_fetch, missing) in [(false, false), (true, false), (false, true)] {
            let TestServer {
                mut server,
                client,
                disk: _,
                tasks,
                debounced: _,
            } = server();
            let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
                .join(format!("reload-repositories-{during_fetch}-{missing}"));
            let workspace = root.join("workspace");
            let external = root.join("external");
            let old = root.join("old");
            let new = root.join("new");
            let helper = root.join("helper");
            for directory in [&workspace, &external, &old, &new, &helper] {
                std::fs::create_dir_all(directory).unwrap();
                std::fs::write(directory.join("BUILD"), "").unwrap();
            }
            let text = "load(\"@@helper+//:defs.bzl\", \"helper_value\")\nvalue = helper_value\n";
            std::fs::write(old.join("defs.bzl"), text).unwrap();
            if !missing {
                std::fs::write(new.join("defs.bzl"), "value = 2\n").unwrap();
            }
            std::fs::write(helper.join("defs.bzl"), "helper_value = 1\n").unwrap();
            std::os::unix::fs::symlink(&old, external.join("rules+")).unwrap();
            std::os::unix::fs::symlink(&helper, external.join("helper+")).unwrap();
            server.bazel_client = Arc::new(crate::document::source_tests::TestBazelClient {
                fetch_requests: Default::default(),
                fetch_batches: Default::default(),
                fetch_files: Default::default(),
                fetch_failures: Default::default(),
                retarget: std::sync::Mutex::new(
                    during_fetch.then(|| (external.join("rules+"), new.clone())),
                ),
                mapping_requests: Default::default(),
            });
            server.loader = Arc::new(DefaultFileLoader::new(
                server.bazel_client.clone(),
                workspace.clone(),
                None,
                external.clone(),
                tasks,
                false,
            ));
            server.analysis = Analysis::new(server.loader.clone(), Default::default()).unwrap();
            server.workspace = workspace.clone();
            let caller = workspace.join("caller.bzl");
            server
                .open_document(
                    &caller,
                    "load(\"@@rules+//:defs.bzl\", \"value\")\nresult = value\n".into(),
                    1,
                )
                .unwrap();
            let file = server
                .analysis
                .snapshot()
                .open_file(&caller)
                .unwrap()
                .unwrap();
            let diagnostics = collect_diagnostics(&server.snapshot(), file).unwrap();
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
            assert!(server.loader.repository_roots().unwrap().contains(&helper));
            Arc::get_mut(&mut server.config).unwrap().caps.workspace =
                Some(lsp_types::WorkspaceClientCapabilities {
                    did_change_watched_files: Some(
                        lsp_types::DidChangeWatchedFilesClientCapabilities {
                            dynamic_registration: Some(true),
                            relative_pattern_support: Some(true),
                        },
                    ),
                    ..Default::default()
                });
            server.watch_type_interfaces().unwrap();
            let lsp_server::Message::Request(request) = client.receiver.try_recv().unwrap() else {
                panic!("watch request")
            };
            assert!(
                request.params.to_string().contains(
                    lsp_types::Url::from_directory_path(&helper)
                        .unwrap()
                        .as_str()
                ),
                "{request:?}"
            );
            let alias = external.join("rules+/defs.bzl");
            server
                .open_document(&alias, format!("{text}unsaved = 42\n"), 7)
                .unwrap();
            if !during_fetch {
                std::fs::remove_file(external.join("rules+")).unwrap();
                std::os::unix::fs::symlink(&new, external.join("rules+")).unwrap();
            }
            if during_fetch {
                server.reload_configuration().unwrap();
            } else {
                crate::handlers::notifications::did_change_watched_files(
                    &mut server,
                    lsp_types::DidChangeWatchedFilesParams {
                        changes: vec![lsp_types::FileEvent {
                            uri: lsp_types::Url::from_file_path(&alias).unwrap(),
                            typ: lsp_types::FileChangeType::CHANGED,
                        }],
                    },
                )
                .unwrap();
            }
            finish_reload(&mut server);
            if during_fetch {
                assert!(!server.configuration.needs_reopen);
                server.analysis.close_document(&caller).unwrap();
                let external_file = server
                    .analysis
                    .snapshot()
                    .open_file(&alias)
                    .unwrap()
                    .unwrap();
                assert!(!collect_diagnostics(&server.snapshot(), external_file)
                    .unwrap()
                    .is_empty());
                finish_fetch(&mut server);
                assert!(server.configuration.refreshing);
                finish_reload(&mut server);
            }
            assert!(server.configuration.needs_reopen);
            assert!(server
                .analysis
                .document(&alias)
                .unwrap()
                .contents
                .contains("unsaved = 42"));
            assert!(!collect_diagnostics(&server.snapshot(), file)
                .unwrap()
                .is_empty());
            assert!(client.receiver.try_iter().any(|message| match message {
                lsp_server::Message::Notification(message) =>
                    message.params.to_string().contains("close and reopen"),
                _ => false,
            }));
            server
                .open_document(&alias, format!("{text}unsaved = 43\n"), 8)
                .unwrap();
            assert_eq!(server.analysis.document(&alias).unwrap().version, 8);
            server.reload_configuration().unwrap();
            finish_reload(&mut server);
            assert!(server.configuration.needs_reopen);
            assert_eq!(server.analysis.document(&alias).unwrap().version, 8);
            assert_eq!(server.loader.repository_for_path(&alias).unwrap(), None);

            crate::handlers::notifications::did_close_text_document(
                &mut server,
                lsp_types::DidCloseTextDocumentParams {
                    text_document: lsp_types::TextDocumentIdentifier {
                        uri: lsp_types::Url::from_file_path(&alias).unwrap(),
                    },
                },
            )
            .unwrap();
            finish_reload(&mut server);
            assert!(!server.configuration.needs_reopen);
            std::fs::write(new.join("defs.bzl"), "value = 2\n").unwrap();
            if during_fetch {
                server
                    .open_document(
                        &caller,
                        "load(\"@@rules+//:defs.bzl\", \"value\")\nresult = value\n".into(),
                        2,
                    )
                    .unwrap();
            }
            let _ = collect_diagnostics(&server.snapshot(), file);
            // Existing directories are fetched for the new dependency snapshot too.
            finish_fetch(&mut server);
            let diagnostics = collect_diagnostics(&server.snapshot(), file).unwrap();
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn saved_configuration_replaces_stubs_and_recovers() {
        let TestServer {
            mut server,
            client,
            disk: _,
            tasks,
            debounced: _,
        } = server();
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("configuration-reload");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("BUILD"), "").unwrap();
        std::fs::write(root.join("source.bzl"), "def f(value): pass\n").unwrap();
        std::fs::write(root.join("source.bzli"), "def f(value: int): ...\n").unwrap();
        let manifest = "format-version = 1\n[source]\nrepository = '@'\nmodule = 'local'\nversions = ['1']\n[files]\n'source.bzl' = 'source.bzli'\n";
        std::fs::write(root.join("stubs.toml"), manifest).unwrap();
        server.loader = Arc::new(DefaultFileLoader::new(
            server.bazel_client.clone(),
            root.clone(),
            None,
            root.join("external"),
            tasks,
            false,
        ));
        server.analysis = Analysis::new(server.loader.clone(), Default::default()).unwrap();
        server.workspace = root.clone();
        let caller = root.join("caller.bzl");
        server
            .open_document(
                &caller,
                "load(\"//:source.bzl\", \"f\")\nf(\"wrong\")\n".into(),
                7,
            )
            .unwrap();
        let file = server
            .analysis
            .snapshot()
            .open_file(&caller)
            .unwrap()
            .unwrap();
        assert!(collect_diagnostics(&server.snapshot(), file)
            .unwrap()
            .is_empty());
        let old = captured_diagnostics(&mut server, file);
        let request = lsp_server::Request::new(
            51.into(),
            "textDocument/hover".into(),
            serde_json::json!({
                "textDocument": { "uri": lsp_types::Url::from_file_path(&caller).unwrap() },
                "position": { "line": 1, "character": 0 },
            }),
        );
        server.req_queue.incoming.register(request.id.clone(), ());
        let old_response = Task::ResponseReady {
            revision: server.configuration.revision,
            response: lsp_server::Response::new_ok(request.id.clone(), "stale"),
            request,
        };
        std::fs::write(
            root.join("starpls.toml"),
            "[[stub-packages]]\nmanifest = 'stubs.toml'\nallow-unversioned = true\n",
        )
        .unwrap();
        let changed = |path: &std::path::Path| lsp_types::DidChangeWatchedFilesParams {
            changes: vec![lsp_types::FileEvent {
                uri: lsp_types::Url::from_file_path(path).unwrap(),
                typ: lsp_types::FileChangeType::CHANGED,
            }],
        };
        crate::handlers::notifications::did_change_watched_files(
            &mut server,
            changed(&root.join("starpls.toml")),
        )
        .unwrap();
        server.handle_task(old);
        assert!(published(&client).is_empty());
        server.handle_task(old_response);
        let ready = next_configuration(&mut server);
        // A second save arrives while a prepared result is waiting in the queue.
        std::fs::write(
            root.join("starpls.toml"),
            "[[stub-packages]]\nmanifest = 'stubs.toml'\nallow-unversioned = true\n# second save\n",
        )
        .unwrap();
        crate::handlers::notifications::did_change_watched_files(
            &mut server,
            changed(&root.join("starpls.toml")),
        )
        .unwrap();
        server.handle_task(ready);
        assert!(server.analysis.type_interface_files().is_empty());
        finish_reload(&mut server);
        let response = response(&mut server, &client, 51);
        assert!(!response.result.unwrap().to_string().contains("stale"));
        assert_eq!(server.analysis.type_interface_files().len(), 1);
        assert!(!collect_diagnostics(&server.snapshot(), file)
            .unwrap()
            .is_empty());

        // Unsaved interfaces remain authoritative across a valid dependency reload.
        server
            .open_document(
                &root.join("source.bzli"),
                "def f(value: str): ...\n".into(),
                3,
            )
            .unwrap();
        std::fs::write(root.join("MODULE.bazel.lock"), "{}").unwrap();
        crate::handlers::notifications::did_change_watched_files(
            &mut server,
            changed(&root.join("MODULE.bazel.lock")),
        )
        .unwrap();
        assert!(server.analysis.type_interface_files().is_empty());
        let ready = server
            .task_pool_handle
            .receiver
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        server.handle_task(ready);
        assert!(collect_diagnostics(&server.snapshot(), file)
            .unwrap()
            .is_empty());
        assert_eq!(
            server
                .analysis
                .document(&root.join("source.bzli"))
                .unwrap()
                .version,
            3
        );
        let revision = server.configuration.revision;
        crate::handlers::notifications::did_change_watched_files(
            &mut server,
            changed(&root.join("MODULE.bazel.lock")),
        )
        .unwrap();
        assert_eq!(
            server.configuration.revision, revision,
            "unchanged lock notification must settle"
        );

        std::fs::write(root.join("stubs.toml"), "invalid = true\n").unwrap();
        crate::handlers::notifications::did_change_watched_files(
            &mut server,
            changed(&root.join("stubs.toml")),
        )
        .unwrap();
        assert!(server.analysis.type_interface_files().is_empty());
        let ready = server
            .task_pool_handle
            .receiver
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        server.handle_task(ready);
        assert!(server.analysis.type_interface_files().is_empty());
        assert_eq!(server.analysis.document(&caller).unwrap().version, 7);
        assert!(client.receiver.try_iter().any(|message| match message {
            lsp_server::Message::Notification(message) =>
                message.method == "window/showMessage"
                    && message.params.to_string().contains("invalid manifest"),
            _ => false,
        }));
        // Repair the selected manifest alone; the failed attempt retained its watch input.
        std::fs::write(root.join("stubs.toml"), manifest).unwrap();
        crate::handlers::notifications::did_change_watched_files(
            &mut server,
            changed(&root.join("stubs.toml")),
        )
        .unwrap();
        let ready = server
            .task_pool_handle
            .receiver
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        server.handle_task(ready);
        assert_eq!(server.analysis.type_interface_files().len(), 1);
        std::fs::remove_file(root.join("starpls.toml")).unwrap();
        crate::handlers::notifications::did_save_text_document(
            &mut server,
            lsp_types::DidSaveTextDocumentParams {
                text_document: lsp_types::TextDocumentIdentifier {
                    uri: lsp_types::Url::from_file_path(root.join("starpls.toml")).unwrap(),
                },
                text: None,
            },
        )
        .unwrap();
        let ready = server
            .task_pool_handle
            .receiver
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        server.handle_task(ready);
        assert!(server.analysis.type_interface_files().is_empty());
        std::fs::write(root.join(".bazelrc"), "common --enable_bzlmod\n").unwrap();
        crate::handlers::notifications::did_change_watched_files(
            &mut server,
            changed(&root.join(".bazelrc")),
        )
        .unwrap();
        assert!(!server.configuration.refreshing);
        assert!(client.receiver.try_iter().any(|message| match message {
            lsp_server::Message::Notification(message) =>
                message.params.to_string().contains("Restart Starpls"),
            _ => false,
        }));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn shared_buffers_preserve_diagnostic_uris_and_unique_rename_edits() {
        let TestServer {
            mut server,
            client,
            disk: _,
            tasks,
            debounced: _,
        } = server();
        let root = std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap())
            .join("editor-contexts");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("BUILD"), "").unwrap();
        let defs = workspace.join("defs.bzl");
        std::fs::write(&defs, "def f(): pass\n").unwrap();
        let stub = root.join("defs.bzli");
        let installed_stub = workspace.join("defs.bzli");
        std::fs::write(&stub, "def f(): ...\n").unwrap();
        std::os::unix::fs::symlink(&stub, &installed_stub).unwrap();
        server.loader = Arc::new(DefaultFileLoader::new(
            Arc::new(crate::document::source_tests::TestBazelClient::default()),
            workspace.clone(),
            None,
            root.join("external"),
            tasks,
            false,
        ));
        server.analysis = Analysis::new(server.loader.clone(), Default::default()).unwrap();
        server.workspace = workspace.clone();
        server.configuration.complete = true;
        Arc::get_mut(&mut server.config).unwrap().caps =
            serde_json::from_value(serde_json::json!({
                "workspace": {"workspaceEdit": {"documentChanges": true}}
            }))
            .unwrap();
        let source = server
            .analysis
            .file(&defs, starpls_common::Dialect::Bazel, None)
            .unwrap();
        let interface = server
            .analysis
            .file(&installed_stub, starpls_common::Dialect::Bazel, None)
            .unwrap();
        server
            .analysis
            .set_type_interfaces([(source, interface)])
            .unwrap();
        server
            .open_document(&stub, "bad: Missing\n".into(), 9)
            .unwrap();
        let task = captured_diagnostics(&mut server, interface);
        server.handle_task(task);
        let updates = published(&client);
        let [update] = updates.as_slice() else {
            panic!("{updates:?}")
        };
        assert_eq!(
            update.uri,
            lsp_types::Url::from_file_path(&installed_stub).unwrap()
        );
        assert_eq!(update.version, None);
        assert!(!update.diagnostics.is_empty(), "{update:?}");
        server.analysis.set_type_interfaces([]).unwrap();
        server.analysis.close_document(&stub).unwrap();

        let backing = workspace.join("backing.bzl");
        std::fs::write(&backing, "value = 0\n").unwrap();
        for name in ["one.bzl", "two.bzl"] {
            let alias = workspace.join(name);
            std::os::unix::fs::symlink(&backing, &alias).unwrap();
            server
                .analysis
                .file(&alias, starpls_common::Dialect::Bazel, None)
                .unwrap();
        }
        server
            .open_document(&backing, "load('//:defs.bzl', 'f')\nf()\n".into(), 7)
            .unwrap();
        server
            .open_document(&defs, "def f(): pass\n".into(), 1)
            .unwrap();
        let snapshot = server.snapshot();
        let candidates = snapshot.reference_files("f").unwrap();
        let paths: Vec<_> = candidates
            .iter()
            .map(|file| snapshot.analysis_snapshot.path(*file))
            .collect();
        assert!(
            paths.contains(&workspace.join("one.bzl").as_path()),
            "{paths:?}"
        );
        assert!(
            paths.contains(&workspace.join("two.bzl").as_path()),
            "{paths:?}"
        );
        let parameters = lsp_types::RenameParams {
            text_document_position: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier {
                    uri: lsp_types::Url::from_file_path(&defs).unwrap(),
                },
                position: lsp_types::Position::new(0, 4),
            },
            new_name: "renamed".into(),
            work_done_progress_params: Default::default(),
        };
        let edit = crate::handlers::requests::rename(&snapshot, parameters.clone())
            .unwrap()
            .unwrap();
        let Some(lsp_types::DocumentChanges::Edits(edits)) = edit.document_changes else {
            panic!("{edit:?}")
        };
        assert_eq!(edits.len(), 2, "{edits:?}");
        let references = edits
            .iter()
            .find(|edit| {
                edit.text_document.uri == lsp_types::Url::from_file_path(&backing).unwrap()
            })
            .unwrap();
        assert_eq!(references.text_document.version, Some(7));
        assert_eq!(references.edits.len(), 2, "{references:?}");
        drop(snapshot);
        // A workspace symlink must not make an outside file eligible for edits.
        let outside = root.join("outside.bzl");
        std::fs::write(&outside, "load('//:defs.bzl', 'f')\nf()\n").unwrap();
        let alias = workspace.join("outside.bzl");
        std::os::unix::fs::symlink(&outside, &alias).unwrap();
        server
            .analysis
            .file(&alias, starpls_common::Dialect::Bazel, None)
            .unwrap();
        let error =
            crate::handlers::requests::rename(&server.snapshot(), parameters.clone()).unwrap_err();
        assert!(error.to_string().contains("Cannot rename"), "{error}");
        std::fs::remove_file(&alias).unwrap();
        server
            .analysis
            .sync_files(std::slice::from_ref(&alias))
            .unwrap();
        // Distinct module identities can share a declaration's physical bytes.
        let second_defs = workspace.join("other-defs.bzl");
        std::os::unix::fs::symlink(&defs, &second_defs).unwrap();
        server
            .analysis
            .file(&second_defs, starpls_common::Dialect::Bazel, None)
            .unwrap();
        server
            .open_document(
                &workspace.join("other-caller.bzl"),
                "load('//:other-defs.bzl', 'f')\nf()\n".into(),
                1,
            )
            .unwrap();
        let error = crate::handlers::requests::rename(&server.snapshot(), parameters).unwrap_err();
        assert!(
            error.to_string().contains("Cannot rename shared source"),
            "{error}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn a_rejected_alias_open_preserves_the_server_and_buffer() {
        let TestServer {
            mut server,
            client,
            disk: _,
            tasks,
            debounced,
        } = server();
        let root =
            std::path::PathBuf::from(std::env::var_os("TEST_TMPDIR").unwrap()).join("editor-alias");
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("target.star");
        let alias = root.join("alias.star");
        std::fs::write(&target, "value = 0\n").unwrap();
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        let loader = DefaultFileLoader::new(
            server.bazel_client.clone(),
            root.clone(),
            None,
            root.join("external"),
            tasks,
            false,
        );
        server.loader = Arc::new(loader);
        server.analysis = Analysis::new(server.loader.clone(), Default::default()).unwrap();
        server.workspace = root.clone();
        let alias_uri = lsp_types::Url::from_file_path(&alias).unwrap();
        for (path, value) in [(&alias, 1), (&target, 2)] {
            server
                .handle_event(
                    notification::<lsp_types::notification::DidOpenTextDocument>(
                        lsp_types::DidOpenTextDocumentParams {
                            text_document: lsp_types::TextDocumentItem {
                                uri: lsp_types::Url::from_file_path(path).unwrap(),
                                language_id: "starlark".into(),
                                version: value,
                                text: format!("value = {value}\n"),
                            },
                        },
                    ),
                )
                .unwrap();
            if value == 1 {
                analyze_requested_files(&mut server, &debounced);
                let diagnostics = published(&client);
                let [diagnostics] = diagnostics.as_slice() else {
                    panic!("{diagnostics:?}")
                };
                assert_eq!(diagnostics.uri, alias_uri);
            }
        }
        let message = client.receiver.try_recv().unwrap();
        let lsp_server::Message::Notification(message) = message else {
            panic!("{message:?}")
        };
        assert_eq!(message.method, "window/showMessage");
        assert!(message.params["message"]
            .as_str()
            .unwrap()
            .contains("already open"));
        assert_eq!(
            server.analysis.document(&alias).unwrap().contents,
            "value = 1\n"
        );
        let rejected_uri = lsp_types::Url::from_file_path(&target).unwrap();
        server
            .handle_event(
                notification::<lsp_types::notification::DidChangeTextDocument>(
                    lsp_types::DidChangeTextDocumentParams {
                        text_document: lsp_types::VersionedTextDocumentIdentifier {
                            uri: rejected_uri.clone(),
                            version: 3,
                        },
                        content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                            range: None,
                            range_length: None,
                            text: "value = 3\n".into(),
                        }],
                    },
                ),
            )
            .unwrap();
        server
            .handle_event(
                notification::<lsp_types::notification::DidCloseTextDocument>(
                    lsp_types::DidCloseTextDocumentParams {
                        text_document: lsp_types::TextDocumentIdentifier { uri: rejected_uri },
                    },
                ),
            )
            .unwrap();
        assert_eq!(
            server.analysis.document(&alias).unwrap().contents,
            "value = 1\n"
        );
        server
            .handle_event(Event::Message(
                lsp_server::Request::new(
                    42.into(),
                    "textDocument/hover".into(),
                    serde_json::json!({
                        "textDocument": {"uri": alias_uri},
                        "position": {"line": 0, "character": 2}
                    }),
                )
                .into(),
            ))
            .unwrap();
        let response = server
            .task_pool_handle
            .receiver
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        server.handle_event(Event::Task(response)).unwrap();
        let message = client
            .receiver
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        let lsp_server::Message::Response(response) = message else {
            panic!("{message:?}")
        };
        assert_eq!(response.id, 42.into());
        assert!(response.result.unwrap().to_string().contains("Literal[1]"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dependency_changes_refresh_unchanged_callers() {
        let TestServer {
            mut server,
            client,
            disk,
            tasks,
            debounced,
        } = server();
        let dependency = Path::new("/workspace/dep.bzl");
        let dependency_uri = lsp_types::Url::from_file_path(dependency).unwrap();
        let caller = Path::new("/workspace/BUILD");
        let caller_uri = lsp_types::Url::from_file_path(caller).unwrap();
        let requires_int = "def f(value):\n    # type: (int) -> None\n    pass\n";
        let requires_string = "def f(value):\n    # type: (string) -> None\n    pass\n";
        disk.write_file(SystemPath::new("/workspace/dep.bzl"), requires_int)
            .unwrap();
        server
            .open_document(dependency, requires_int.into(), 1)
            .unwrap();
        server
            .open_document(
                caller,
                "load(\"@//:dep.bzl\", \"f\")\nf(\"value\")\n".into(),
                7,
            )
            .unwrap();
        let caller_file = server
            .analysis
            .snapshot()
            .open_file(caller)
            .unwrap()
            .unwrap();
        let old = captured_diagnostics(&mut server, caller_file);
        assert!(!collect_diagnostics(&server.snapshot(), caller_file)
            .unwrap()
            .is_empty());

        server
            .handle_event(
                notification::<lsp_types::notification::DidChangeTextDocument>(
                    lsp_types::DidChangeTextDocumentParams {
                        text_document: lsp_types::VersionedTextDocumentIdentifier {
                            uri: dependency_uri.clone(),
                            version: 2,
                        },
                        content_changes: vec![lsp_types::TextDocumentContentChangeEvent {
                            range: None,
                            range_length: None,
                            text: requires_string.into(),
                        }],
                    },
                ),
            )
            .unwrap();
        server.handle_event(Event::Task(old)).unwrap();
        assert!(published(&client).is_empty());
        analyze_requested_files(&mut server, &debounced);
        let updates = published(&client);
        let caller_update = updates
            .iter()
            .find(|update| update.uri == caller_uri)
            .unwrap();
        assert!(caller_update.diagnostics.is_empty(), "{caller_update:?}");
        assert_eq!(caller_update.version, Some(7));

        // Closing the dependency restores its disk contents and checks the
        // caller again, while clearing diagnostics for the closed buffer.
        server
            .handle_event(
                notification::<lsp_types::notification::DidCloseTextDocument>(
                    lsp_types::DidCloseTextDocumentParams {
                        text_document: lsp_types::TextDocumentIdentifier {
                            uri: dependency_uri.clone(),
                        },
                    },
                ),
            )
            .unwrap();
        let updates = published(&client);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].uri, dependency_uri);
        assert!(updates[0].diagnostics.is_empty());
        analyze_requested_files(&mut server, &debounced);
        let updates = published(&client);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].uri, caller_uri);
        assert!(!updates[0].diagnostics.is_empty());
        assert_eq!(updates[0].version, Some(7));

        // A completed repository fetch may be followed by an old result in
        // the same event-loop batch. Cancel its ticket before draining it.
        let old = captured_diagnostics(&mut server, caller_file);
        disk.write_file(SystemPath::new("/workspace/dep.bzl"), requires_string)
            .unwrap();
        tasks.send(old).unwrap();
        server
            .handle_event(Event::Task(Task::FetchExternalRepos(
                FetchExternalReposProgress::End {
                    revision: server.configuration.revision,
                    results: Vec::new(),
                },
            )))
            .unwrap();
        assert!(published(&client).is_empty());
        analyze_requested_files(&mut server, &debounced);
        let updates = published(&client);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].uri, caller_uri);
        assert!(updates[0].diagnostics.is_empty(), "{updates:?}");
        assert_eq!(updates[0].version, Some(7));

        // Completion metadata also changes a Salsa input, which can cancel
        // diagnostic snapshots even though the source files did not change.
        let old = captured_diagnostics(&mut server, caller_file);
        tasks.send(old).unwrap();
        server
            .handle_event(Event::Task(Task::RefreshAllWorkspaceTargets(
                RefreshAllWorkspaceTargetsProgress::End {
                    revision: server.configuration.revision,
                    targets: Some(vec!["//:target".into()]),
                },
            )))
            .unwrap();
        assert!(published(&client).is_empty());
        analyze_requested_files(&mut server, &debounced);
        let updates = published(&client);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].uri, caller_uri);
        assert!(updates[0].diagnostics.is_empty());
        assert_eq!(updates[0].version, Some(7));
    }
    #[test]
    fn closed_interfaces_remain_live_diagnostic_roots() {
        let TestServer {
            mut server,
            client,
            disk,
            tasks: _,
            debounced,
        } = server();
        let source_path = Path::new("/workspace/source.bzl");
        let interface_path = Path::new("/contracts/source.bzli");
        let interface_uri = lsp_types::Url::from_file_path(interface_path).unwrap();
        disk.create_directory_all(SystemPath::new("/contracts"))
            .unwrap();
        disk.write_file(SystemPath::new("/workspace/source.bzl"), "def f(): pass\n")
            .unwrap();
        let int_contract = "def f(value: int) -> None: ...\n";
        let str_contract = "def f(value: string) -> None: ...\n";
        disk.write_file(SystemPath::new("/contracts/source.bzli"), int_contract)
            .unwrap();
        let source = server
            .analysis
            .file(source_path, starpls_common::Dialect::Bazel, None)
            .unwrap();
        let interface = server
            .analysis
            .file(interface_path, starpls_common::Dialect::Bazel, None)
            .unwrap();
        server
            .analysis
            .set_type_interfaces([(source, interface)])
            .unwrap();
        server
            .open_document(
                Path::new("/workspace/BUILD"),
                "load(\"@//:source.bzl\", \"f\")\nf('text')\n".to_owned(),
                4,
            )
            .unwrap();
        let caller = server
            .analysis
            .snapshot()
            .open_file(Path::new("/workspace/BUILD"))
            .unwrap()
            .unwrap();
        let old = captured_diagnostics(&mut server, caller);
        assert!(!collect_diagnostics(&server.snapshot(), caller)
            .unwrap()
            .is_empty());
        disk.write_file(SystemPath::new("/contracts/source.bzli"), str_contract)
            .unwrap();
        let changed = || {
            notification::<lsp_types::notification::DidChangeWatchedFiles>(
                lsp_types::DidChangeWatchedFilesParams {
                    changes: vec![lsp_types::FileEvent {
                        uri: interface_uri.clone(),
                        typ: lsp_types::FileChangeType::CHANGED,
                    }],
                },
            )
        };
        server.handle_event(changed()).unwrap();
        server.handle_event(Event::Task(old)).unwrap();
        assert!(published(&client).is_empty());
        analyze_requested_files(&mut server, &debounced);
        let updates = published(&client);
        assert_eq!(updates.len(), 2, "{updates:?}");
        assert!(
            updates.iter().all(|update| update.diagnostics.is_empty()),
            "{updates:?}"
        );
        let closed = updates
            .iter()
            .find(|update| update.uri == interface_uri)
            .unwrap();
        assert_eq!(closed.version, None);

        server
            .open_document(interface_path, int_contract.to_owned(), 9)
            .unwrap();
        server.handle_event(changed()).unwrap();
        assert_eq!(server.analysis.diagnostic_files().len(), 2);
        analyze_requested_files(&mut server, &debounced);
        let updates = published(&client);
        let open = updates
            .iter()
            .find(|update| update.uri == interface_uri)
            .unwrap();
        assert_eq!(open.version, Some(9));
        assert!(!collect_diagnostics(&server.snapshot(), caller)
            .unwrap()
            .is_empty());

        server
            .handle_event(
                notification::<lsp_types::notification::DidCloseTextDocument>(
                    lsp_types::DidCloseTextDocumentParams {
                        text_document: lsp_types::TextDocumentIdentifier {
                            uri: interface_uri.clone(),
                        },
                    },
                ),
            )
            .unwrap();
        published(&client);
        analyze_requested_files(&mut server, &debounced);
        let updates = published(&client);
        assert_eq!(updates.len(), 2, "{updates:?}");
        assert!(
            updates.iter().all(|update| update.diagnostics.is_empty()),
            "{updates:?}"
        );
        assert!(updates
            .iter()
            .any(|update| update.uri == interface_uri && update.version.is_none()));

        disk.fs()
            .remove_file(SystemPath::new("/contracts/source.bzli"))
            .unwrap();
        server.handle_event(changed()).unwrap();
        analyze_requested_files(&mut server, &debounced);
        let updates = published(&client);
        let closed = updates
            .iter()
            .find(|update| update.uri == interface_uri)
            .unwrap();
        assert!(
            closed
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("cannot read")),
            "{closed:?}"
        );
        disk.write_file(SystemPath::new("/contracts/source.bzli"), str_contract)
            .unwrap();
        server.handle_event(changed()).unwrap();
        analyze_requested_files(&mut server, &debounced);
        let updates = published(&client);
        assert!(
            updates.iter().all(|update| update.diagnostics.is_empty()),
            "{updates:?}"
        );

        let config = std::sync::Arc::get_mut(&mut server.config).unwrap();
        config.caps.workspace = Some(lsp_types::WorkspaceClientCapabilities {
            did_change_watched_files: Some(lsp_types::DidChangeWatchedFilesClientCapabilities {
                dynamic_registration: Some(true),
                relative_pattern_support: Some(true),
            }),
            ..Default::default()
        });
        server.configuration = Default::default();
        server.watch_type_interfaces().unwrap();
        let lsp_server::Message::Request(request) = client.receiver.try_recv().unwrap() else {
            panic!("expected watch registration")
        };
        let registration: lsp_types::RegistrationParams =
            serde_json::from_value(request.params).unwrap();
        let [registration] = registration.registrations.as_slice() else {
            panic!("expected one watch registration: {registration:?}")
        };
        let options: lsp_types::DidChangeWatchedFilesRegistrationOptions =
            serde_json::from_value(registration.register_options.clone().unwrap()).unwrap();
        assert!(
            options
                .watchers
                .iter()
                .any(|watcher| match &watcher.glob_pattern {
                    lsp_types::GlobPattern::String(pattern) =>
                        pattern.contains("starpls.toml") && pattern.contains("*.bzli"),
                    lsp_types::GlobPattern::Relative(_) => false,
                }),
            "{options:?}"
        );
        assert!(
            options
                .watchers
                .iter()
                .any(|watcher| match &watcher.glob_pattern {
                    lsp_types::GlobPattern::Relative(pattern) =>
                        pattern.base_uri
                            == lsp_types::OneOf::Right(
                                lsp_types::Url::from_directory_path("/contracts").unwrap()
                            )
                            && pattern.pattern.contains("*.bzli"),
                    lsp_types::GlobPattern::String(_) => false,
                }),
            "{options:?}"
        );
        disk.write_file(SystemPath::new("/contracts/source.bzli"), int_contract)
            .unwrap();
        server
            .handle_event(Event::Message(
                lsp_server::Response::new_ok(request.id, ()).into(),
            ))
            .unwrap();
        analyze_requested_files(&mut server, &debounced);
        published(&client);
        assert!(!collect_diagnostics(&server.snapshot(), caller)
            .unwrap()
            .is_empty());

        server.configuration = Default::default();
        server.watch_type_interfaces().unwrap();
        let lsp_server::Message::Request(request) = client.receiver.try_recv().unwrap() else {
            panic!("expected watch registration")
        };
        server.complete_request(lsp_server::Response::new_err(
            request.id,
            -32601,
            "watching rejected".to_owned(),
        ));
        let lsp_server::Message::Notification(message) = client.receiver.try_recv().unwrap() else {
            panic!("expected watch error")
        };
        assert_eq!(message.method, "window/showMessage");
        assert!(message.params.to_string().contains("watching rejected"));

        // The handshake consumes initialized before run starts. Registration must
        // happen even when the client's first event is a shutdown request.
        server
            .analysis
            .close_document(Path::new("/workspace/BUILD"))
            .unwrap();
        server.analysis_changed = true;
        client
            .sender
            .send(lsp_server::Request::new(99.into(), "shutdown".to_owned(), ()).into())
            .unwrap();
        client
            .sender
            .send(lsp_server::Notification::new("exit".to_owned(), ()).into())
            .unwrap();
        server.configuration = Default::default();
        server.run().unwrap();
        assert_eq!(debounced.try_recv().unwrap(), vec![interface]);
        let lsp_server::Message::Request(request) = client.receiver.try_recv().unwrap() else {
            panic!("expected startup watch registration")
        };
        assert_eq!(request.method, "client/registerCapability");
    }
}
