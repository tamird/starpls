use std::collections::HashMap;

use starpls_common::DocumentStamp;
use starpls_common::File;
use starpls_ide::AnalysisSnapshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DiagnosticTicket {
    job: u64,
    pub(crate) document: Option<DocumentStamp>,
    revision: ruff_db::file_revision::FileRevision,
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
        let file = snapshot
            .open_file(path)
            .ok()?
            .or_else(|| snapshot.is_type_interface_root(file).then_some(file))?;
        let document = snapshot.document(path).map(|document| document.stamp());
        let revision = snapshot.file_revision(file).ok()?;
        let Self {
            next_job,
            requested,
        } = self;
        *next_job += 1;
        let ticket = DiagnosticTicket {
            job: *next_job,
            document,
            revision,
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
        snapshot: &AnalysisSnapshot,
        file: File,
        ticket: DiagnosticTicket,
    ) -> bool {
        let Self {
            next_job: _,
            requested,
        } = self;
        let current = snapshot
            .document(snapshot.path(file))
            .map(|document| document.stamp());
        if current != ticket.document
            || snapshot.file_revision(file).ok() != Some(ticket.revision)
            || requested.get(&file) != Some(&ticket)
        {
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
        assert!(!jobs.complete(&analysis.snapshot(), file, first));
        assert!(jobs.complete(&analysis.snapshot(), file, second));
        assert!(!jobs.complete(&analysis.snapshot(), file, second));

        let (_, old) = jobs.request(&analysis.snapshot(), file).unwrap();
        analysis
            .open_document(path, Dialect::Standard, None, "value = 2\n".into(), 2)
            .unwrap();
        let edited = analysis.document(path).unwrap().stamp();
        assert!(!jobs.complete(&analysis.snapshot(), file, old));
        let (_, current) = jobs.request(&analysis.snapshot(), file).unwrap();
        assert_eq!(current.document.unwrap().version, 2);
        assert!(jobs.complete(&analysis.snapshot(), file, current));

        let (_, before_close) = jobs.request(&analysis.snapshot(), file).unwrap();
        analysis.close_document(path).unwrap();
        assert!(!jobs.complete(&analysis.snapshot(), file, before_close));
        analysis
            .open_document(path, Dialect::Standard, None, "value = 2\n".into(), 2)
            .unwrap();
        let reopened = analysis.document(path).unwrap().stamp();
        assert_ne!(edited, reopened);
        assert!(!jobs.complete(&analysis.snapshot(), file, before_close));
        let (_, current) = jobs.request(&analysis.snapshot(), file).unwrap();
        assert!(jobs.complete(&analysis.snapshot(), file, current));
    }
}
