use ruff_db::diagnostic::Annotation;
pub use ruff_db::diagnostic::Diagnostic;
pub use ruff_db::diagnostic::DiagnosticId;
use ruff_db::diagnostic::DiagnosticMessage;
pub use ruff_db::diagnostic::DiagnosticTag;
pub use ruff_db::diagnostic::Severity;
use ruff_db::diagnostic::Span;
use starpls_syntax::TextRange;

use crate::File;

/// Creates a source diagnostic with the primary annotation used by the editor.
pub fn diagnostic(
    file: File,
    id: DiagnosticId,
    severity: Severity,
    range: TextRange,
    message: impl Into<String>,
    tags: impl IntoIterator<Item = DiagnosticTag>,
) -> Diagnostic {
    let range = ruff_text_size::TextRange::new(
        u32::from(range.start()).into(),
        u32::from(range.end()).into(),
    );
    let mut annotation = Annotation::primary(Span::from(file.source).with_range(range));
    for tag in tags {
        annotation.push_tag(tag);
    }
    let message: String = message.into();
    let mut diagnostic = Diagnostic::new(id, severity, DiagnosticMessage::from(message));
    diagnostic.annotate(annotation);
    diagnostic
}

#[salsa::accumulator]
#[derive(Clone, Debug)]
pub struct Diagnostics(pub Diagnostic);
