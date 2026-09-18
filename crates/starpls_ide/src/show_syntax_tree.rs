use starpls_common::parse;
use starpls_common::File;

use crate::Database;

pub(crate) fn show_syntax_tree(db: &Database, file_id: File) -> Option<String> {
    let file = file_id;
    let parse = parse(db, file);
    Some(format!("{:#?}", parse.syntax()))
}
