use std::collections::BTreeMap;
use std::collections::HashSet;

use starpls_ide::CompletionItemKind;
use starpls_ide::CompletionMode::InsertText;
use starpls_ide::CompletionMode::TextEdit;
use starpls_ide::Edit;
use starpls_ide::FilePosition;

use crate::convert::path_buf_from_url;
use crate::convert::{self};
use crate::extensions::ShowHirParams;
use crate::extensions::ShowSyntaxTreeParams;
use crate::server::ServerSnapshot;
use crate::utils::response_from_locations;

macro_rules! try_opt {
    ($expr:expr) => {
        match { $expr } {
            Some(res) => res,
            None => return Ok(None),
        }
    };
}

pub(crate) fn show_hir(snapshot: &ServerSnapshot, params: ShowHirParams) -> anyhow::Result<String> {
    let path = path_buf_from_url(&params.text_document.uri)?;
    let file_id = match snapshot.analysis_snapshot.open_file(&path)? {
        Some(file_id) => file_id,
        None => return Ok("".to_string()),
    };
    let rendered_hir = snapshot.analysis_snapshot.show_hir(file_id)?;
    Ok(rendered_hir.unwrap_or_else(|| "".to_string()))
}

pub(crate) fn show_syntax_tree(
    snapshot: &ServerSnapshot,
    params: ShowSyntaxTreeParams,
) -> anyhow::Result<String> {
    let path = path_buf_from_url(&params.text_document.uri)?;
    let file_id = match snapshot.analysis_snapshot.open_file(&path)? {
        Some(file_id) => file_id,
        None => return Ok("".to_string()),
    };
    let rendered_syntax_tree = snapshot.analysis_snapshot.show_syntax_tree(file_id)?;
    Ok(rendered_syntax_tree.unwrap_or_else(|| "".to_string()))
}

pub(crate) fn goto_definition(
    snapshot: &ServerSnapshot,
    params: lsp_types::GotoDefinitionParams,
) -> anyhow::Result<Option<lsp_types::GotoDefinitionResponse>> {
    goto_definition_impl(
        snapshot,
        params,
        snapshot.config.args.goto_definition_skip_re_exports,
    )
}

pub(crate) fn goto_declaration(
    snapshot: &ServerSnapshot,
    params: lsp_types::GotoDefinitionParams,
) -> anyhow::Result<Option<lsp_types::GotoDefinitionResponse>> {
    goto_definition_impl(snapshot, params, true)
}

fn goto_definition_impl(
    snapshot: &ServerSnapshot,
    params: lsp_types::GotoDefinitionParams,
    skip_re_exports: bool,
) -> anyhow::Result<Option<lsp_types::GotoDefinitionResponse>> {
    let path = path_buf_from_url(&params.text_document_position_params.text_document.uri)?;
    let file_id = try_opt!(snapshot.analysis_snapshot.open_file(&path)?);
    let pos = try_opt!(convert::text_size_from_lsp_position(
        snapshot,
        file_id,
        params.text_document_position_params.position,
    )?);
    let resp = response_from_locations(
        snapshot,
        file_id,
        snapshot
            .analysis_snapshot
            .goto_definition(FilePosition { file_id, pos }, skip_re_exports)?
            .unwrap_or_else(Vec::new)
            .into_iter(),
    );
    Ok(Some(resp))
}

pub(crate) fn find_references(
    snapshot: &ServerSnapshot,
    params: lsp_types::ReferenceParams,
) -> anyhow::Result<Option<Vec<lsp_types::Location>>> {
    let path = path_buf_from_url(&params.text_document_position.text_document.uri)?;
    let file_id = try_opt!(snapshot.analysis_snapshot.open_file(&path)?);
    let pos = try_opt!(convert::text_size_from_lsp_position(
        snapshot,
        file_id,
        params.text_document_position.position,
    )?);
    let position = FilePosition { file_id, pos };
    let name = try_opt!(snapshot
        .analysis_snapshot
        .reference_name(position.clone())?);
    let candidates = snapshot.reference_files(&name)?;
    let references = snapshot
        .analysis_snapshot
        .workspace_references(position, &candidates, params.context.include_declaration)?
        .unwrap_or_default();
    snapshot.ensure_workspace_ready()?;
    let mut locations = Vec::with_capacity(references.len());
    for location in references {
        let source = snapshot.analysis_snapshot.source(location.file_id)?;
        if let (Some(range), Ok(uri)) = (
            convert::lsp_range_from_text_range(location.range, &source),
            lsp_types::Url::from_file_path(snapshot.analysis_snapshot.path(location.file_id)),
        ) {
            locations.push(lsp_types::Location { range, uri });
        }
    }
    Ok(Some(locations))
}

fn rename_locations(
    snapshot: &ServerSnapshot,
    position: lsp_types::TextDocumentPositionParams,
    new_name: Option<&str>,
) -> anyhow::Result<Option<(starpls_common::File, starpls_ide::Rename)>> {
    let path = path_buf_from_url(&position.text_document.uri)?;
    let file_id = try_opt!(snapshot.analysis_snapshot.open_file(&path)?);
    let pos = try_opt!(convert::text_size_from_lsp_position(
        snapshot,
        file_id,
        position.position
    )?);
    let position = FilePosition { file_id, pos };
    let name = try_opt!(snapshot
        .analysis_snapshot
        .reference_name(position.clone())?);
    let candidates = snapshot.reference_files(&name)?;
    let rename = try_opt!(snapshot
        .analysis_snapshot
        .rename(position, &candidates, new_name)??);
    snapshot.ensure_workspace_ready()?;
    let matched: HashSet<_> = rename
        .locations
        .iter()
        .map(|location| {
            (
                snapshot.analysis_snapshot.path(location.file_id),
                location.range,
            )
        })
        .collect();
    for location in &rename.locations {
        let path = snapshot.analysis_snapshot.path(location.file_id);
        let source = snapshot.analysis_snapshot.source_path(path)?;
        if !snapshot.loader.is_editable(path, &source)? {
            anyhow::bail!(
                "Cannot rename a declaration or reference in external repository {}",
                path.display()
            );
        }
        for alias in snapshot.analysis_snapshot.source_aliases(path)? {
            if !matched.contains(&(alias.as_path(), location.range)) {
                anyhow::bail!(
                    "Cannot rename shared source {}: the interpretation at {} is outside this rename",
                    source.display(), alias.display()
                );
            }
        }
    }
    Ok(Some((file_id, rename)))
}

pub(crate) fn prepare_rename(
    snapshot: &ServerSnapshot,
    params: lsp_types::TextDocumentPositionParams,
) -> anyhow::Result<Option<lsp_types::PrepareRenameResponse>> {
    let (file, rename) = try_opt!(rename_locations(snapshot, params, None)?);
    let source = snapshot.analysis_snapshot.source(file)?;
    let range = try_opt!(convert::lsp_range_from_text_range(rename.range, &source));
    Ok(Some(lsp_types::PrepareRenameResponse::Range(range)))
}

pub(crate) fn rename(
    snapshot: &ServerSnapshot,
    params: lsp_types::RenameParams,
) -> anyhow::Result<Option<lsp_types::WorkspaceEdit>> {
    let (_, rename) = try_opt!(rename_locations(
        snapshot,
        params.text_document_position,
        Some(&params.new_name)
    )?);
    let mut documents = BTreeMap::new();
    let mut edits = HashSet::new();
    for location in rename.locations {
        let path = snapshot.analysis_snapshot.path(location.file_id);
        let physical = snapshot.analysis_snapshot.source_path(path)?;
        if !edits.insert((physical.clone(), location.range)) {
            continue;
        }
        let source = snapshot.analysis_snapshot.source(location.file_id)?;
        let document = snapshot.analysis_snapshot.document(path);
        let uri_path = document.map_or(physical.as_path(), |document| document.path.as_std_path());
        let uri = lsp_types::Url::from_file_path(uri_path).map_err(|()| {
            anyhow::anyhow!("Cannot construct an editor URI for {}", uri_path.display())
        })?;
        let range =
            convert::lsp_range_from_text_range(location.range, &source).ok_or_else(|| {
                anyhow::anyhow!("Cannot convert a rename range in {}", path.display())
            })?;
        let edit = lsp_types::TextEdit {
            range,
            new_text: params.new_name.clone(),
        };
        documents
            .entry(uri.clone())
            .or_insert_with(|| lsp_types::TextDocumentEdit {
                text_document: lsp_types::OptionalVersionedTextDocumentIdentifier {
                    uri,
                    version: document.map(|document| document.version),
                },
                edits: Vec::new(),
            })
            .edits
            .push(lsp_types::OneOf::Left(edit));
    }
    let documents: Vec<_> = documents.into_values().collect();
    let document_changes = snapshot
        .config
        .caps
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.workspace_edit.as_ref())
        .and_then(|edit| edit.document_changes)
        .unwrap_or(false);
    if !document_changes
        && documents
            .iter()
            .any(|document| document.text_document.version.is_some())
    {
        anyhow::bail!(
            "Renaming open documents requires client support for versioned document changes"
        );
    }
    Ok(Some(if document_changes {
        lsp_types::WorkspaceEdit {
            document_changes: Some(lsp_types::DocumentChanges::Edits(documents)),
            ..Default::default()
        }
    } else {
        lsp_types::WorkspaceEdit {
            changes: Some(
                documents
                    .into_iter()
                    .map(|document| {
                        (
                            document.text_document.uri,
                            document
                                .edits
                                .into_iter()
                                .map(|edit| match edit {
                                    lsp_types::OneOf::Left(edit) => edit,
                                    lsp_types::OneOf::Right(_) => {
                                        unreachable!("rename uses plain text edits")
                                    }
                                })
                                .collect(),
                        )
                    })
                    .collect(),
            ),
            ..Default::default()
        }
    }))
}

pub(crate) fn document_highlights(
    snapshot: &ServerSnapshot,
    params: lsp_types::DocumentHighlightParams,
) -> anyhow::Result<Option<Vec<lsp_types::DocumentHighlight>>> {
    let position = params.text_document_position_params;
    let path = path_buf_from_url(&position.text_document.uri)?;
    let file_id = try_opt!(snapshot.analysis_snapshot.open_file(&path)?);
    let source = snapshot.analysis_snapshot.source(file_id)?;
    let pos = try_opt!(convert::offset_from_lsp_position(
        &source.text,
        &source.index,
        position.position
    ));
    let highlights = snapshot
        .analysis_snapshot
        .document_highlights(FilePosition { file_id, pos })?;
    Ok(highlights.map(|references| {
        references
            .into_iter()
            .filter_map(|reference| {
                let range = reference.range();
                let range = starpls_syntax::TextRange::new(
                    u32::from(range.start()).into(),
                    u32::from(range.end()).into(),
                );
                Some(lsp_types::DocumentHighlight {
                    range: convert::lsp_range_from_text_range(range, &source)?,
                    kind: Some(match reference.kind() {
                        starpls_ide::ReferenceKind::Read => lsp_types::DocumentHighlightKind::READ,
                        starpls_ide::ReferenceKind::Write => {
                            lsp_types::DocumentHighlightKind::WRITE
                        }
                        starpls_ide::ReferenceKind::Other => lsp_types::DocumentHighlightKind::TEXT,
                    }),
                })
            })
            .collect()
    }))
}

pub(crate) fn selection_ranges(
    snapshot: &ServerSnapshot,
    params: lsp_types::SelectionRangeParams,
) -> anyhow::Result<Option<Vec<lsp_types::SelectionRange>>> {
    let path = path_buf_from_url(&params.text_document.uri)?;
    let file_id = try_opt!(snapshot.analysis_snapshot.open_file(&path)?);
    let source = snapshot.analysis_snapshot.source(file_id)?;
    let mut selections = Vec::with_capacity(params.positions.len());
    for position in params.positions {
        let pos = try_opt!(convert::offset_from_lsp_position(
            &source.text,
            &source.index,
            position
        ));
        let ranges = snapshot
            .analysis_snapshot
            .selection_ranges(FilePosition { file_id, pos })?;
        let mut parent = None;
        for range in ranges {
            parent = Some(lsp_types::SelectionRange {
                range: try_opt!(convert::lsp_range_from_text_range(range, &source)),
                parent: parent.map(Box::new),
            });
        }
        selections.push(try_opt!(parent));
    }
    Ok(Some(selections))
}

pub(crate) fn folding_ranges(
    snapshot: &ServerSnapshot,
    params: lsp_types::FoldingRangeParams,
) -> anyhow::Result<Option<Vec<lsp_types::FoldingRange>>> {
    let path = path_buf_from_url(&params.text_document.uri)?;
    let file = try_opt!(snapshot.analysis_snapshot.open_file(&path)?);
    let source = snapshot.analysis_snapshot.source(file)?;
    let line_only = snapshot
        .config
        .caps
        .text_document
        .as_ref()
        .and_then(|caps| caps.folding_range.as_ref())
        .and_then(|caps| caps.line_folding_only)
        .unwrap_or(false);
    let ranges = snapshot.analysis_snapshot.folding_ranges(file)?;
    Ok(Some(
        ranges
            .into_iter()
            .filter_map(|fold| convert::lsp_folding_range(fold, &source, line_only))
            .collect(),
    ))
}

pub(crate) fn inlay_hints(
    snapshot: &ServerSnapshot,
    params: lsp_types::InlayHintParams,
) -> anyhow::Result<Option<Vec<lsp_types::InlayHint>>> {
    let path = path_buf_from_url(&params.text_document.uri)?;
    let file = try_opt!(snapshot.analysis_snapshot.open_file(&path)?);
    let source = snapshot.analysis_snapshot.source(file)?;
    let range = try_opt!(convert::text_range_from_lsp_range(params.range, &source));
    let hints = snapshot.analysis_snapshot.inlay_hints(file, range)?;
    let mut result = Vec::with_capacity(hints.len());
    for hint in hints {
        let starpls_ide::InlayHint {
            position,
            kind,
            label,
            text_edits: _,
        } = hint;
        let position = starpls_syntax::TextRange::empty(u32::from(position).into());
        let Some(position) = convert::lsp_range_from_text_range(position, &source) else {
            continue;
        };
        let mut parts = Vec::with_capacity(label.parts().len());
        for part in label.parts() {
            let location = match part.target() {
                Some(target) => {
                    if let Some(path) = snapshot.analysis_snapshot.system_path(target.file()) {
                        let target_source = snapshot.analysis_snapshot.source(target.file())?;
                        let range = target.focus_range();
                        let range = starpls_syntax::TextRange::new(
                            u32::from(range.start()).into(),
                            u32::from(range.end()).into(),
                        );
                        match (
                            lsp_types::Url::from_file_path(path),
                            convert::lsp_range_from_text_range(range, &target_source),
                        ) {
                            (Ok(uri), Some(range)) => Some(lsp_types::Location { uri, range }),
                            _ => None,
                        }
                    } else {
                        None
                    }
                }
                None => None,
            };
            parts.push(lsp_types::InlayHintLabelPart {
                value: part.text().to_owned(),
                location,
                tooltip: None,
                command: None,
            });
        }
        result.push(lsp_types::InlayHint {
            position: position.start,
            label: lsp_types::InlayHintLabel::LabelParts(parts),
            kind: Some(match kind {
                starpls_ide::InlayHintKind::Type => lsp_types::InlayHintKind::TYPE,
                starpls_ide::InlayHintKind::CallArgumentName => lsp_types::InlayHintKind::PARAMETER,
            }),
            text_edits: None,
            tooltip: None,
            padding_left: None,
            padding_right: None,
            data: None,
        });
    }
    Ok(Some(result))
}

pub(crate) fn completion(
    snapshot: &ServerSnapshot,
    params: lsp_types::CompletionParams,
) -> anyhow::Result<Option<lsp_types::CompletionResponse>> {
    let path = path_buf_from_url(&params.text_document_position.text_document.uri)?;
    let file_id = try_opt!(snapshot.analysis_snapshot.open_file(&path)?);
    let source = snapshot.analysis_snapshot.source(file_id)?;
    let pos = try_opt!(convert::text_size_from_lsp_position(
        snapshot,
        file_id,
        params.text_document_position.position,
    )?);

    Ok(Some(
        snapshot
            .analysis_snapshot
            .completions(
                FilePosition { file_id, pos },
                params.context.and_then(|cx| cx.trigger_character),
            )?
            .unwrap_or_else(Vec::new)
            .into_iter()
            .flat_map(|item| {
                let sort_text = Some(item.sort_text());
                let (insert_text, text_edit) = match item.mode {
                    Some(mode) => match mode {
                        InsertText(text) => (Some(text), None),
                        TextEdit(edit) => (
                            None,
                            Some(match edit {
                                Edit::TextEdit(edit) => {
                                    lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
                                        range: convert::lsp_range_from_text_range(
                                            edit.range, &source,
                                        )?,
                                        new_text: edit.new_text,
                                    })
                                }
                                Edit::InsertReplaceEdit(edit)
                                    if snapshot.config.has_insert_replace_support() =>
                                {
                                    lsp_types::CompletionTextEdit::InsertAndReplace(
                                        lsp_types::InsertReplaceEdit {
                                            new_text: edit.new_text,
                                            insert: convert::lsp_range_from_text_range(
                                                edit.insert,
                                                &source,
                                            )?,
                                            replace: convert::lsp_range_from_text_range(
                                                edit.replace,
                                                &source,
                                            )?,
                                        },
                                    )
                                }
                                _ => return None,
                            }),
                        ),
                    },
                    None => (None, None),
                };

                Some(lsp_types::CompletionItem {
                    label: item.label,
                    kind: Some(match item.kind {
                        CompletionItemKind::Function => lsp_types::CompletionItemKind::FUNCTION,
                        CompletionItemKind::Field => lsp_types::CompletionItemKind::FIELD,
                        CompletionItemKind::Variable => lsp_types::CompletionItemKind::VARIABLE,
                        CompletionItemKind::Module => lsp_types::CompletionItemKind::MODULE,
                        CompletionItemKind::Keyword => lsp_types::CompletionItemKind::KEYWORD,
                        CompletionItemKind::File => lsp_types::CompletionItemKind::FILE,
                        CompletionItemKind::Folder => lsp_types::CompletionItemKind::FOLDER,
                        CompletionItemKind::Constant => lsp_types::CompletionItemKind::CONSTANT,
                    }),
                    sort_text,
                    insert_text,
                    text_edit,
                    filter_text: item.filter_text,
                    ..Default::default()
                })
            })
            .collect::<Vec<_>>()
            .into(),
    ))
}

pub(crate) fn semantic_tokens(
    snapshot: &ServerSnapshot,
    params: lsp_types::SemanticTokensParams,
) -> anyhow::Result<Option<lsp_types::SemanticTokensResult>> {
    Ok(
        semantic_tokens_impl(snapshot, &params.text_document.uri, None)?
            .map(lsp_types::SemanticTokensResult::Tokens),
    )
}

pub(crate) fn semantic_tokens_range(
    snapshot: &ServerSnapshot,
    params: lsp_types::SemanticTokensRangeParams,
) -> anyhow::Result<Option<lsp_types::SemanticTokensRangeResult>> {
    Ok(
        semantic_tokens_impl(snapshot, &params.text_document.uri, Some(params.range))?
            .map(lsp_types::SemanticTokensRangeResult::Tokens),
    )
}

fn semantic_tokens_impl(
    snapshot: &ServerSnapshot,
    uri: &lsp_types::Url,
    range: Option<lsp_types::Range>,
) -> anyhow::Result<Option<lsp_types::SemanticTokens>> {
    let path = path_buf_from_url(uri)?;
    let file = try_opt!(snapshot.analysis_snapshot.open_file(&path)?);
    let source = snapshot.analysis_snapshot.source(file)?;
    let range = match range {
        Some(range) => Some(try_opt!(convert::text_range_from_lsp_range(range, &source))),
        None => None,
    };
    let tokens = snapshot.analysis_snapshot.semantic_tokens(file, range)?;
    Ok(Some(lsp_types::SemanticTokens {
        result_id: None,
        data: convert::lsp_semantic_tokens(&tokens, &source),
    }))
}

pub(crate) fn hover(
    snapshot: &ServerSnapshot,
    params: lsp_types::HoverParams,
) -> anyhow::Result<Option<lsp_types::Hover>> {
    let path = path_buf_from_url(&params.text_document_position_params.text_document.uri)?;
    let file_id = try_opt!(snapshot.analysis_snapshot.open_file(&path)?);
    let pos = try_opt!(convert::text_size_from_lsp_position(
        snapshot,
        file_id,
        params.text_document_position_params.position,
    )?);
    Ok(snapshot
        .analysis_snapshot
        .hover(FilePosition { file_id, pos })?
        .map(|hover| lsp_types::Hover {
            contents: lsp_types::HoverContents::Markup(lsp_types::MarkupContent {
                kind: lsp_types::MarkupKind::Markdown,
                value: hover.contents.value,
            }),
            range: None,
        }))
}

pub(crate) fn signature_help(
    snapshot: &ServerSnapshot,
    params: lsp_types::SignatureHelpParams,
) -> anyhow::Result<Option<lsp_types::SignatureHelp>> {
    let path = path_buf_from_url(&params.text_document_position_params.text_document.uri)?;
    let file_id = try_opt!(snapshot.analysis_snapshot.open_file(&path)?);
    let pos = try_opt!(convert::text_size_from_lsp_position(
        snapshot,
        file_id,
        params.text_document_position_params.position,
    )?);
    Ok(snapshot
        .analysis_snapshot
        .signature_help(FilePosition { file_id, pos })?
        .map(|help| lsp_types::SignatureHelp {
            signatures: help
                .signatures
                .into_iter()
                .map(|sig| lsp_types::SignatureInformation {
                    label: sig.label,
                    documentation: sig.documentation.map(to_markup_doc),
                    parameters: sig.parameters.map(|params| {
                        params
                            .into_iter()
                            .map(|param| lsp_types::ParameterInformation {
                                label: lsp_types::ParameterLabel::Simple(param.label),
                                documentation: param.documentation.map(to_markup_doc),
                            })
                            .collect()
                    }),
                    active_parameter: sig.active_parameter.map(|i| i as u32),
                })
                .collect(),
            active_signature: None,
            active_parameter: None,
        }))
}

pub(crate) fn document_symbols(
    snapshot: &ServerSnapshot,
    params: lsp_types::DocumentSymbolParams,
) -> anyhow::Result<Option<lsp_types::DocumentSymbolResponse>> {
    let path = path_buf_from_url(&params.text_document.uri)?;
    let file_id = try_opt!(snapshot.analysis_snapshot.open_file(&path)?);
    let source = snapshot.analysis_snapshot.source(file_id)?;
    Ok(snapshot
        .analysis_snapshot
        .document_symbols(file_id)?
        .map(|symbols| {
            symbols
                .into_iter()
                .filter_map(|symbol| convert::lsp_document_symbol_from_native(symbol, &source))
                .collect::<Vec<_>>()
                .into()
        }))
}

fn to_markup_doc(doc: String) -> lsp_types::Documentation {
    lsp_types::Documentation::MarkupContent(lsp_types::MarkupContent {
        kind: lsp_types::MarkupKind::Markdown,
        value: doc,
    })
}
