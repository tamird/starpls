use starpls_common::File;

use crate::Database;

pub(crate) fn show_hir(_db: &Database, _file_id: File) -> Option<String> {
    Some("Note: This functionality is now deprecated.".to_string())
}
