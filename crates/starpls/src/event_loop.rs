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
    End(Vec<String>),
}

#[derive(Debug)]
pub(crate) struct FetchExternalRepoRequest {
    pub(crate) repo: String,
}

#[derive(Debug)]
pub(crate) enum RefreshAllWorkspaceTargetsProgress {
    Begin,
    End(Option<Vec<String>>),
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
    ResponseReady(lsp_server::Response),
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
    let config = ServerConfig {
        args,
        caps: initialize_params.capabilities,
    };
    let server = Server::new(connection, config)?;
    server.run()
}

impl Server {
    fn run(mut self) -> anyhow::Result<()> {
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
                .send(self.analysis.open_files())
                .unwrap();
        } else if let Some(file_ids) = self.analysis_requested_for_files.take() {
            self.update_diagnostics(file_ids);
        }

        Ok(())
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
            .on::<lsp_types::request::References>(requests::find_references)
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
                    let stamp = snapshot.document(path).map(|document| document.stamp());
                    if self.diagnostics_manager.complete(file, ticket, stamp) {
                        let uri = lsp_types::Url::from_file_path(path).expect("absolute file path");
                        self.send_notification::<lsp_types::notification::PublishDiagnostics>(
                            lsp_types::PublishDiagnosticsParams {
                                uri,
                                diagnostics,
                                version: Some(ticket.document.version),
                            },
                        );
                    }
                }
            }
            Task::ResponseReady(resp) => {
                self.respond(resp);
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
                    FetchExternalReposProgress::End(failed_repos) => {
                        self.is_fetching_repos = false;
                        self.analysis.invalidate_loads();
                        self.invalidate_diagnostics();

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
            Task::FetchExternalRepoRequest(FetchExternalRepoRequest { repo }) => {
                if !self.fetched_repos.contains(&repo) {
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
                    RefreshAllWorkspaceTargetsProgress::End(targets) => {
                        self.is_refreshing_all_workspace_targets = false;
                        if let Some(targets) = targets {
                            self.analysis.set_all_workspace_targets(targets);
                            self.invalidate_diagnostics();
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
            "/external".into(),
            sender.clone(),
            false,
        );
        let disk = InMemorySystem::default();
        disk.create_directory_all(SystemPath::new("/workspace"))
            .unwrap();
        let analysis = Analysis::with_system(Arc::new(loader), Default::default(), disk.clone());
        let server = Server {
            config: Arc::new(ServerConfig {
                args: Default::default(),
                caps: Default::default(),
            }),
            connection,
            req_queue: Default::default(),
            task_pool_handle: TaskPoolHandle::new(receiver, pool),
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
            fetched_repos: Default::default(),
            is_fetching_repos: false,
            is_refreshing_all_workspace_targets: false,
            bzlmod_enabled: false,
        };
        TestServer {
            server,
            client,
            disk,
            tasks: sender,
            debounced,
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
                FetchExternalReposProgress::End(Vec::new()),
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
                RefreshAllWorkspaceTargetsProgress::End(Some(vec!["//:target".into()])),
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
}
