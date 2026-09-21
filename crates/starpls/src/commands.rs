use clap::Args;

pub(crate) mod check;
pub(crate) mod server;
mod stub_package;
pub(crate) mod type_interface;

#[derive(Args, Default)]
pub(crate) struct InferenceOptions {
    /// Infer attributes on a rule implementation function's context parameter.
    #[clap(long = "experimental_infer_ctx_attributes", default_value_t = false)]
    pub(crate) infer_ctx_attributes: bool,

    /// Report unreachable code and possibly unbound variables.
    /// Type inference always uses code-flow analysis.
    #[clap(long = "experimental_use_code_flow_analysis", default_value_t = false)]
    pub(crate) use_code_flow_analysis: bool,
}
