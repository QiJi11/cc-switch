/**
 * Codex 配置模板
 * 用于新建自定义供应商时的默认配置
 */

export interface CodexTemplate {
  auth: Record<string, any>;
  config: string;
}

export const CODEX_MODEL_PROVIDER_ID = "第三方 5.6 Fast";

/**
 * 获取 Codex 自定义模板
 * @returns Codex 模板配置
 */
export function getCodexCustomTemplate(): CodexTemplate {
  const providerId = JSON.stringify(CODEX_MODEL_PROVIDER_ID);
  const config = `model_provider = ${providerId}
model = "gpt-5.5"
model_reasoning_effort = "high"
disable_response_storage = true

[model_providers.${providerId}]
name = "custom"
wire_api = "responses"
requires_openai_auth = true`;

  return {
    auth: { OPENAI_API_KEY: "" },
    config,
  };
}
