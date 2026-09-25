//! Host inputs and source annotations for shared semantic analysis.

use std::sync::Arc;

use rustc_hash::FxHashMap;
use rustc_hash::FxHashSet;
use starpls_bazel::Builtins;
use starpls_common::Dialect;
use starpls_common::File;

mod fixture;
mod source;
#[cfg(test)]
mod test_database;

pub use fixture::Fixture;
pub use salsa::Cancelled;
pub use source::diagnostics_for_file;
pub use source::Source;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InferenceOptions {
    pub infer_ctx_attributes: bool,
    pub use_code_flow_analysis: bool,
    pub allow_unused_definitions: bool,
    pub skip_load_cycle_checks: bool,
}

#[salsa::input(debug)]
pub struct BuiltinDefs {
    #[returns(ref)]
    pub builtins: Builtins,
    #[returns(ref)]
    pub rules: starpls_bazel::build::BuildLanguage,
}

#[salsa::db]
pub trait Db: starpls_common::Db {
    fn environment(&self) -> Environment;

    fn set_builtin_defs(
        &mut self,
        dialect: Dialect,
        builtins: Builtins,
        rules: starpls_bazel::build::BuildLanguage,
    ) -> anyhow::Result<()>;

    fn get_builtin_defs(&self, dialect: &Dialect) -> BuiltinDefs;

    fn set_bazel_prelude_file(&mut self, file_id: File);
    fn get_bazel_prelude_file(&self) -> Option<File>;
    fn set_all_workspace_targets(&mut self, targets: Vec<String>);
    fn get_all_workspace_targets(&self) -> Arc<Vec<String>>;
}

/// Syntax correspondence used by a synchronous stub validation pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StubValidation {
    pub annotations:
        FxHashMap<(ruff_db::files::File, ruff_python_ast::NodeIndex), ValidationAnnotation>,
    pub provider_returns:
        FxHashMap<(ruff_db::files::File, ruff_python_ast::NodeIndex), ProviderContract>,
    pub files: FxHashSet<ruff_db::files::File>,
}

/// The kind of source check selected by the validation overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValidationAnnotation {
    Declaration {
        file: File,
        owner: ruff_python_ast::NodeIndex,
    },
    ValueContract {
        file: File,
        owner: ruff_python_ast::NodeIndex,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderContract {
    pub stub: File,
    pub class: ruff_python_ast::NodeIndex,
    pub allowed_fields: Box<[ruff_python_ast::name::Name]>,
}

/// Inputs shared by semantic queries. Input identities remain stable when the
/// host changes configuration or discovers previously unavailable modules.
#[salsa::input(debug)]
pub struct Environment {
    #[returns(ref)]
    pub options: InferenceOptions,
    #[returns(clone)]
    pub standard_builtins: BuiltinDefs,
    #[returns(clone)]
    pub bazel_builtins: BuiltinDefs,
    #[returns(clone)]
    pub prelude_file: Option<File>,
    #[returns(clone)]
    pub all_workspace_targets: Arc<Vec<String>>,
    #[returns(clone)]
    pub load_revision: u64,
    #[returns(ref)]
    pub type_interfaces: FxHashMap<ruff_db::files::File, (File, File)>,
    #[returns(ref)]
    pub stub_validation: StubValidation,
}

impl Environment {
    pub fn initialize(db: &dyn Db, options: InferenceOptions) -> Self {
        let standard = BuiltinDefs::new(db, Builtins::default(), Default::default());
        let bazel = BuiltinDefs::new(db, Builtins::default(), Default::default());
        Self::new(
            db,
            options,
            standard,
            bazel,
            None,
            Arc::default(),
            0,
            FxHashMap::default(),
            StubValidation::default(),
        )
    }

    pub fn builtin_defs(self, db: &dyn Db, dialect: Dialect) -> BuiltinDefs {
        match dialect {
            Dialect::Standard => self.standard_builtins(db),
            Dialect::Bazel => self.bazel_builtins(db),
        }
    }
}
