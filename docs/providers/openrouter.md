# OpenRouter

Set `OPENROUTER_API_KEY` to use OpenRouter. Chat Completions is the default transport. Set `openRouter.responses: true` in global or project settings to prefer stateless Responses requests.

Responses requests preserve session and cache identifiers, routing preferences, tool definitions, and reasoning effort, with `store: false`. An unavailable Responses endpoint can fall back once to Chat before streaming starts. Authentication, context overflow, rate limits, cancellation, and errors after streaming starts do not trigger fallback.

Both transports send Prime Agent attribution headers. Explicit caller headers override attribution defaults.
