/// Metadata about a model, particularly OpenAI models.
/// We may want to consider including details like the pricing for
/// input tokens, output tokens, etc., though users will need to be able to
/// override this in config.toml, as this information can get out of date.
/// Though this would help present more accurate pricing information in the UI.
#[derive(Debug)]
pub(crate) struct ModelInfo {
    /// Size of the context window in tokens.
    pub(crate) context_window: u64,

    /// Maximum number of output tokens that can be generated for the model.
    pub(crate) max_output_tokens: u64,
}

/// Note details such as what a model like gpt-4o is aliased to may be out of
/// date.
pub(crate) fn get_model_info(name: &str) -> Option<ModelInfo> {
    match name {
        // https://platform.openai.com/docs/models/o3
        "o3" => Some(ModelInfo {
            context_window: 200_000,
            max_output_tokens: 100_000,
        }),

        // https://platform.openai.com/docs/models/o4-mini
        "o4-mini" => Some(ModelInfo {
            context_window: 200_000,
            max_output_tokens: 100_000,
        }),

        // https://platform.openai.com/docs/models/codex-mini-latest
        "codex-mini-latest" => Some(ModelInfo {
            context_window: 200_000,
            max_output_tokens: 100_000,
        }),

        // As of Jun 25, 2025, gpt-4.1 defaults to gpt-4.1-2025-04-14.
        // https://platform.openai.com/docs/models/gpt-4.1
        "gpt-4.1" | "gpt-4.1-2025-04-14" => Some(ModelInfo {
            context_window: 1_047_576,
            max_output_tokens: 32_768,
        }),

        // As of Jun 25, 2025, gpt-4o defaults to gpt-4o-2024-08-06.
        // https://platform.openai.com/docs/models/gpt-4o
        "gpt-4o" | "gpt-4o-2024-08-06" => Some(ModelInfo {
            context_window: 128_000,
            max_output_tokens: 16_384,
        }),

        // https://platform.openai.com/docs/models/gpt-4o?snapshot=gpt-4o-2024-05-13
        "gpt-4o-2024-05-13" => Some(ModelInfo {
            context_window: 128_000,
            max_output_tokens: 4_096,
        }),

        // https://platform.openai.com/docs/models/gpt-4o?snapshot=gpt-4o-2024-11-20
        "gpt-4o-2024-11-20" => Some(ModelInfo {
            context_window: 128_000,
            max_output_tokens: 16_384,
        }),

        // https://platform.openai.com/docs/models/gpt-3.5-turbo
        "gpt-3.5-turbo" => Some(ModelInfo {
            context_window: 16_385,
            max_output_tokens: 4_096,
        }),

        _ => None,
    }
}
