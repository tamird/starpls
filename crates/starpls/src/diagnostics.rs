use std::collections::HashMap;

use starpls_common::DocumentStamp;
use starpls_common::File;
use starpls_ide::AnalysisSnapshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DiagnosticTicket {
    job: u64,
    pub(crate) document: DocumentStamp,
}

/// Owns outstanding diagnostic jobs. Completed results are sent immediately;
/// cached copies cannot outlive the editor revision they describe.
#[derive(Default)]
pub(crate) struct DiagnosticsManager {
    next_job: u64,
    requested: HashMap<File, DiagnosticTicket>,
}

impl DiagnosticsManager {
    pub(crate) fn request(
        &mut self,
        snapshot: &AnalysisSnapshot,
        file: File,
    ) -> Option<(File, DiagnosticTicket)> {
        let path = snapshot.path(file);
        // A fetch may have been requested while interpreting an open query
        // file as an imported module. Publish its editor interpretation.
        let file = snapshot.open_file(path).ok()??;
        let document = snapshot.document(path)?.stamp();
        let Self {
            next_job,
            requested,
        } = self;
        *next_job += 1;
        let ticket = DiagnosticTicket {
            job: *next_job,
            document,
        };
        requested.insert(file, ticket);
        Some((file, ticket))
    }

    pub(crate) fn cancel_all(&mut self) {
        let Self {
            next_job: _,
            requested,
        } = self;
        requested.clear();
    }

    pub(crate) fn complete(
        &mut self,
        file: File,
        ticket: DiagnosticTicket,
        current: Option<DocumentStamp>,
    ) -> bool {
        let Self {
            next_job: _,
            requested,
        } = self;
        if current != Some(ticket.document) || requested.get(&file) != Some(&ticket) {
            return false;
        }
        requested.remove(&file);
        true
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use ruff_db::system::InMemorySystem;
    use starpls_bazel::client::BazelCLI;
    use starpls_bazel::APIContext;
    use starpls_common::Dialect;
    use starpls_common::FileInfo;
    use starpls_ide::Analysis;

    use super::DiagnosticsManager;
    use crate::document::DefaultFileLoader;

    #[test]
    fn completed_jobs_belong_to_their_requested_buffers() {
        let (sender, _) = crossbeam_channel::unbounded();
        let loader = DefaultFileLoader::new(
            Arc::new(BazelCLI::new("bazel")),
            Default::default(),
            None,
            Default::default(),
            sender,
            false,
        );
        let mut analysis = Analysis::with_system(
            Arc::new(loader),
            Default::default(),
            InMemorySystem::default(),
        );
        let path = Path::new("/main.star");
        let file = analysis
            .open_document(path, Dialect::Standard, None, "value = 1\n".into(), 1)
            .unwrap();
        let mut jobs = DiagnosticsManager::default();
        let stamp = analysis.document(path).unwrap().stamp();
        let imported = analysis
            .file(
                path,
                Dialect::Bazel,
                Some(FileInfo::Bazel {
                    api_context: APIContext::Bzl,
                    is_external: false,
                }),
            )
            .unwrap();
        let (editor_file, _) = jobs.request(&analysis.snapshot(), imported).unwrap();
        assert_eq!(editor_file, file);
        let (_, first) = jobs.request(&analysis.snapshot(), file).unwrap();
        let (_, second) = jobs.request(&analysis.snapshot(), file).unwrap();
        assert!(!jobs.complete(file, first, Some(stamp)));
        assert!(jobs.complete(file, second, Some(stamp)));
        assert!(!jobs.complete(file, second, Some(stamp)));

        let (_, old) = jobs.request(&analysis.snapshot(), file).unwrap();
        analysis
            .open_document(path, Dialect::Standard, None, "value = 2\n".into(), 2)
            .unwrap();
        let edited = analysis.document(path).unwrap().stamp();
        assert!(!jobs.complete(file, old, Some(edited)));
        let (_, current) = jobs.request(&analysis.snapshot(), file).unwrap();
        assert_eq!(current.document.version, 2);
        assert!(jobs.complete(file, current, Some(edited)));

        let (_, before_close) = jobs.request(&analysis.snapshot(), file).unwrap();
        analysis.close_document(path).unwrap();
        assert!(!jobs.complete(file, before_close, None));
        analysis
            .open_document(path, Dialect::Standard, None, "value = 2\n".into(), 2)
            .unwrap();
        let reopened = analysis.document(path).unwrap().stamp();
        assert_ne!(edited, reopened);
        assert!(!jobs.complete(file, before_close, Some(reopened)));
        let (_, current) = jobs.request(&analysis.snapshot(), file).unwrap();
        assert!(jobs.complete(file, current, Some(reopened)));
    }
}
