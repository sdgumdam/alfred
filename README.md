# alfred

Workboss 治理架构的最小可用骨架（working skeleton）：属主提需求 → 规划器拆成执行计划 → 审查者审计划 → 执行者在 Docker 容器里跑 pi → 审查者审产物 → 失败分级路由 → 升级到属主拍板。

[English](README_EN.md)

## 前置

| 依赖 | 版本 | 检查 |
|---|---|---|
| Rust | 1.97+ | `cargo --version` |
| Docker | 29+ | `docker info` |
| pi (TS) | 0.80+ | `pi --version` |

## 安装

```bash
git clone https://github.com/sdgumdam/alfred.git
cd alfred
cargo build
```

## 配置

配置文件：`~/.config/alfred/config.yml`（YAML 三层结构）

```yaml
providers:
  kuaizi:
    base_url: "https://your-gateway/v1"
    api_key: "your-key"
    protocol: openai-compatible

models:
  - id: qwen3.8-max
    provider: kuaizi
    contextWindow: 131072
    maxTokens: 8192
  - id: glm-5.2
    provider: kuaizi
    contextWindow: 131072
    maxTokens: 16384
    thinking: high

roles:
  planner: qwen3.8-max
  executor: qwen3.8-max
  reviewer: glm-5.2
```

**要求**：审查者（reviewer）和执行者（executor）必须用不同 provider（异构审查）。

## 运行

### 提交需求

```bash
./target/debug/alfred run my-request.json
```

`my-request.json` 格式：

```json
{
  "request_id": "req-1",
  "requirement": "在 /tmp/demo 下创建 hello.txt，内容为 Hello Alfred。",
  "acceptance_criteria": "文件 /tmp/demo/hello.txt 存在且内容为 Hello Alfred"
}
```

### 查看状态

```bash
./target/debug/alfred status run-req-1
```

### 升级时拍板

升级时（需求不满足、审查不通过、重试预算耗尽），alfred 自动起 pi TUI 会话，浮出三按钮卡片（重跑/改契约/放弃），键盘选择即可。

或者手动：

```bash
./target/debug/alfred decide run-req-1 retry          # 重跑
./target/debug/alfred decide run-req-1 revise-contract # 改契约
./target/debug/alfred decide run-req-1 abandon         # 放弃
```

跳过 pi 卡片（CI 用）：

```bash
ALFRED_NO_ASK_PANEL=1 ./target/debug/alfred run my-request.json
```

## 测试

```bash
# 单元测试（126 个）
cargo test

# S0 黑盒 e2e（17 条，离线模式，不依赖 LLM）
ALFRED_OFFLINE=1 bash tests/e2e/s0.sh

# S3 端到端 e2e（46 条，离线模式，覆盖四类用例）
bash tests/e2e/skeleton.sh
```

## 验证

```bash
# 静态层：LSP（rust-analyzer）零诊断
# 编译层：cargo build 0 error 0 warning
# 行为层：126 单测 + s0 17 + skeleton 46 全绿
```

## 架构

| 角色 | 实现 | 说明 |
|---|---|---|
| 属主 | 人 | 在终端前拍板 |
| 规划器 | alfred-planner | Rust crate，builder API 模式拆需求 |
| 编排器 | alfred-core | 确定性状态机，无 LLM |
| 执行者 | Docker 容器里的 pi | SandboxProfile 四维约束（挂卷/网络/provider/运行时） |
| 审查者 | alfred-reviewer | 异构模型审查 |
| 持久层 | run-<id>/ | state.json + verdicts.jsonl + audit.jsonl |

## 文档

知识库（不进仓库）：`the-path-of-least-resistance/alfred-research/docs/`

- `SKELETON施工清单.md` — 唯一权威施工依据
- `治理架构.md` — 原理总纲（LeastR 论文依据）
- `业务架构.md` — 角色与权限
- `技术架构.md` — 技术实现
- `限界上下文.md` — schema 单源

## 仓库

- `sdgumdam/alfred`（公开）：代码
- `alfred-research`（本地物理隔离）：文档、papers
