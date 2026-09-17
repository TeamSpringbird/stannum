use std::sync::LazyLock;

use crate::{CompiledTokenizerPipeline, TokenizerPipelineSpec};

/// Stable default tokenizer pipeline spec used by tin for indexing and query evaluation.
pub fn default_pipeline_spec() -> &'static TokenizerPipelineSpec {
    static SPEC: LazyLock<TokenizerPipelineSpec> =
        LazyLock::new(TokenizerPipelineSpec::tin_default);
    &SPEC
}

/// Lazily compiled default tokenization pipeline used by tin for indexing and query evaluation.
pub fn default_pipeline() -> &'static CompiledTokenizerPipeline {
    static PIPELINE: LazyLock<CompiledTokenizerPipeline> = LazyLock::new(|| {
        (*default_pipeline_spec())
            .compile()
            .expect("tin default tokenizer pipeline spec should always compile")
    });
    &PIPELINE
}
