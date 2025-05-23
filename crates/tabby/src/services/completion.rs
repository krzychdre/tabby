mod completion_prompt;
mod next_edit_prompt;

use std::sync::Arc;

use regex::Regex;
use serde::{Deserialize, Serialize};
use tabby_common::{
    api::{
        self,
        code::CodeSearch,
        event::{Event, EventLogger},
    },
    axum::AllowedCodeRepository,
    config::{CompletionConfig, ModelConfig},
    languages::get_language,
};
use tabby_inference::{
    ChatCompletionStream, CodeGeneration, CodeGenerationOptions, CodeGenerationOptionsBuilder,
    CompletionStream,
};
use thiserror::Error;
use utoipa::ToSchema;

use super::model;

#[derive(Error, Debug)]
pub enum CompletionError {
    #[error("empty prompt from completion request")]
    EmptyPrompt,
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
#[schema(example=json!({
    "language": "python",
    "segments": {
        "prefix": "def fib(n):\n    ",
        "suffix": "\n        return fib(n - 1) + fib(n - 2)"
    }
}))]
pub struct CompletionRequest {
    /// Language identifier, full list is maintained at
    /// https://code.visualstudio.com/docs/languages/identifiers
    #[schema(example = "python")]
    language: Option<String>,

    /// When segments are set, the `prompt` is ignored during the inference.
    segments: Option<Segments>,

    /// A unique identifier representing your end-user, which can help Tabby to monitor & generating
    /// reports.
    pub(crate) user: Option<String>,

    debug_options: Option<DebugOptions>,

    /// The temperature parameter for the model, used to tune variance and "creativity" of the model output
    temperature: Option<f32>,

    /// The seed used for randomly selecting tokens
    seed: Option<u64>,

    /// The mode for completion. Use 'standard' for normal code completions or 'next_edit_suggestion'
    /// to predict the next edit the user will make.
    #[serde(default = "default_standard_mode")]
    mode: String,
}

pub fn default_standard_mode() -> String {
    "standard".to_string()
}

/// Contains information about edit history for next edit suggestion mode
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct EditHistory {
    original_code: String,

    /// Unified git-style diff of all edits made to the file
    edits_diff: String,

    /// Current version of the code after all edits
    current_version: String,
}

impl CompletionRequest {
    /// Returns the language info or "unknown" if not specified.
    fn language_or_unknown(&self) -> String {
        self.language.clone().unwrap_or("unknown".to_string())
    }

    /// Returns the raw prompt if specified.
    fn raw_prompt(&self) -> Option<String> {
        self.debug_options
            .as_ref()
            .and_then(|x| x.raw_prompt.clone())
    }

    /// Returns true if retrieval augmented code completion is disabled.
    fn disable_retrieval_augmented_code_completion(&self) -> bool {
        self.debug_options
            .as_ref()
            .is_some_and(|x| x.disable_retrieval_augmented_code_completion)
    }

    /// Returns true if the request is for next edit suggestion mode.
    fn is_next_edit_suggestion_mode(&self) -> bool {
        self.mode == "next_edit_suggestion"
    }
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct DebugOptions {
    /// When `raw_prompt` is specified, it will be passed directly to the inference engine for completion. `segments` field in `CompletionRequest` will be ignored.
    ///
    /// This is useful for certain requests that aim to test the tabby's e2e quality.
    raw_prompt: Option<String>,

    /// When true, returns `snippets` in `debug_data`.
    #[serde(default = "default_false")]
    return_snippets: bool,

    /// When true, returns `prompt` in `debug_data`.
    #[serde(default = "default_false")]
    return_prompt: bool,

    /// When true, disable retrieval augmented code completion.
    #[serde(default = "default_false")]
    disable_retrieval_augmented_code_completion: bool,
}

fn default_false() -> bool {
    false
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct Segments {
    /// Content that appears before the cursor in the editor window.
    prefix: String,

    /// Content that appears after the cursor in the editor window.
    suffix: Option<String>,

    /// The relative path of the file that is being edited.
    /// - When [Segments::git_url] is set, this is the path of the file in the git repository.
    /// - When [Segments::git_url] is empty, this is the path of the file in the workspace.
    filepath: Option<String>,

    /// The remote URL of the current git repository.
    /// Leave this empty if the file is not in a git repository,
    /// or the git repository does not have a remote URL.
    git_url: Option<String>,

    /// The relevant declaration code snippets provided by the editor's LSP,
    /// contain declarations of symbols extracted from [Segments::prefix].
    declarations: Option<Vec<Declaration>>,

    /// The relevant code snippets extracted from recently edited files.
    /// These snippets are selected from candidates found within code chunks
    /// based on the edited location.
    /// The current editing file is excluded from the search candidates.
    ///
    /// When provided alongside [Segments::declarations], the snippets have
    /// already been deduplicated to ensure no duplication with entries
    /// in [Segments::declarations].
    ///
    /// Sorted in descending order of [Snippet::score].
    relevant_snippets_from_changed_files: Option<Vec<Snippet>>,

    /// The relevant code snippets extracted from recently opened files.
    /// These snippets are selected from candidates found within code chunks
    /// based on the last visited location.
    ///
    /// Current Active file is excluded from the search candidates.
    /// When provided with [Segments::relevant_snippets_from_changed_files], the snippets have
    /// already been deduplicated to ensure no duplication with entries
    /// in [Segments::relevant_snippets_from_changed_files].
    relevant_snippets_from_recently_opened_files: Option<Vec<Snippet>>,

    /// Clipboard content when requesting code completion.
    clipboard: Option<String>,

    /// Required when mode is 'next_edit_suggestion'. Contains information about edit history.
    edit_history: Option<EditHistory>,
}

impl From<Segments> for api::event::Segments {
    fn from(val: Segments) -> Self {
        Self {
            prefix: val.prefix,
            suffix: val.suffix,
            clipboard: val.clipboard,
            git_url: val.git_url,
            declarations: val
                .declarations
                .map(|x| x.into_iter().map(Into::into).collect()),
            filepath: val.filepath,
        }
    }
}

/// A snippet of declaration code that is relevant to the current completion request.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct Declaration {
    /// Filepath of the file where the snippet is from.
    /// - When the file belongs to the same workspace as the current file,
    ///   this is a relative filepath, use the same rule as [Segments::filepath].
    /// - When the file located outside the workspace, such as in a dependency package,
    ///   this is a file URI with an absolute filepath.
    pub filepath: String,

    /// Body of the snippet.
    pub body: String,
}

impl From<Declaration> for api::event::Declaration {
    fn from(val: Declaration) -> Self {
        Self {
            filepath: val.filepath,
            body: val.body,
        }
    }
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct Choice {
    index: u32,
    text: String,
}

impl Choice {
    pub fn new(text: String) -> Self {
        Self { index: 0, text }
    }
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug, PartialEq)]
pub struct Snippet {
    filepath: String,
    body: String,
    score: f32,
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
#[schema(example=json!({
    "id": "string",
    "choices": [ { "index": 0, "text": "string" } ]
}))]
pub struct CompletionResponse {
    id: String,
    choices: Vec<Choice>,

    #[serde(skip_serializing_if = "Option::is_none")]
    debug_data: Option<DebugData>,

    #[serde(default = "default_standard_mode")]
    mode: String,
}

impl CompletionResponse {
    pub fn new(
        id: String,
        choices: Vec<Choice>,
        debug_data: Option<DebugData>,
        mode: String,
    ) -> Self {
        Self {
            id,
            choices,
            debug_data,
            mode,
        }
    }
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct DebugData {
    #[serde(skip_serializing_if = "Option::is_none")]
    snippets: Option<Vec<Snippet>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
}

/// CompletionService enhances the CodeGeneration feature by adding Retrieval Augmented Code Completion capability.
/// It enables the retrieval of pertinent code snippets from the code repository,
/// which are then utilized as prompts for the code generation model.
pub struct CompletionService {
    config: CompletionConfig,
    engine: Arc<CodeGeneration>,
    logger: Arc<dyn EventLogger>,
    prompt_builder: completion_prompt::PromptBuilder,
    next_edit_prompt_builder: next_edit_prompt::NextEditPromptBuilder,
}

impl CompletionService {
    fn new(
        config: CompletionConfig,
        engine: Arc<CodeGeneration>,
        code: Arc<dyn CodeSearch>,
        logger: Arc<dyn EventLogger>,
        prompt_template: Option<String>,
    ) -> Self {
        Self {
            engine,
            prompt_builder: completion_prompt::PromptBuilder::new(
                &config.code_search_params,
                prompt_template,
                Some(code),
            ),
            next_edit_prompt_builder: next_edit_prompt::NextEditPromptBuilder::new(),
            config,
            logger,
        }
    }

    async fn build_snippets(
        &self,
        language: &str,
        segments: &Segments,
        allowed_code_repository: &AllowedCodeRepository,
        disable_retrieval_augmented_code_completion: bool,
    ) -> Vec<Snippet> {
        if disable_retrieval_augmented_code_completion {
            return vec![];
        }

        self.prompt_builder
            .collect(language, segments, allowed_code_repository)
            .await
    }

    fn text_generation_options(
        language: &str,
        temperature: Option<f32>,
        seed: Option<u64>,
        max_input_length: usize,
        max_output_tokens: usize,
        mode: String,
    ) -> CodeGenerationOptions {
        let mut builder = CodeGenerationOptionsBuilder::default();
        builder
            .max_input_length(max_input_length)
            .max_decoding_tokens(max_output_tokens as i32)
            .language(Some(get_language(language)));
        temperature.inspect(|x| {
            builder.sampling_temperature(*x);
        });
        seed.inspect(|x| {
            builder.seed(*x);
        });

        builder.mode(mode);

        builder
            .build()
            .expect("Failed to create text generation options")
    }

    pub async fn generate(
        &self,
        request: &CompletionRequest,
        allowed_code_repository: &AllowedCodeRepository,
        user_agent: Option<&str>,
    ) -> Result<CompletionResponse, CompletionError> {
        let completion_id = format!("cmpl-{}", uuid::Uuid::new_v4());
        let language = request.language_or_unknown();

        if request.is_next_edit_suggestion_mode() {
            return self
                .generate_next_edit_suggestion(request, completion_id, language, user_agent)
                .await;
        }

        let options = Self::text_generation_options(
            language.as_str(),
            request.temperature,
            request.seed,
            self.config.max_input_length,
            self.config.max_decoding_tokens,
            request.mode.clone(),
        );

        let mut use_crlf = false;
        let (prompt, segments, snippets) = if let Some(prompt) = request.raw_prompt() {
            (prompt, None, vec![])
        } else if let Some(segments) = request.segments.as_ref() {
            if contains_crlf(segments) {
                use_crlf = true;
            }

            let snippets = self
                .build_snippets(
                    &language,
                    segments,
                    allowed_code_repository,
                    request.disable_retrieval_augmented_code_completion(),
                )
                .await;
            let prompt = self
                .prompt_builder
                .build(&language, segments.clone(), &snippets);

            (override_prompt(prompt, use_crlf), Some(segments), snippets)
        } else {
            return Err(CompletionError::EmptyPrompt);
        };

        let generated_text =
            override_generated_text(self.engine.generate(&prompt, options).await, use_crlf);

        self.logger.log(
            request.user.clone(),
            Event::Completion {
                completion_id: completion_id.clone(),
                language,
                prompt: prompt.clone(),
                segments: segments.cloned().map(|x| x.into()),
                choices: vec![api::event::Choice {
                    index: 0,
                    text: generated_text.clone(),
                }],
                user_agent: user_agent.map(|x| x.to_owned()),
            },
        );

        let debug_data = request
            .debug_options
            .as_ref()
            .map(|debug_options| DebugData {
                snippets: debug_options.return_snippets.then_some(snippets),
                prompt: debug_options.return_prompt.then_some(prompt),
            });

        Ok(CompletionResponse::new(
            completion_id,
            vec![Choice::new(generated_text)],
            debug_data,
            "standard".to_string(),
        ))
    }

    async fn generate_next_edit_suggestion(
        &self,
        request: &CompletionRequest,
        completion_id: String,
        language: String,
        user_agent: Option<&str>,
    ) -> Result<CompletionResponse, CompletionError> {
        let segments = request
            .segments
            .as_ref()
            .ok_or(CompletionError::EmptyPrompt)?;

        let edit_history = segments
            .edit_history
            .as_ref()
            .ok_or(CompletionError::EmptyPrompt)?;

        let prompt = self.next_edit_prompt_builder.build_prompt(edit_history);

        let options = Self::text_generation_options(
            language.as_str(),
            request.temperature,
            request.seed,
            self.config.max_input_length,
            self.config.max_decoding_tokens * 2,
            request.mode.clone(),
        );

        let generated_text = self.engine.generate(&prompt, options).await;

        self.logger.log(
            request.user.clone(),
            Event::Completion {
                completion_id: completion_id.clone(),
                language,
                prompt: prompt.clone(),
                segments: None,
                choices: vec![api::event::Choice {
                    index: 0,
                    text: generated_text.clone(),
                }],
                user_agent: user_agent.map(|x| x.to_owned()),
            },
        );

        let debug_data = request
            .debug_options
            .as_ref()
            .map(|debug_options| DebugData {
                snippets: None,
                prompt: debug_options.return_prompt.then_some(prompt),
            });

        Ok(CompletionResponse::new(
            completion_id,
            vec![Choice::new(generated_text)],
            debug_data,
            "next_edit_suggestion".to_string(),
        ))
    }
}

fn contains_crlf(segments: &Segments) -> bool {
    if segments.prefix.contains("\r\n") {
        return true;
    }
    if let Some(suffix) = &segments.suffix {
        if suffix.contains("\r\n") {
            return true;
        }
    }

    false
}

fn override_prompt(prompt: String, use_crlf: bool) -> String {
    if use_crlf {
        prompt.replace("\r\n", "\n")
    } else {
        prompt
    }
}

/// override_generated_text replaces \n with \r\n in the generated text if use_crlf is true.
/// This is used to ensure that the generated text has the same line endings as the prompt.
///
/// Because there might be \r\n in the text, which also has a `\n` and should not be replaced,
/// we can not simply replace \n with \r\n.
fn override_generated_text(generated: String, use_crlf: bool) -> String {
    if use_crlf {
        let re = Regex::new(r"([^\r])\n").unwrap(); // Match \n that is preceded by anything except \r
        re.replace_all(&generated, "$1\r\n").to_string() // Replace with captured character and \r\n
    } else {
        generated
    }
}

pub async fn create_completion_service_and_chat(
    config: &CompletionConfig,
    code: Arc<dyn CodeSearch>,
    logger: Arc<dyn EventLogger>,
    completion: Option<ModelConfig>,
    chat: Option<ModelConfig>,
) -> (
    Option<CompletionService>,
    Option<Arc<dyn CompletionStream>>,
    Option<Arc<dyn ChatCompletionStream>>,
) {
    let (code_generation, completion_stream, chat, prompt) =
        model::load_code_generation_and_chat(completion, chat).await;

    let completion = code_generation.clone().map(|code_generation| {
        CompletionService::new(
            config.to_owned(),
            code_generation.clone(),
            code,
            logger,
            prompt
                .unwrap_or_else(|| panic!("Prompt template is required for code completion"))
                .prompt_template,
        )
    });

    (completion, completion_stream, chat)
}

#[cfg(test)]
mod tests {
    use api::code::CodeSearchParams;
    use async_stream::stream;
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use tabby_common::api::code::{CodeSearchError, CodeSearchQuery, CodeSearchResponse};
    use tabby_inference::{CompletionOptions, CompletionStream};

    use super::*;

    struct MockEventLogger;

    impl EventLogger for MockEventLogger {
        fn write(&self, _x: api::event::LogEntry) {}
    }

    struct MockCompletionStream;

    #[async_trait]
    impl CompletionStream for MockCompletionStream {
        async fn generate(&self, _prompt: &str, _options: CompletionOptions) -> BoxStream<String> {
            let s = stream! {
                yield r#""Hello, world!""#.into();
            };

            Box::pin(s)
        }
    }

    struct MockCodeSearch;

    #[async_trait]
    impl CodeSearch for MockCodeSearch {
        async fn search_in_language(
            &self,
            _query: CodeSearchQuery,
            _params: CodeSearchParams,
        ) -> Result<CodeSearchResponse, CodeSearchError> {
            Ok(CodeSearchResponse { hits: vec![] })
        }
    }

    fn mock_completion_service() -> CompletionService {
        let generation = CodeGeneration::new(Arc::new(MockCompletionStream), None);
        CompletionService::new(
            CompletionConfig::default(),
            Arc::new(generation),
            Arc::new(MockCodeSearch),
            Arc::new(MockEventLogger),
            Some("<pre>{prefix}<mid>{suffix}<end>".into()),
        )
    }

    #[tokio::test]
    async fn test_completion_service() {
        let completion_service = mock_completion_service();
        let segment = Segments {
            prefix: "fn hello_world() -> &'static str {".into(),
            suffix: Some("}".into()),
            filepath: None,
            git_url: None,
            declarations: None,
            relevant_snippets_from_changed_files: None,
            relevant_snippets_from_recently_opened_files: None,
            clipboard: None,
            edit_history: None,
        };
        let request = CompletionRequest {
            language: Some("rust".into()),
            segments: Some(segment.clone()),
            user: None,
            debug_options: None,
            temperature: None,
            seed: None,
            mode: "standard".into(),
        };

        let allowed_code_repository = AllowedCodeRepository::default();
        let response = completion_service
            .generate(&request, &allowed_code_repository, Some("test user agent"))
            .await
            .unwrap();
        assert_eq!(response.choices[0].text, r#""Hello, world!""#);

        let prompt = completion_service
            .prompt_builder
            .build("rust", segment.clone(), &[]);
        assert_eq!(prompt, "<pre>fn hello_world() -> &'static str {<mid>}<end>");
    }

    #[test]
    fn test_contains_crlf() {
        let contained_crlf = vec![
            Segments {
                prefix: "fn hello_world() -> &'static str {\r\n".into(),
                suffix: Some("}".into()),
                filepath: None,
                git_url: None,
                declarations: None,
                relevant_snippets_from_changed_files: None,
                relevant_snippets_from_recently_opened_files: None,
                clipboard: None,
                edit_history: None,
            },
            Segments {
                prefix: "fn hello_world() -> &'static str {".into(),
                suffix: Some("}\r\n".into()),
                filepath: None,
                git_url: None,
                declarations: None,
                relevant_snippets_from_changed_files: None,
                relevant_snippets_from_recently_opened_files: None,
                clipboard: None,
                edit_history: None,
            },
            Segments {
                prefix: "fn hello_world() -> &'static str {\r\n".into(),
                suffix: Some("}\r\n".into()),
                filepath: None,
                git_url: None,
                declarations: None,
                relevant_snippets_from_changed_files: None,
                relevant_snippets_from_recently_opened_files: None,
                clipboard: None,
                edit_history: None,
            },
        ];
        for segments in contained_crlf {
            assert!(contains_crlf(&segments));
        }

        let not_contained_crlf = vec![Segments {
            prefix: "fn hello_world() -> &'static str {\r".into(),
            suffix: Some("}\n".into()),
            filepath: None,
            git_url: None,
            declarations: None,
            relevant_snippets_from_changed_files: None,
            relevant_snippets_from_recently_opened_files: None,
            clipboard: None,
            edit_history: None,
        }];
        for segments in not_contained_crlf {
            assert!(!contains_crlf(&segments));
        }
    }

    #[test]
    fn test_override_prompt() {
        let prompt = "fn hello_world() -> &'static str {\r\n".to_string();
        let use_crlf = true;
        assert_eq!(
            override_prompt(prompt.clone(), use_crlf),
            "fn hello_world() -> &'static str {\n"
        );

        let use_crlf = false;
        assert_eq!(override_prompt(prompt.clone(), use_crlf), prompt);
    }

    #[test]
    fn test_override_generated() {
        let cases = vec![
            (
                "fn hello_world() -> &'static str {\r\n".to_string(),
                "fn hello_world() -> &'static str {\r\n".to_string(),
            ),
            (
                "fn hello_world() -> &'static str {\n".to_string(),
                "fn hello_world() -> &'static str {\r\n".to_string(),
            ),
            (
                "fn hello_world() -> &'static str {\r".to_string(),
                "fn hello_world() -> &'static str {\r".to_string(),
            ),
            (
                "fn hello_world() -> &'static str {".to_string(),
                "fn hello_world() -> &'static str {".to_string(),
            ),
        ];

        for (generated, expected) in cases {
            assert_eq!(override_generated_text(generated, true), expected);
        }
    }
}
