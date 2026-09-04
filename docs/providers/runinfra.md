# RunInfra

Set `RUNINFRA_GATEWAY_KEY` to use the built-in `runinfra` provider:

```sh
export RUNINFRA_GATEWAY_KEY="your-key"
prime-agent --provider runinfra --model deepseek-v4-flash
```

The default model is `deepseek-v4-flash`. The provider uses streaming Chat Completions at `https://api.runinfra.ai/v1` with bearer authentication.

You can also store an API key under `runinfra` in `auth.json`:

```json
{"runinfra":{"type":"api_key","key":"your-key"}}
```
