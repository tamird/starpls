use crate::convert;
use crate::server::Server;
use crate::utils::apply_document_content_changes;

pub(crate) fn did_open_text_document(
    server: &mut Server,
    params: lsp_types::DidOpenTextDocumentParams,
) -> anyhow::Result<()> {
    let path = convert::path_buf_from_url(&params.text_document.uri)?;
    server.open_document(
        &path,
        params.text_document.text,
        params.text_document.version,
    )
}

pub(crate) fn did_close_text_document(
    server: &mut Server,
    params: lsp_types::DidCloseTextDocumentParams,
) -> anyhow::Result<()> {
    let path = convert::path_buf_from_url(&params.text_document.uri)?;
    if server.analysis.close_document(&path)?.is_some() {
        server.invalidate_diagnostics();
        server.send_notification::<lsp_types::notification::PublishDiagnostics>(
            lsp_types::PublishDiagnosticsParams {
                uri: params.text_document.uri,
                diagnostics: Vec::new(),
                version: None,
            },
        );
    }
    Ok(())
}

pub(crate) fn did_change_text_document(
    server: &mut Server,
    params: lsp_types::DidChangeTextDocumentParams,
) -> anyhow::Result<()> {
    let path = convert::path_buf_from_url(&params.text_document.uri)?;
    if let Some(document) = server.analysis.document(&path) {
        let contents =
            apply_document_content_changes(document.contents.clone(), params.content_changes);
        server.open_document(&path, contents, params.text_document.version)?;
    }
    Ok(())
}

pub(crate) fn did_save_text_document(
    server: &mut Server,
    params: lsp_types::DidSaveTextDocumentParams,
) -> anyhow::Result<()> {
    let path = convert::path_buf_from_url(&params.text_document.uri)?;
    if server.analysis.document(&path).is_some() {
        match path.file_name().and_then(|file_name| file_name.to_str()) {
            Some("MODULE.bazel" | "WORKSPACE" | "WORKSPACE.bazel" | "WORKSPACE.bzlmod") => {}
            Some(file_name) if file_name.ends_with(".MODULE.bazel") => {}
            Some("BUILD" | "BUILD.bazel") => {
                server.refresh_all_workspace_targets();
                return Ok(());
            }
            _ => return Ok(()),
        }
        server.bazel_client.clear_repo_mappings();
        server.fetched_repos.clear();
        server.analysis.invalidate_loads();
        server.invalidate_diagnostics();
    }
    Ok(())
}
