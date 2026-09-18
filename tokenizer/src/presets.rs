use std::sync::LazyLock;

use crate::{CompiledTokenizerPipeline, TokenizerPipelineSpec};

/// Stable default tokenizer pipeline spec used by Stanum for indexing and query evaluation.
pub fn default_pipeline_spec() -> &'static TokenizerPipelineSpec {
    static SPEC: LazyLock<TokenizerPipelineSpec> =
        LazyLock::new(TokenizerPipelineSpec::stanum_default);
    &SPEC
}

/// Lazily compiled default tokenization pipeline used by Stanum for indexing and query evaluation.
pub fn default_pipeline() -> &'static CompiledTokenizerPipeline {
    static PIPELINE: LazyLock<CompiledTokenizerPipeline> = LazyLock::new(|| {
        (*default_pipeline_spec())
            .compile()
            .expect("Stanum default tokenizer pipeline spec should always compile")
    });
    &PIPELINE
}
