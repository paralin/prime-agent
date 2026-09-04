# Runtime settings

Global settings live in the agent directory; project settings live in `.prime/agent`. Use one of `settings.json`, `settings.yml`, or `settings.yaml` in each directory. Multiple files in one scope are rejected. Saves preserve the selected format and unrelated external edits. Project values override global values; runtime overrides take precedence and survive reloads.

```yaml
modelRoles:
  task:
    - openrouter/deepseek/deepseek-v4-flash-0731:max
    - runinfra/deepseek-v4-flash:high
  review: claude-code/claude-opus-4-7:high
claudeCode:
  executable: /usr/local/bin/claude
openRouter:
  responses: true
rlmActMaxDepth: 1
rlmActDefaultModel: '@task'
rlmAllowedServiceTiers: [default, priority]
compaction:
  native: true
  strategy: native-or-scratch
  triggerContextTokens: 256000
scratchHandoff:
  enabled: true
  rootDir: agent
```

Role candidates retain their order. Each role must contain either native provider-qualified selectors or Claude Code selectors. Thinking suffixes are optional. An array in `rlmActDefaultModel` selects one entry per Act depth, starting at depth one; a scalar applies only at depth one.

`codexHomes` is a global-only ordered list of Codex CLI directories because credential rotation is shared by the daemon. Project attempts to override it produce a settings warning.

Rust callers that retain settings across turns can use `WatchedSettingsManager` for external edits and atomic replacements. Its `with` and `with_mut` methods provide synchronized access; `dispose` and dropping it stop reloads. Callers that create a settings manager per request read the current document directly.
