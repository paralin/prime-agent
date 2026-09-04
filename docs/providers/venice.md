# Venice

Set `VENICE_API_KEY` or use `/login` to configure the `venice` provider.

```sh
export VENICE_API_KEY="your-key"
prime-agent --provider venice --model stealth-ox-alpha
```

The default is `stealth-ox-alpha`. Venice uses streaming Chat Completions at `https://api.venice.ai/api/v1`. Automatic default selection prefers other configured providers before Venice.
