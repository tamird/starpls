use starpls_common::parsed_module;
use starpls_common::File;

use crate::Database;

pub(crate) fn show_syntax_tree(db: &Database, file_id: File) -> Option<String> {
    let file = file_id;
    let parse = parsed_module(db, file).load(db);
    Some(format!("{:#?}", parse.syntax()))
}
