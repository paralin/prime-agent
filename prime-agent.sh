#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
command=(cargo run --quiet --release --locked --manifest-path "$script_dir/Cargo.toml" -p pa-cli --bin prime-agent --)
no_env=false
parse_launcher_flags=true

for arg in "$@"; do
  if [[ "$parse_launcher_flags" == true && "$arg" == --no-env ]]; then
    no_env=true
  else
    command+=("$arg")
    if [[ "$arg" == -- ]]; then
      parse_launcher_flags=false
    fi
  fi
done

if [[ "$no_env" == true ]]; then
  # Match the native providers' environment credentials, including cloud SDKs.
  unset ANTHROPIC_API_KEY ANTHROPIC_OAUTH_TOKEN OPENAI_API_KEY PRIME_API_KEY
  unset DEEPSEEK_API_KEY GEMINI_API_KEY GOOGLE_CLOUD_API_KEY GROQ_API_KEY
  unset CEREBRAS_API_KEY XAI_API_KEY OPENROUTER_API_KEY ZAI_API_KEY MISTRAL_API_KEY
  unset MINIMAX_API_KEY MINIMAX_CN_API_KEY AI_GATEWAY_API_KEY OPENCODE_API_KEY
  unset MOONSHOT_API_KEY HF_TOKEN FIREWORKS_API_KEY KIMI_API_KEY
  unset RUNINFRA_GATEWAY_KEY MERGE_GATEWAY_API_KEY VENICE_API_KEY CLOUDFLARE_API_KEY
  unset XIAOMI_API_KEY XIAOMI_TOKEN_PLAN_CN_API_KEY
  unset XIAOMI_TOKEN_PLAN_AMS_API_KEY XIAOMI_TOKEN_PLAN_SGP_API_KEY
  unset COPILOT_GITHUB_TOKEN GH_TOKEN GITHUB_TOKEN
  unset GOOGLE_APPLICATION_CREDENTIALS GOOGLE_CLOUD_PROJECT GCLOUD_PROJECT
  unset GOOGLE_CLOUD_LOCATION AWS_PROFILE AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY
  unset AWS_SESSION_TOKEN AWS_REGION AWS_DEFAULT_REGION AWS_BEARER_TOKEN_BEDROCK
  unset AWS_CONTAINER_CREDENTIALS_RELATIVE_URI AWS_CONTAINER_CREDENTIALS_FULL_URI
  unset AWS_WEB_IDENTITY_TOKEN_FILE AZURE_OPENAI_API_KEY AZURE_OPENAI_BASE_URL
  unset AZURE_OPENAI_RESOURCE_NAME
  echo "Running Prime Agent without environment credentials..." >&2
fi

# Cargo rebuilds changed sources and runs the native binary in the caller's cwd.
exec "${command[@]}"
