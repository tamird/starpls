use crate::convert;
use crate::server::Server;
use crate::utils::apply_document_content_changes;

pub(crate) fn did_open_text_document(
    server: &mut Server,
    params: lsp_types::DidOpenTextDocumentParams,
) -> anyhow::Result<()> {
    let path = convert::path_buf_from_url(&params.text_document.uri)?;
    if let Err(error) = server.open_document(
        &path,
        params.text_document.text,
        params.text_document.version,
    ) {
        server.send_error_message(&format!("{error:#}"));
    }
    Ok(())
}

pub(crate) fn did_close_text_document(
    server: &mut Server,
    params: lsp_types::DidCloseTextDocumentParams,
) -> anyhow::Result<()> {
    let path = convert::path_buf_from_url(&params.text_document.uri)?;
    if !server
        .analysis
        .document(&path)
        .is_some_and(|document| document.path.as_std_path() == path)
    {
        return Ok(());
    }
    if server.analysis.close_document(&path)?.is_some() {
        if server.configuration.needs_reopen {
            server.reload_configuration()?;
        }
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
        if document.path.as_std_path() != path {
            return Ok(());
        }
        let contents =
            apply_document_content_changes(document.contents.clone(), params.content_changes);
        if let Err(error) = server.open_document(&path, contents, params.text_document.version) {
            server.send_error_message(&format!("{error:#}"));
        }
    }
    Ok(())
}

pub(crate) fn did_save_text_document(
    server: &mut Server,
    params: lsp_types::DidSaveTextDocumentParams,
) -> anyhow::Result<()> {
    let path = convert::path_buf_from_url(&params.text_document.uri)?;
    if matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("BUILD" | "BUILD.bazel")
    ) {
        server.refresh_all_workspace_targets();
    }
    server.configuration_changed(&[path])?;
    Ok(())
}

pub(crate) fn did_change_watched_files(
    server: &mut Server,
    params: lsp_types::DidChangeWatchedFilesParams,
) -> anyhow::Result<()> {
    let paths = params
        .changes
        .into_iter()
        .map(|event| convert::path_buf_from_url(&event.uri))
        .collect::<anyhow::Result<Vec<_>>>()?;
    server.configuration_changed(&paths)?;
    server.analysis.sync_files(&paths)?;
    server.invalidate_diagnostics();
    Ok(())
}
