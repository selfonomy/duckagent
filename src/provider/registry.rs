use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApiMode {
    ChatCompletions,
    CodexResponses,
    AnthropicMessages,
    GeminiNative,
    GeminiCloudcode,
    BedrockConverse,
    CopilotAcp,
}

impl ApiMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "chat" | "chat_completions" | "chat-completions" => Some(Self::ChatCompletions),
            "responses" | "codex_responses" | "codex-responses" => Some(Self::CodexResponses),
            "anthropic" | "anthropic_messages" | "anthropic-messages" => {
                Some(Self::AnthropicMessages)
            }
            "gemini" | "gemini_native" | "gemini-native" => Some(Self::GeminiNative),
            "gemini_cloudcode" | "gemini-cloudcode" | "google-gemini-cli" => {
                Some(Self::GeminiCloudcode)
            }
            "bedrock" | "bedrock_converse" | "bedrock-converse" => Some(Self::BedrockConverse),
            "copilot_acp" | "copilot-acp" => Some(Self::CopilotAcp),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::CodexResponses => "codex_responses",
            Self::AnthropicMessages => "anthropic_messages",
            Self::GeminiNative => "gemini_native",
            Self::GeminiCloudcode => "gemini_cloudcode",
            Self::BedrockConverse => "bedrock_converse",
            Self::CopilotAcp => "copilot_acp",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Nous,
    OpenRouter,
    AiGateway,
    Anthropic,
    OpenAi,
    OpenAiCodex,
    Xiaomi,
    Nvidia,
    QwenOauth,
    Copilot,
    CopilotAcp,
    HuggingFace,
    Gemini,
    GoogleGeminiCli,
    DeepSeek,
    Xai,
    Zai,
    KimiCoding,
    KimiCodingCn,
    Stepfun,
    Minimax,
    MinimaxCn,
    Alibaba,
    OllamaCloud,
    Arcee,
    Kilocode,
    OpencodeZen,
    OpencodeGo,
    Bedrock,
    AzureFoundry,
    Custom,
}

impl ProviderKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "nous" => Some(Self::Nous),
            "openrouter" => Some(Self::OpenRouter),
            "ai-gateway" | "ai_gateway" | "vercel" => Some(Self::AiGateway),
            "anthropic" => Some(Self::Anthropic),
            "openai" => Some(Self::OpenAi),
            "openai-codex" | "codex" => Some(Self::OpenAiCodex),
            "xiaomi" => Some(Self::Xiaomi),
            "nvidia" => Some(Self::Nvidia),
            "qwen-oauth" | "qwen_oauth" => Some(Self::QwenOauth),
            "copilot" => Some(Self::Copilot),
            "copilot-acp" | "copilot_acp" => Some(Self::CopilotAcp),
            "huggingface" | "hf" => Some(Self::HuggingFace),
            "gemini" | "google" => Some(Self::Gemini),
            "google-gemini-cli" | "gemini-cli" => Some(Self::GoogleGeminiCli),
            "deepseek" | "deep-seek" => Some(Self::DeepSeek),
            "xai" => Some(Self::Xai),
            "zai" | "glm" | "z-ai" => Some(Self::Zai),
            "kimi-coding" | "kimi" => Some(Self::KimiCoding),
            "kimi-coding-cn" | "kimi-cn" => Some(Self::KimiCodingCn),
            "stepfun" => Some(Self::Stepfun),
            "minimax" => Some(Self::Minimax),
            "minimax-cn" => Some(Self::MinimaxCn),
            "alibaba" | "dashscope" => Some(Self::Alibaba),
            "ollama-cloud" => Some(Self::OllamaCloud),
            "arcee" => Some(Self::Arcee),
            "kilocode" => Some(Self::Kilocode),
            "opencode-zen" => Some(Self::OpencodeZen),
            "opencode-go" => Some(Self::OpencodeGo),
            "bedrock" => Some(Self::Bedrock),
            "azure-foundry" | "azure" => Some(Self::AzureFoundry),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nous => "nous",
            Self::OpenRouter => "openrouter",
            Self::AiGateway => "ai-gateway",
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::OpenAiCodex => "openai-codex",
            Self::Xiaomi => "xiaomi",
            Self::Nvidia => "nvidia",
            Self::QwenOauth => "qwen-oauth",
            Self::Copilot => "copilot",
            Self::CopilotAcp => "copilot-acp",
            Self::HuggingFace => "huggingface",
            Self::Gemini => "gemini",
            Self::GoogleGeminiCli => "google-gemini-cli",
            Self::DeepSeek => "deepseek",
            Self::Xai => "xai",
            Self::Zai => "zai",
            Self::KimiCoding => "kimi-coding",
            Self::KimiCodingCn => "kimi-coding-cn",
            Self::Stepfun => "stepfun",
            Self::Minimax => "minimax",
            Self::MinimaxCn => "minimax-cn",
            Self::Alibaba => "alibaba",
            Self::OllamaCloud => "ollama-cloud",
            Self::Arcee => "arcee",
            Self::Kilocode => "kilocode",
            Self::OpencodeZen => "opencode-zen",
            Self::OpencodeGo => "opencode-go",
            Self::Bedrock => "bedrock",
            Self::AzureFoundry => "azure-foundry",
            Self::Custom => "custom",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI",
            Self::OpenAiCodex => "OpenAI Codex",
            Self::AiGateway => "AI Gateway",
            Self::QwenOauth => "Qwen OAuth",
            Self::GoogleGeminiCli => "Google Gemini CLI",
            Self::KimiCoding => "Kimi Coding",
            Self::KimiCodingCn => "Kimi Coding CN",
            Self::OllamaCloud => "Ollama Cloud",
            Self::OpencodeZen => "OpenCode Zen",
            Self::OpencodeGo => "OpenCode Go",
            Self::AzureFoundry => "Azure Foundry",
            _ => self.as_str(),
        }
    }

    pub fn default_api_mode(self) -> ApiMode {
        match self {
            Self::Anthropic | Self::Minimax | Self::MinimaxCn => ApiMode::AnthropicMessages,
            Self::OpenAiCodex => ApiMode::CodexResponses,
            Self::Gemini => ApiMode::GeminiNative,
            Self::GoogleGeminiCli => ApiMode::GeminiCloudcode,
            Self::Bedrock => ApiMode::BedrockConverse,
            Self::CopilotAcp => ApiMode::CopilotAcp,
            _ => ApiMode::ChatCompletions,
        }
    }

    pub fn default_base_url(self) -> Option<&'static str> {
        match self {
            Self::Nous => Some("https://inference-api.nousresearch.com/v1"),
            Self::OpenRouter => Some("https://openrouter.ai/api/v1"),
            Self::AiGateway => Some("https://ai-gateway.vercel.sh/v1"),
            Self::Anthropic => Some("https://api.anthropic.com/v1"),
            Self::OpenAi => Some("https://api.openai.com/v1"),
            Self::OpenAiCodex => Some("https://chatgpt.com/backend-api/codex"),
            Self::Xiaomi => Some("https://api.xiaomimimo.com/v1"),
            Self::Nvidia => Some("https://integrate.api.nvidia.com/v1"),
            Self::QwenOauth => Some("https://portal.qwen.ai/v1"),
            Self::Copilot => Some("https://api.githubcopilot.com"),
            Self::CopilotAcp => Some("https://api.githubcopilot.com"),
            Self::Gemini => Some("https://generativelanguage.googleapis.com/v1beta"),
            Self::GoogleGeminiCli => Some("https://cloudcode-pa.googleapis.com"),
            Self::DeepSeek => Some("https://api.deepseek.com/v1"),
            Self::Xai => Some("https://api.x.ai/v1"),
            Self::Zai => Some("https://open.bigmodel.cn/api/paas/v4"),
            Self::KimiCoding => Some("https://api.moonshot.ai/v1"),
            Self::KimiCodingCn => Some("https://api.moonshot.cn/v1"),
            Self::Stepfun => Some("https://api.stepfun.ai/step_plan/v1"),
            Self::Minimax => Some("https://api.minimax.io/anthropic"),
            Self::MinimaxCn => Some("https://api.minimaxi.com/anthropic"),
            Self::Alibaba => Some("https://dashscope-intl.aliyuncs.com/compatible-mode/v1"),
            Self::OllamaCloud => Some("https://ollama.com/api"),
            Self::Arcee => Some("https://api.arcee.ai/api/v1"),
            Self::Kilocode => Some("https://api.kilo.ai/api/gateway"),
            Self::OpencodeZen => Some("https://opencode.ai/zen/v1"),
            Self::OpencodeGo => Some("https://opencode.ai/zen/go/v1"),
            Self::HuggingFace => Some("https://router.huggingface.co/v1"),
            Self::Bedrock => Some("https://bedrock-runtime.us-east-1.amazonaws.com"),
            Self::AzureFoundry | Self::Custom => None,
        }
    }

    pub fn api_key_env_keys(self) -> &'static [&'static str] {
        match self {
            Self::Nous => &["NOUS_API_KEY", "NOUS_AGENT_KEY"],
            Self::OpenRouter => &["OPENROUTER_API_KEY"],
            Self::AiGateway => &["AI_GATEWAY_API_KEY"],
            Self::Anthropic => &[
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_TOKEN",
                "CLAUDE_CODE_OAUTH_TOKEN",
            ],
            Self::OpenAi => &["OPENAI_API_KEY"],
            Self::OpenAiCodex => &["OPENAI_CODEX_TOKEN", "CODEX_TOKEN"],
            Self::Xiaomi => &["XIAOMI_API_KEY"],
            Self::Nvidia => &["NVIDIA_API_KEY", "NVIDIA_NIM_API_KEY"],
            Self::QwenOauth => &["QWEN_OAUTH_TOKEN"],
            Self::Copilot => &["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"],
            Self::CopilotAcp => &[],
            Self::HuggingFace => &["HF_TOKEN"],
            Self::Gemini => &["GOOGLE_API_KEY", "GEMINI_API_KEY"],
            Self::GoogleGeminiCli => &["GEMINI_CLI_OAUTH_TOKEN"],
            Self::DeepSeek => &["DEEPSEEK_API_KEY"],
            Self::Xai => &["XAI_API_KEY"],
            Self::Zai => &["GLM_API_KEY", "ZAI_API_KEY", "Z_AI_API_KEY"],
            Self::KimiCoding => &["KIMI_API_KEY", "KIMI_CODING_API_KEY"],
            Self::KimiCodingCn => &["KIMI_CN_API_KEY"],
            Self::Stepfun => &["STEPFUN_API_KEY"],
            Self::Minimax => &["MINIMAX_API_KEY"],
            Self::MinimaxCn => &["MINIMAX_CN_API_KEY"],
            Self::Alibaba => &["DASHSCOPE_API_KEY"],
            Self::OllamaCloud => &["OLLAMA_API_KEY"],
            Self::Arcee => &["ARCEEAI_API_KEY"],
            Self::Kilocode => &["KILOCODE_API_KEY"],
            Self::OpencodeZen => &["OPENCODE_ZEN_API_KEY"],
            Self::OpencodeGo => &["OPENCODE_GO_API_KEY"],
            Self::Bedrock => &[],
            Self::AzureFoundry => &["AZURE_FOUNDRY_API_KEY"],
            Self::Custom => &["CUSTOM_API_KEY", "OPENAI_API_KEY"],
        }
    }

    pub fn base_url_env_keys(self) -> &'static [&'static str] {
        match self {
            Self::Nous => &["NOUS_BASE_URL"],
            Self::OpenRouter => &["OPENROUTER_BASE_URL"],
            Self::AiGateway => &["AI_GATEWAY_BASE_URL"],
            Self::Anthropic => &["ANTHROPIC_BASE_URL"],
            Self::OpenAi => &["OPENAI_BASE_URL"],
            Self::OpenAiCodex => &["OPENAI_CODEX_BASE_URL", "CODEX_BASE_URL"],
            Self::Xiaomi => &["XIAOMI_BASE_URL"],
            Self::Nvidia => &["NVIDIA_BASE_URL", "NVIDIA_NIM_BASE_URL"],
            Self::QwenOauth => &["QWEN_BASE_URL"],
            Self::Copilot => &["COPILOT_API_BASE_URL"],
            Self::CopilotAcp => &["COPILOT_ACP_BASE_URL"],
            Self::HuggingFace => &["HF_BASE_URL"],
            Self::Gemini | Self::GoogleGeminiCli => &["GEMINI_BASE_URL"],
            Self::DeepSeek => &["DEEPSEEK_BASE_URL"],
            Self::Xai => &["XAI_BASE_URL"],
            Self::Zai => &["GLM_BASE_URL"],
            Self::KimiCoding => &["KIMI_BASE_URL"],
            Self::KimiCodingCn => &["KIMI_CN_BASE_URL"],
            Self::Stepfun => &["STEPFUN_BASE_URL"],
            Self::Minimax => &["MINIMAX_BASE_URL"],
            Self::MinimaxCn => &["MINIMAX_CN_BASE_URL"],
            Self::Alibaba => &["DASHSCOPE_BASE_URL"],
            Self::OllamaCloud => &["OLLAMA_BASE_URL"],
            Self::Arcee => &["ARCEE_BASE_URL"],
            Self::Kilocode => &["KILOCODE_BASE_URL"],
            Self::OpencodeZen => &["OPENCODE_ZEN_BASE_URL"],
            Self::OpencodeGo => &["OPENCODE_GO_BASE_URL"],
            Self::Bedrock => &["BEDROCK_BASE_URL"],
            Self::AzureFoundry => &["AZURE_FOUNDRY_BASE_URL"],
            Self::Custom => &["CUSTOM_BASE_URL", "OPENAI_BASE_URL"],
        }
    }

    pub fn model_env_keys(self) -> &'static [&'static str] {
        match self {
            Self::Nous => &["NOUS_MODEL", "MODEL"],
            Self::OpenRouter => &["OPENROUTER_MODEL", "MODEL"],
            Self::AiGateway => &["AI_GATEWAY_MODEL", "MODEL"],
            Self::Anthropic => &["ANTHROPIC_MODEL", "MODEL"],
            Self::OpenAi => &["OPENAI_MODEL", "MODEL"],
            Self::OpenAiCodex => &["OPENAI_CODEX_MODEL", "CODEX_MODEL", "MODEL"],
            Self::Xiaomi => &["XIAOMI_MODEL", "MODEL"],
            Self::Nvidia => &["NVIDIA_MODEL", "MODEL"],
            Self::QwenOauth => &["QWEN_MODEL", "MODEL"],
            Self::Copilot | Self::CopilotAcp => &["COPILOT_MODEL", "MODEL"],
            Self::HuggingFace => &["HF_MODEL", "MODEL"],
            Self::Gemini | Self::GoogleGeminiCli => &["GEMINI_MODEL", "MODEL"],
            Self::DeepSeek => &["DEEPSEEK_MODEL", "MODEL"],
            Self::Xai => &["XAI_MODEL", "MODEL"],
            Self::Zai => &["ZAI_MODEL", "GLM_MODEL", "MODEL"],
            Self::KimiCoding | Self::KimiCodingCn => &["KIMI_MODEL", "MODEL"],
            Self::Stepfun => &["STEPFUN_MODEL", "MODEL"],
            Self::Minimax | Self::MinimaxCn => &["MINIMAX_MODEL", "MODEL"],
            Self::Alibaba => &["DASHSCOPE_MODEL", "MODEL"],
            Self::OllamaCloud => &["OLLAMA_MODEL", "MODEL"],
            Self::Arcee => &["ARCEE_MODEL", "MODEL"],
            Self::Kilocode => &["KILOCODE_MODEL", "MODEL"],
            Self::OpencodeZen => &["OPENCODE_ZEN_MODEL", "MODEL"],
            Self::OpencodeGo => &["OPENCODE_GO_MODEL", "MODEL"],
            Self::Bedrock => &["BEDROCK_MODEL", "MODEL"],
            Self::AzureFoundry => &["AZURE_FOUNDRY_MODEL", "MODEL"],
            Self::Custom => &["CUSTOM_MODEL", "MODEL"],
        }
    }

    pub fn requires_secret(self) -> bool {
        !matches!(
            self,
            Self::Bedrock
                | Self::Nous
                | Self::CopilotAcp
                | Self::QwenOauth
                | Self::GoogleGeminiCli
                | Self::Custom
                | Self::AzureFoundry
        )
    }

    pub fn is_setup_wired(self) -> bool {
        !matches!(self, Self::Copilot)
    }
}
