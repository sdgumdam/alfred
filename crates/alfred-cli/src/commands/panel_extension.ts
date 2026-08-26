// alfred panel pi 扩展（R4/P4 决策面板 RPC）。
//
// 由 `alfred panel` 生成到 run_dir/panel-extension.ts（provider 凭证/模型从
// ~/.config/alfred/config.yml 唯一真源嵌入，不写死在这里）。职责：
//   1. 注册 provider（zhipucoding 等）→ pi 可解析 `--model <id> --provider <name>`；
//   2. 注册 alfred_panel_decision 工具 → 调 ctx.ui.select 发三选项决策卡
//      （RPC 侧翻译为 extension_ui_request method=select），把属主选择的
//      选项原样返回（OWNER_DECISION:<value>，结构化、非文本关键词猜测）。
//
// 占位符（panel.rs 生成时替换）：
//   __PROVIDER_JSON__  __BASE_URL_JSON__  __API_KEY_JSON__
//   __MODEL_JSON__     __MAX_TOKENS__
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
  pi.registerProvider(__PROVIDER_JSON__, {
    baseUrl: __BASE_URL_JSON__,
    apiKey: __API_KEY_JSON__,
    api: "openai-completions",
    models: [
      {
        id: __MODEL_JSON__,
        name: __MODEL_JSON__,
        reasoning: false,
        input: ["text"],
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
        contextWindow: 131072,
        maxTokens: __MAX_TOKENS__,
        compat: { supportsDeveloperRole: false },
      },
    ],
  });

  pi.registerTool({
    name: "alfred_panel_decision",
    label: "Alfred Panel Decision",
    description:
      "Present the governance escalation decision card to the owner. " +
      "Call this with a title and the decision options. " +
      "Returns the exact option string the owner selected.",
    parameters: {
      type: "object",
      properties: {
        title: { type: "string", description: "Card title" },
        options: {
          type: "array",
          items: { type: "string" },
          description: "Decision choices shown to the owner",
        },
      },
      required: ["title", "options"],
    },
    async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
      const chosen = await ctx.ui.select(params.title, params.options);
      return {
        content: [{ type: "text", text: `OWNER_DECISION:${chosen}` }],
        details: {},
      };
    },
  });
}
