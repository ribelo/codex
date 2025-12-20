#[derive(Debug, Clone)]
pub(crate) struct StatusAccountDisplay {
    /// ChatGPT OAuth login info (if logged in)
    pub chatgpt: Option<ChatGptAccountInfo>,
    /// Gemini OAuth accounts (if any)
    pub gemini_accounts: Vec<String>,
    /// Antigravity OAuth accounts (if any)
    pub antigravity_accounts: Vec<String>,
    /// Whether GEMINI_API_KEY env var is set
    pub gemini_api_key_set: bool,
    /// Whether OPENAI_API_KEY env var is set (via auth.json or env)
    pub openai_api_key_set: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ChatGptAccountInfo {
    pub email: Option<String>,
    pub plan: Option<String>,
}
