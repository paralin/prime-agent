# Merge Gateway

Set `MERGE_GATEWAY_API_KEY` or use `/login` to configure `merge-gateway`.

```sh
export MERGE_GATEWAY_API_KEY="your-key"
prime-agent --provider merge-gateway --model anthropic/claude-sonnet-4-6
```

The default is `anthropic/claude-sonnet-4-6`. The provider uses `https://api-gateway.merge.dev/v1/ai-sdk`, preserves signed thinking in Chat Completions, and sends session affinity through `x-session-affinity` and `X-Session-Id`.

Authenticated catalog refreshes discover additional models from `/v1/models`, including paginated vendor routes. Discovery keeps tool-capable text routes and advertises only their shared image and reasoning capabilities. Failed refreshes retain bootstrap models and the last successful catalog for the current credential.
