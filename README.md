# alfred

**阻力最小路径（Least Resistance）AI 代理治理骨架。**

alfred 是一个最小可运行（working skeleton）的 AI 代理治理系统：属主提交需求 →
规划器拆解为 DAG → 计划审查判忠实度 → 沙箱容器内执行（pi）→ 执行审查判验收 →
分级路由（推进 / 机械重跑 / 升级属主）→ 属主拍板后续跑。所有状态转移可审计、
可续跑；执行者与审查者进程层面隔离，执行者容器断网、密钥不进容器。

本仓库是重写后的骨架实现（v0.1.0，全量回滚重写，不保留 v1 旧代码）。R1-R4
阶段交付完成，R5 收尾（AGT 评估 + 全链 e2e 汇总 + 越界写自动化 + 文档同步）。

---

## 目录

- [核心架构](#核心架构)
- [构建与测试](#构建与测试)
- [配置](#配置)
- [库 API 与 alfred bin](#库-api-与-alfred-binowner-交互走-alfred-chat)
- [治理环流程](#治理环流程)
- [端到端测试](#端到端测试)
- [安全边界](#安全边界)
- [目录结构](#目录结构)

---

## 核心架构

```
属主（人）
  └─ alfred 库（Rust；治理环驱动 alfred-cli::governance，owner 交互走 `alfred chat`
       持续会话（codux 调度的常驻 REPL）→ run_governance_loop / feed_owner_message；
       编排器状态机在 alfred-core 内，进程内调用）
       ├─ planner：容器内 pi 对话 agent（converse 建图；maintain 会话文档**待重做**——
       │    半实现已回退），
       │    经 sandbox_agent_bridge 桥调模型（容器断网 + 宿主代发）→ 产出 → DagSpec
       ├─ 执行：生成宿主侧容器驱动脚本（driver.py，非 eval Task）→ spawn `python3 driver.py`
       │    └─ Inspect 容器管理接口：起 docker 沙箱（network_mode: none + 只挂 workspace）
       │         ├─ sandbox_agent_bridge：容器内 localhost 模型代理 → 宿主 provider
       │         └─ 容器内 pi：只拿契约 prompt，经桥调模型（密钥不进容器）
       │    ← 轮询 `<work>/driver.done.json` done 记录 → 读 bind mount 产物
       ├─ 计划审查 / 执行审查：同机制独立 reviewer 容器（driver.py 驱动容器内 pi
       │    判 DagSpec vs OwnerRequest 忠实度 / 产物 vs 验收标准）→ verdict.json
       └─ 持久层：run-<id>/{state.json, audit.jsonl, llm-calls/, exec-N/}（exec-N/ 下
            driver.done.json + driver.stdout/stderr.log 为驱动证据，替代旧 evals/）
```

### Crate 划分（Cargo workspace，5 crates）

| crate | 职责 |
|---|---|
| `alfred-core` | 跨组件共享实体（唯一真源）：OwnerRequest / DagSpec / GraphBuilder / Contract / TaskAssignment / ExecVerdict / PlanVerdict / SessionDoc + **治理环状态机**（`governance.rs`，§3.3 路由表落码） |
| `alfred-planner` | 规划器（converse 建图 / 打回伪装 disguise；maintain 会话文档维护**待重做**——半实现已回退）；容器内 pi 对话 agent（桥代发 LLM），llm-calls/ 落盘；`ALFRED_OFFLINE=1` 离线确定性直通 |
| `alfred-reviewer` | 审查侧：计划/执行审查都在独立 reviewer 容器内完成（driver.py 容器 pi，判忠实度 PlanVerdict / 验收 ExecVerdict） |
| `alfred-cli` | 治理环库驱动（`governance::run_governance_loop` / `feed_owner_message` / `init_governance_run` / `build_governance_context`）+ 真实 `alfred` bin（**owner 持续会话入口 `chat`**：需求收集/对话路由/拍板/断点恢复的确定性 REPL，codux 调度的常驻会话进程）+ codux 可调度 CLI driver：run/feed/status（脚本/e2e 技术接口，消费前置 `--append-system-prompt`，注入的项目上下文追加到 planner pi 系统提示） |

---

## 构建与测试

```bash
cargo build --workspace      # 0 error
cargo test  --workspace      # 97 passed; 0 failed（R4 实测）
```

依赖：Rust（edition 2021）+ Docker（沙箱镜像 `alfred-executor:latest`，构建见
`docker/Dockerfile`）+ Inspect AI（`inspect` CLI，见 e2e 脚本定位逻辑）+ pi-coding-agent。

---

## 配置

模型配置唯一真源：`${XDG_CONFIG_HOME:-~/.config}/alfred/config.yml`（权限建议 0600）。
三层结构：`providers`（endpoint + 凭证 + protocol）→ `models`（id + 挂哪个 provider + 参数）
→ `roles`（只点名模型 id：planner / executor / reviewer）。

env 覆盖：

| env | 语义 |
|---|---|
| `LLM_<ROLE>_MODEL` / `ALFRED_<ROLE>_MODEL` | 角色模型 id（planner/executor/reviewer） |
| `LLM_BASE_URL` / `LLM_API_KEY` | 指定 provider 的 endpoint / key |
| `ALFRED_CONFIG` | config.yml 路径覆盖 |
| `ALFRED_INSPECT` | inspect CLI 路径覆盖 |
| `ALFRED_IMAGE` | 沙箱镜像覆盖（缺省 `alfred-executor:latest`） |
| `ALFRED_OFFLINE=1` + `ALFRED_OFFLINE_PLAN_FILE=<dag.json>` | 规划器离线确定性直通（e2e 用） |
| `ALFRED_AGT_DIR=<dir>` | AGT 策略目录覆盖（含 `agt-policy.ts` + `policy.json`；未设 = 内置默认策略 `docker/agt/<role>/`） |
| `ALFRED_AGT_DISABLE=1` | 显式关闭 AGT 拦写层（opt-out，压过 `ALFRED_AGT_DIR`） |

> 注意：config.yml 含真实 API key，已在 `.gitignore` 面（`~/.config/` 不在仓库内）。
> 密钥只留在宿主进程；容器内 models.json 用哑 key `sk-none` 指向桥。

---

## 库 API 与 alfred bin（owner 交互走 alfred chat）

alfred 出库 API（编排器状态机驱动）+ 真实 `alfred` bin（照 omp.rs 范式，不发明
面板）：

- `governance::run_governance_loop(&mut GovernanceRun, &GovernanceContext)`：
  初始化 / 从当前状态推进治理环，直到挂起态（PlanRejected / Escalated）或终态
  （Completed / Abandoned）。每次状态进入打印 `[orchestrator]` 流转状态行
  （owner 可见的协调者路由行为）。
- `governance::feed_owner_message(&mut run, &ctx, message, decision)`：
  owner 决策入口：设属主消息（revise 重规划 / Planning 态续入对话）、落
  conversation.json，按挂起态路由续跑，返回新状态 + 规划器答复
  给调用方显示。`decision` ∈ retry | revise | abandon（retry/abandon 消息可选）。
- `governance::init_governance_run(run_dir, request, options)` /
  `governance::build_governance_context(run_dir)`：run 目录初始化与驱动上下文
  组装（`run` 子命令与 `chat` 需求收集共用的单一初始化真源）。

### alfred chat（owner 持续会话入口）

codux 终端调度的**常驻 REPL 进程**（读 stdin 行 → 打印 → 循环，Ctrl-D/EOF 退出）：

```bash
alfred chat [--run-dir <dir>]
```

确定性循环壳（不发明治理机制；LLM 只在被治理容器里），按当前 run 状态路由：

- **入口定位**：显式 `--run-dir` 优先；否则默认 state 基目录（`$ALFRED_STATE_DIR`
  或 `~/.local/state/alfred/runs`）取 updated_at 最新 run 按态呈现；多挂起 run
  并存时列出清单要求 `--run-dir` 消歧；REPL 横幅常显 run_dir。
- **需求收集态**（无 run / 终态后新需求）：第一行=需求 → `[orchestrator]` 确定性
  转写（title=首行 ≤40 字、id=`chat-<ts>`）→ 追问验收标准（"按需求"=用需求原文）
  → 建 run → 提交 planner（与 `run` 同一初始化真源）。
- **Planning 态**（pi 答复后停驻）：行=owner 消息 → feed Revise 续入对话 →
  `[pi]` 答复（答复与建图计划摘要都从 conversation.json ConverseReply 语义轮
  呈现）；整行精确 "放弃" → Abandon（属主放弃恒可选）。
- **挂起态**（escalated/plan_rejected）：升级包呈现（产物摘要 + 审查意见，按态取
  verdict 历史 / audit 升级事件，owner 全可见）→ 整行精确匹配："重试"→Retry、
  "放弃"→Abandon、**其余一律 Revise + 整行作属主消息**（最安全分支：进 planner
  它会追问澄清）。
- **终态**：呈现结果 + "新需求请直接说"（回需求收集态）。
- **断点恢复**：run 从 state.json 恢复（每转移 persist）；流转中间态无需属主输入
  直接续跑；feed/loop 出错 catch + reload 续会话不退进程。

真实 `alfred` bin 的技术 driver 子命令（脚本/e2e 接口，保留不动）驱动治理环：


```bash
# 运行治理环（request → 规划 → 计划审查 → 执行 → 执行审查 → 分级路由 → 挂起/完成）
cargo run --quiet -p alfred-cli --bin alfred -- run \
  --request <req.json> [--run-dir <dir>] [--time-limit 600] [--review-time-limit 300] \
  [--image alfred-executor:latest]

# 喂属主决策（retry / revise / abandon）并从挂起态续跑
cargo run --quiet -p alfred-cli --bin alfred -- feed \
  --run-dir <dir> --decision retry|revise|abandon [--message <文本|文件>]

# 只读查看治理环状态
cargo run --quiet -p alfred-cli --bin alfred -- status --run-dir <dir>
```

`run` / `feed` 的续跑模型：state.json 存状态机（`GovernanceRun`），`feed` 是 signal
不是终点——`Escalated + retry` 重入执行循环、`PlanRejected + retry` 以伪装消息
重规划、`revise` 以属主新需求重规划、任一 + `abandon` 终止。

---

## 治理环流程

```
Planning → PlanReviewing → Executing → ExecReviewing
   出口：Completed（验收 C）/ Escalated（升级属主，挂起）/ PlanRejected（计划打回，挂起）/ Abandoned（终态）
```

分级路由（§3.3 六行，全部落码）：

| ExecVerdict | failure_class | 去向 |
|---|---|---|
| C | — | 推进 |
| I/P | mechanical | 同契约重跑（预算 N=2，耗尽升级） |
| I/P | contract_ambiguity / fidelity_dispute / disagreement | 升级属主 |
| I/P | contract_fault | 升级属主（预标注建议改契约） |

- **机械失败判定**：执行容器驱动状态（success/error/timed_out/crash）——非 success → mechanical。
- **审查本身出错**（unscored / driver error）→ 升级属主，不悄悄放行。
- **打回伪装（P7）**：计划审查打回的 reason 被转写为属主口吻消息（禁词检查：
  reject/verdict/审查/打回 等结构化信号不得出现），再喂给规划器重规划。
- **会话文档（P6，维护者待重做）**：原 maintain 在 ① 计划审查结论落定、
  ② 属主补充新需求 两时机更新 SessionDoc 三段（key_file_paths / key_conclusions /
  review_summary）——**该半实现已回退**（新架构下重做：宿主 pi + 定期 + 真实数据源）；
  SessionDoc schema 保留，喂给规划器时投影中性化（review_summary → owner_feedback，
  禁词净化），当前恒为空文档。

---

## 端到端测试

`tests/e2e/` 全部真跑（LLM 经 config.yml；离线用例用 `ALFRED_OFFLINE` 确定性直通）：

| 脚本 | 覆盖 | 模式 |
|---|---|---|
| `r1.sh` | 执行侧：容器内 pi 产出 hello.txt 落宿主 + driver.done.json/stdout/stderr 证据归档 | 真容器真 LLM |
| `r2.sh` | 审查侧两用例：执行审查 C / 部分兑现 P（独立 plan-review 两用例已归档，等价覆盖见 r3 case3 / r6b caseA） | 真 LLM + 离线注入 |
| `r3.sh` | 治理环闭环：正路径全环 / 机械升级闭环（case2b 归档）/ 打回伪装闭环（feed retry）/ 多轮会话文档（feed revise） | 真 LLM + 离线注入 |
| `r4.sh` | 属主决策 feed 续跑两用例：escalated→feed abandon→Abandoned / plan_rejected→feed retry→重规划→Escalated（决策面板 RPC 已删归档） | 离线注入 |
| `chat.sh` | owner 持续会话（`alfred chat`）黑盒：需求收集+确定性转写+建 run+[pi] 答复 / 建图→[orchestrator] 流转→升级包→拍板（重试/修改/放弃 + "不要重试/别放弃/算了/两词同现"精确匹配边界）/ Planning 态放弃出口 / PlanRejected 打回呈现+伪装重试 / 终态→新需求回收集态 / run 发现与多挂起消歧；`CHAT_REAL=1` 附加真容器真 LLM REPL 对话 | 离线注入（+ CHAT_REAL=1 真 LLM） |
| `agt/agt-policy.test.mjs` | AGT 策略求值确定性测试（119 断言；planner 段含不可知隔离负向断言：真实 run 目录派生前缀命中 + outputs 白名单穿越封堵 + bash 写族 deny + 拒绝反馈中性 + 第四轮对抗探针：组合遮蔽/../覆写/无空格·fd 重定向/~/\$HOME/相对/关键词形态/block reason 无规则名 + 优先级防漂移真断言 + review-outputs 读/重定向写对抗探针：carve 收窄 planner/outputs 子树后 plan-review/exec-review outputs 的 grep -r/head/cat 通配/ls/find/less/wc/stat/file/引号形态逐条 deny） | 无 LLM 无容器 |
| `agt/demo.sh` | AGT 实机演示：沙箱容器内 pi + 策略扩展拦截 `rm -rf`（审计 deny+allow） | 真容器真 LLM（可选演示） |
| `agt-default.sh` | AGT 默认启用黑盒：二进制落盘内置策略（byte 级 == `docker/agt/`）+ compose 挂载 + driver 注入 + 容器内审计 allow；`ALFRED_AGT_DISABLE=1` 不挂不加载；`AGT_DEFAULT_REAL=1` 附加真容器全链 Completed + 越界写对抗探针（审计 deny） | Tier 1 确定性（mock 驱动）/ Tier 2 真 LLM |

**统一入口**：

```bash
bash tests/e2e/skeleton.sh   # r1 → r2 → r3 → r4 → chat → escape → agt → agt-default，全绿才算过
```

skeleton.sh 头部注释如实说明两种模式：真容器真 LLM（r1 / r2 case1·1b / r3 case1 /
escape）覆盖"真实执行与审查"；离线注入（r3 case2·3·4 / r4 case1·2 / chat case1-4，
`ALFRED_OFFLINE=1` + `ALFRED_OFFLINE_PLAN_FILE`）覆盖"确定性状态机路径"（机械升级、
伪装打回、feed 属主决策、chat owner 会话路由），绕过 planner LLM 保证确定性；
agt 为无 LLM 确定性原型测试。

---

## 安全边界

| 边界 | 机制 | 验证 |
|---|---|---|
| 联网默认拒绝 | 沙箱 compose `network_mode: none`（容器内只有 lo） | R0 实验 + r1 沿用 |
| 密钥不进容器 | 容器内 models.json 哑 key；真实 key 只留宿主 driver 进程（env_clear + 白名单注入） | R0 审计（docker inspect env 零命中） |
| 越界写拦截 | 只挂 workspace 卷；工作区外路径在容器 overlay，不落宿主 | `tests/e2e/escape.sh`（两向验证 PASS） |
| 审查隔离 | 非声明性由挂载面保证：契约全本/验收标准/对话记录不挂给执行者容器；reviewer 容器独立挂 ws 全量 ro + 对话记录判分；规划器不感知审查者/执行者 | r2/r3 e2e 断言 |
| 工具级策略（AGT 拦写层，**默认启用**） | AGT 风格 pi 扩展拦 `tool_call`（rm -rf / sudo / 秘密读取 / 越界写）：三容器默认挂载（策略落盘 → compose 挂 `/tmp/.agt` ro + 审计子目录 rw，driver env 注入 `-e` 扩展）。内置默认策略 `docker/agt/{executor,planner,reviewer}/policy.json` 编译期内嵌随二进制分发；`ALFRED_AGT_DIR` 显式目录覆盖，`ALFRED_AGT_DISABLE=1` 显式关闭 | `tests/e2e/agt/`（确定性求值 + 实机演示 + `exec-demo.sh` 越界写被拒 + 审计 deny）、`tests/e2e/agt-default.sh`（默认启用/显式关闭黑盒：无 env 落盘内置策略 + 容器内审计 allow；DISABLE=1 无 staging 无挂载无注入；真容器探针越界写被拒） |

---

## 目录结构

```
crates/
  alfred-core/      实体 + 状态机 + 路由 + GraphBuilder
  alfred-planner/   converse / disguise / container / task_gen / llm（maintain 已回退待重做）
  alfred-executor/  task_gen / compose_gen / driver / artifact / run / config + templates/executor_driver.py.tmpl
  alfred-reviewer/  plan_review / exec_review / container / task_gen / verdict
  alfred-cli/       src/{main.rs (alfred bin: chat/run/feed/status), chat.rs, governance.rs, lib.rs}
docker/
  Dockerfile        沙箱镜像（inspect 基座 + Node 22 + pi-coding-agent 0.84.3）
  agt/              AGT 内置默认策略资产（三角色 policy.json + 共享 agt-policy.ts，编译期内嵌）
  pi-sandbox.compose.yaml  零挂载参考基座（network none）
tests/
  e2e/              r1-r4 / chat / escape / skeleton / agt/
.plans/             实施计划 + 各阶段交付/验证报告 + AGT评估.md（gitignore 面，不进版本历史）
```

---

## 文档指针

- `.plans/实施计划.md`：R0-R5 阶段计划与钉死项对齐表（重写纪律、环境事实 E1-E5）
- `.plans/R{1..4}交付.md` / `R{0..4}报告.md`：各阶段交付与独立验证记录
- `.plans/AGT评估.md`：AGT（微软 Agent Governance Toolkit）作为 pi 工具权限插件的
  评估 + 原型 + 拍板项（R5/P10）
- 治理文档（治理架构 / 业务架构 / 技术架构 / 限界上下文 / SKELETON 施工清单）在
  仓库外本地磁盘（`the-path-of-least-resistance/alfred-research/docs`），不进版本历史

> 阶段状态：R1（执行侧）→ R2（审查侧）→ R3（治理环闭环）→ R4（属主决策会话 + codux 界面集成）→ R5（AGT 评估 + 全链 e2e + 越界写 + 文档同步）。当前实现状态以
> `.plans/R5交付.md` 为准（验收文档不追认代码）。
