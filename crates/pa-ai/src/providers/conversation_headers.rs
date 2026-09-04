use crate::types::{Model, StreamOptions};

pub(crate) fn apply(headers: &mut Vec<(String, String)>, model: &Model, options: &StreamOptions) {
    let mut additions = Vec::new();
    if model.provider == "openrouter" {
        additions.extend([
            (
                "HTTP-Referer",
                "https://github.com/PrimeIntellect-ai/prime-agent",
            ),
            ("X-OpenRouter-Title", "Prime Agent"),
            ("X-OpenRouter-Categories", "cli-agent"),
        ]);
    }
    if matches!(model.provider.as_str(), "opencode" | "opencode-go") {
        if let Some(id) = options
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            additions.push(("x-opencode-session", id));
        }
    }
    for (name, value) in additions {
        if options
            .headers
            .as_ref()
            .is_some_and(|headers| headers.keys().any(|key| key.eq_ignore_ascii_case(name)))
        {
            continue;
        }
        headers.retain(|(key, _)| !key.eq_ignore_ascii_case(name));
        headers.push((name.to_string(), value.to_string()));
    }
}
