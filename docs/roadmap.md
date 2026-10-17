# kazi 产品方向与路线图

> 状态:0.19 – 0.23 已发布(2026-10-17);可扩展工作台里还剩技能 / hook / profile / 库 API。
> 这份文档只回答一个问题:**每个版本还缺什么,先做哪个,为什么先做它**。
> 原则(§3)、0.19 的六个支柱(§5)与「明确不做」(§7)是本文的可执行部分;
> 版本口径遵循 [VERSIONING.md](VERSIONING.md)(pre-1.0:MINOR = 功能批)。
>
> **0.19 实际交付**(对照 §5):P0 三项(§5.1 权限单元 + 审批身份、§2.8 重定向防护)、
> 审阅模式(§5.2)、档位(§5.3)、成本/缓存(§5.4)、上下文(/pin + 三段 + 摘要 schema,
> §5.5)、工具面(`delete_file` / `move_file` / PDF 文本,§5.6)、会话检索(§5.7)。
> **0.20 实际交付**:§5.6 的图片附件(`@x.png` → user 消息的 image part,两条 wire 各自编码,
> `[providers.*] vision = false` 在请求前拒绝)。
> **0.21 实际交付**:§6 的背景作业(`run_command background=true`、`job_output`/`job_kill`、
> `/jobs`、页脚计数、Windows 进程树 kill)。
> **0.22 实际交付**:§6 的 MCP 补全(`mcp_resources` / `mcp_prompts` 两个只读工具、
> 两条 transport 都实现 `resources/*` 与 `prompts/*`、`/mcp` 概览)。
> §6 的其余项(`fetch_url` 直接返回附件、全文索引 + 正则 `/find`)仍未开工。
> **0.23 实际交付**:§6(0.21 主题)的**项目约定**(§5 里 `KAZI.md`/`AGENTS.md` 的发现、
> 预算、来源标注与 `/instructions`)与**自定义命令**(`.kazi/commands/*.md`、
> front-matter、`$ARGUMENTS`/`$1..$9`、进补全与 `/help`、`kazi run` 同步展开)。
> **0.24 实际交付**:0.21 主题的**技能**(§6：目录只放 name + description,正文由
> `use_skill` 按需加载,/skills 查看,32 KiB 上限、近者优先)。
> **0.25 实际交付**:0.21 主题的 **hook**(`[[hooks]]` 的 pre_tool 拒绝 / post_tool /
> post_task 报告,stdin JSON + 退出码,超时算失败,/hooks 查看)。
> **0.26 实际交付**:0.21 主题的 **profile**(`--profile` / `KAZI_PROFILE`,叠一层
> `profiles/<name>.toml`)与 **`kazi::prelude` 冻结**(公开 Agent / Provider / Tool /
> Approver / Reviewer / 消息类型 / 配置 + 技能与 hook 类型)。
> 0.21 主题至此全部落地。
> **0.26 之后的账面**:0.19 的六个支柱、0.20 的外部接口、0.21 的可扩展工作台全部发布
> (0.19.0 → 0.26.0,共 10 个版本)。仅剩 §6 的两条尾巴**有意不做**,理由见 §9。

## 1. 现状:0.18 的 kazi 是什么

一句话:**一个能干活、看得清、但不完全听你的终端 agent**。

| 维度 | 现在有的 | 成熟度 |
|------|----------|--------|
| 引擎 | 单循环 + 工具并行(默认 4)+ 失败回传自纠 | 稳 |
| 协议 | OpenAI 兼容 / Anthropic / Ollama + SSE 自研解码 | 稳 |
| 工具 | 读/写/补丁/搜索/glob/命令/抓网页 + MCP + delegate + todo | 够用,边界清楚 |
| TUI | markdown、语法高亮、工具卡、diff、浮层、鼠标、主题 | 同行前列 |
| 上下文 | 4 字符≈1 token 估算 + 整轮裁剪 + /compact 摘要 | 可用,不诚实 |
| 会话 | 单文件 JSON,无索引,无全文检索 | 25 个以内还行 |
| 控制权 | 三档权限(只读/编辑/yolo)+ 审批 + 检查点 /undo | **有漏洞(§2)** |
| 接入 | MCP(stdio + HTTP)、本地 auth store、no_auth 网关 | 够 |
| 扩展 | **无**:没有项目约定文件、没有自定义命令、没有技能、没有 hook | **空白** |

## 2. 证据:最近的 bug 和空白点在哪

下列每一条都在代码里核对过,不是猜测。

### 2.1 P0 — 子代理会把父会话切成只读(已复现)

`delegate` 构造子代理时执行 `let mut ctx = self.base_ctx.clone()` 然后
`ctx.permission.set(ReadOnly)`(src/agent/mod.rs:245-251)。但
`ToolCtx.permission` 是一个 `Arc<AtomicU8>` 的句柄(src/tools/mod.rs:142-172),
clone 只复制句柄、共享同一个原子变量 —— 子代理一旦跑了只读子任务,
**父会话的权限模式也被改成只读**,而且 UI 上的 `screen.mode` 不会更新,
用户在头部看到的仍是 edit,下一次命令却整批被拒。

我用一个临时集成测试验证过(测试已删除):

```text
cloned_tool_ctx_shares_the_permission_cell ... FAILED
  left: ReadOnly   right: Edit
```

### 2.2 P0 — 审批没有身份:会被覆盖,也会被无声吞掉

TUI 只建了一个 `ask_tx/ask_rx`(src/agent/tui.rs:1966),`TuiApprover` 拿到的是它的
clone(:1972),/provider、/model 在后台重建 agent 时又用同一个 `ask_tx_rebuild`
(:2904)。事件循环收到任何一条 ask,就无条件塞进唯一的 `pending_approval`
(:3179-3188)。而弹窗只有一个槽位 `pending_approval: Option<..>`(:723)。
于是在「任务 A 正在等审批 + 后台切换 provider 时新 agent 又发一条 + 子代理再发一条」的
组合下:第二条 ask 直接覆盖第一条,第一条的 oneshot 被丢弃 → `decide()` 收到 Err
→ 静默 Deny(fail-closed,不会误批),但**屏幕上没有任何信息说明这条审批属于哪个任务、
哪个 agent**。用户看到的可能是子代理的命令,却以为在批准主任务;或者一条本该弹出的
审批被无声吞掉,模型只收到「被拒绝」而用户从未做过决定。两者都是安全模型里不该有的
模糊。

### 2.3 P1 — 「编辑模式」其实没有任何写入前确认

只有 `run_command` 有 `approval_summary`(src/tools/shell.rs:87-106)。
`write_file` / `apply_patch` 在 edit 模式下**直接落盘**,只有事后 /undo 兜底。
对「让它改 30 个文件」的场景,用户没有一次性看全、逐块取舍的机会。

### 2.4 P1 — 上下文是「砍+摘要」,但不告诉你砍了什么

估算值是「字符数/4」(src/agent/context.rs:49-53),裁剪按整轮丢弃,摘要是自由文本
(context.rs:156-162)。/context 只能给一个预算百分比。实际后果:长任务里模型
突然开始重复劳动,而用户无法判断是「它忘了」还是「它没用工具」。

### 2.5 P1 — 无法给模型调参

`AgentCfg` 只有轮数/上下文/并发/重试/权限/feedback(src/config.rs:94-108)。
没有 temperature、没有 max_tokens(Anthropic 的 4096 写死在 src/llm/anthropic.rs:172)、
没有 reasoning effort、没有超时。想「这次要它稳一点」只能换模型。

### 2.6 P1 — 提示缓存完全没用上

全仓 `cache_control` 命中 0 次。Anthropic 的 system + 工具 schema + 历史前缀
每轮都是全价;OpenAI 兼容端点的自动缓存也只靠运气(前缀必须逐字节稳定)。
长会话的 token 账单里,这一项通常占大头。

### 2.7 P2 — 工具面缺几块「日常必需」

- **删/移文件**:除 `run_command` 之外没有路径;模型只能 del/move,被审批挡(正确但慢)。
- **图片/PDF**:`fetch_url` 剥成纯文本并 1 MiB 封顶(src/tools/fetch.rs:16-18);
  拖入 `note.png` 的 @ 引用会被当二进制拒绝。截图类问题是当前最常见的「我帮不了」。
- **会话检索**:`list_summaries` 每次把目录里所有 JSON 全量解析(src/agent/session.rs:100),
  没有索引、没有跨会话搜索。
- **系统提示与并行矛盾**:提示词写「一次只调一个工具」(src/agent/mod.rs:286),
  引擎却支持并发 4(:370、:624)。并行能力被自己的提示词废掉了。

### 2.8 P1 — `fetch_url` 的私网防护可被一条 302 绕过(已核对)

`fetch_url` 只检查**首个 URL** 的 host(src/tools/fetch.rs:121-128),随后用 reqwest
的默认客户端发请求。reqwest 0.12 的默认重定向策略是 `Policy::limited(10)`
(已在本地 0.12.28 源码确认),而自定义 `Client::builder()` 里没有 `redirect(...)`。
所以 `https://attacker.example/302→http://169.254.169.254/latest/meta-data` 会被跟随,
私网判定完全不参与重定向链。SSRF 防护是 kazi 对外的少数安全承诺之一,这一条必须堵。

修法:`redirect(Policy::custom(...))`,每一跳重新跑 `is_private_host` + 协议检查,
超限直接报错(而不是静默截断)。测试用本地 `TcpListener` 伪造两跳(现有 MCP HTTP 测试
已有同类手写服务器可复用)。

### 2.9 P2 — 扩展面是空的

没有 KAZI.md / 项目约定、没有 .kazi/commands/、没有技能、没有 hook;MCP 只桥接
tools(不读写 resources/prompts)。命令表是静态常量 `COMMANDS`(src/agent/console.rs:827),
面板是写死的 `Overlay::Palette`(src/agent/tui/overlay.rs:154)。

## 3. 产品原则(带判据,不是口号)

1. **默认拒绝,且每一次放行都能溯源。**
   判据:任何能改磁盘或联网的动作,要么在白名单里透明通过,要么有一张能被指认的
   「这张卡对应这条命令」的审批;审批队列不允许模糊。
2. **界面永不等待 I/O。**
   判据:任何网络/模型/子进程操作都有 loading 态,且首帧立刻画出(0.16.0 的教训)。
3. **一个概念,一个入口。**
   判据:同一件事不允许同时存在「命令」「快捷键」「配置文件」三套不同语义;新增入口
   必须先删掉旧的(0.17 删 REPL 是正例)。
4. **错误是数据,不是异常。**
   判据:模型侧的失败必须回传可读原因,不得静默;UI 侧的失败必须给出下一步动作。
5. **诚实优于聪明。**
   判据:上下文、费用、缓存命中、被裁掉的内容,数字必须可核对;不允许「看起来省了」。
6. **扩展点必须能写成三个文件。**
   判据:给项目加一条自定义命令、一个技能、一条约定,各自不超过一个文件、不需要写 Rust。

## 4. 路线图总览

| 版本 | 主题 | 一句话 | 预估改动 |
|------|------|--------|----------|
| **0.19** | 你说了算的会话 | 先修审批/权限的正确性,再给用户三样控制权:写入审阅、模型档位、成本可见 | ~3.5k 行,6 个支柱 |
| 0.20 | 外部世界的接口 | 后台作业、MCP resources/prompts、图片与 PDF 输入、跨会话检索 | ~2.5k 行 |
| 0.21 | 可扩展的工作台 | 项目约定文件、自定义命令、技能、hook、profile;为 1.0 的库 API 定契约 | ~3k 行 |

顺序理由:0.19 全部是**正确性与信任**。审批答错问题、子代理偷改权限,这两个 bug 会让
后面所有「更强能力」变成风险放大器;写入审阅、档位、成本则是用户每天都会碰的三件事,
而且都能复用现有基础设施(diff 卡片、set_ui_setting 回写、Usage)。

## 5. 0.19 详细设计:你说了算的会话

### 5.0 先做正确性:三个 P0(不带新功能,单独可发布)

这一节的三件事都是既有承诺的修补,不引入新概念;按 VERSIONING 的口径它们本身只够
PATCH,但会和 0.19 的功能一起走 MINOR(见该文件「一次批次里既有修复又有功能」一行)。

| # | 问题 | 修法 | 验收 |
|---|------|------|------|
| 1 | 子代理写坏父会话权限(§2.1)| `ToolCtx::derive` 新开权限单元 | 集成测试:delegate 只读子任务后父会话仍可写 |
| 2 | 审批没有身份(§2.2)| ask 带 id/task/origin,弹窗排队而不是覆盖 | 并发两条 ask 时两条都能被逐一回答 |
| 3 | `fetch_url` 重定向绕过私网检查(§2.8)| 自定义 redirect 策略,每一跳复检 | 两跳测试:第二跳指向 127.0.0.1 必须失败 |

### 5.1 P0 审批与权限的正确性(先做,不带新功能)

**目标**:审批与它对应的那条命令永远一一对应;任何子作用域都不能改变父作用域的权限。

设计:

```rust
// 审批请求带身份,UI 不再靠 FIFO 猜
pub struct ApprovalAsk {
    pub id: u64,                 // 单调递增
    pub task: u64,               // 哪个任务发起的
    pub origin: AskOrigin,       // Main | SubAgent { label: String }
    pub tool: String,
    pub summary: String,
}

impl ToolCtx {
    /// 子上下文:换一个独立的权限单元,绝不借用父的原子变量
    pub fn derive(&self, mode: PermissionMode) -> ToolCtx {
        let mut c = self.clone();
        c.permission = Permission::new(mode); // 新 Arc,新原子量
        c
    }
}
```

- `SubAgentRunnerImpl::run_sub` 改用 `self.base_ctx.derive(ReadOnly)`(或 `derive(Edit)`),
  父会话的模式不再被写坏;子代理的审批在弹窗标题上标 `[sub-agent]`。
- 每个 agent 一个 `TuiApprover` 实例(provider 切换时随新 agent 新建),
  `ApprovalAsk` 带 `id` / `task` / `origin`;`PendingApproval` 由单槽改为 `VecDeque`,
  弹窗按 id 回答、按队列顺序显示(`审批 2/3 · 任务 #12 · [sub-agent]`),不再互相覆盖。
- 任务结束时把该任务留下的 ask 统一 `Deny("task already finished")`,而不是留下孤立的框;
  被拒绝的那一方拿到的是一条明确的工具错误,而不是无声的默认值。
- 弹窗同时显示当前命令的**影响范围**:涉及路径、是否含 `;` / `|` / `$(` 、是否会删除。
  已有的整串 `approval_key` 记忆(0.18)不变。
- 回归测试:`derive_does_not_touch_the_parent`、`queued_approvals_are_answered_by_id`、
  `subagent_approval_carries_the_origin`。

### 5.2 写入审阅:edit 模式从「先斩后奏」变成「先看后批」

**目标**:默认路径不变(写就写),但当模型一轮要动多个文件时,用户能一次看全、逐块取舍。

设计:

- 新增一档 `review`(在 `edit` 与 `read-only` 之间,四档循环):
  `write_file`/`apply_patch` 不落盘,先把「计划改动」(路径 + 统一 diff)交给引擎暂存;
  一轮结束时 TUI 弹出 **changeset 审阅**:整份 diff 可滚动,`a` 全部接受、
  `space` 切单个文件、`r` 拒绝该文件、`Enter` 提交。被拒绝的路径以 `is_error` 回传
  (`user rejected this change; keep the rest of the plan`),模型可以继续。
- 实现挂点:工具轮收口处(mod.rs 的 JoinSet 收尾)已经知道每个 call 的 `edited_path`
  与 diff,不需要新协议;审阅模式下的写工具只返回「pending」。
- /undo 语义细化:changeset 被接受后仍记一个检查点 epoch;被拒绝的路径不产生 epoch,
  这样「接受一半」也能整体回滚。
- 默认不开启(保持 0.18 手感),`/permission review` 或 `agent.permission_mode = "review"` 启用。
- 测试:`review_stages_instead_of_writing`、`partial_acceptance_leaves_other_files_untouched`、
  `rejected_file_reports_back_to_the_model`。

### 5.3 模型档位:一次按键换「快/准」

**目标**:调参不需要背配置,而且选择在重启后还在(照抄 [ui] theme 的回写路径)。

设计:

```toml
[agent]
preset = "balanced"            # fast | balanced | deep

[agent.presets.deep]
temperature = 0.2
max_tokens = 8192
reasoning_effort = "high"      # 支持的端点透传,不支持的忽略
```

- `AgentCfg` 增加 `preset` 与 `presets: BTreeMap<String, Preset>`;解析后并入
  `ProviderSettings`(请求级字段,不污染 provider 条目)。
- UI:`/effort [fast|balanced|deep]`,写回 `[agent] preset`(复用 `config::set_ui_setting`
  的行内回写,注释保留);头部在模型名旁挂一个 `· deep` 徽标;Tab 补全档位名。
- `max_tokens` 取代 anthropic.rs:172 的常量;OpenAI 侧同样显式发送。超长回答的截断
  要变成可识别事件(`FinishReason::Length` → `answer truncated by max_tokens`,并提示
  `/effort deep`)。
- 测试:`preset_reaches_the_wire`、`preset_is_written_back_without_losing_comments`。

### 5.4 成本与缓存:把「花了多少」变成可核对的数字

**目标**:每个任务结束后,用户知道这次花了多少钱、缓存省了多少;超预算会停。

设计:

- `Usage` 已带 in/out(src/llm/types.rs:146);增加 `cache_read`/`cache_write`(Anthropic
  的 `cache_creation_input_tokens` / `cache_read_input_tokens`,OpenAI 的
  `prompt_tokens_details.cached_tokens`)。
- **提示缓存**:
  - Anthropic:`cache_control: {type: "ephemeral"}` 打在 system 块、最后一个工具 schema
    块、以及历史末尾的「稳定前缀」上;保证 system 与工具目录**逐字节稳定**(现在
    `system_prompt` 里带 cwd,可接受;压缩摘要追加在末尾会破坏前缀 → 摘要改为插到
    消息流第一条 user 之前,而不是拼进 system)。
  - OpenAI 兼容:前缀稳定即可享受自动缓存,不额外发送字段。
- **价格表**(用户可覆盖,不内置会过期的价格):
  ```toml
  [usage.prices."commandcode/deepseek-v4-pro"]
  input = 0.28      # USD / 1M tokens
  output = 0.42
  cache_read = 0.028
  ```
  未配置价格的模型显示 token 数,**绝不猜钱**。
- UI:`/cost`(本次 / 本会话 / 按模型三行)、页脚在任务结束追加 `· $0.013`(仅在价格已知时),
  `kazi run --output json` 增加 `cost` 对象;`agent.max_usd_per_task` 触发时中断并保留历史。
- 测试:`cache_breakpoints_are_byte_stable`、`unknown_price_never_prints_money`、
  `budget_guard_stops_between_rounds`。

### 5.5 上下文诚实化

**目标**:模型忘事时,用户能看出是「被裁了」还是「它没记住」;裁剪不再随机砍掉关键约定。

设计:

- 裁剪从「从头砍」改为 **保留三段**:头部(项目约定 + 前几条任务的关键句)、
  最近的 N 轮、以及**被丢弃轮次的结构化摘要**(文件路径、决策、未完成项),
  中间才是可裁区。/context 画出三段占比,并标出「即将丢弃」的条目。
- 新增 `/pin [n]`(`Ctrl+P`):把某条消息标为不可裁;UI 在条目左侧画 `◆`。
  这解决「这句约束必须一直生效」的真实需求,比反复 `/compact force` 可靠。
- 摘要器提示词重写为固定 schema(files / decisions / open / next),失败时明确
  标注 lossy(已有行为,保留)。
- 顺带修正 src/agent/mod.rs:286 的「一次只调一个工具」:改成「无依赖的调用请**同一轮
  并列发出**」,与引擎的 4 路并发对齐,并给出反例(先 glob 再读)。
- 测试:`pinned_message_survives_trimming`、`summary_is_structured_and_labeled`、
  `parallel_instruction_matches_the_engine`。

### 5.6 工具面补齐(小而硬)

| 新增 | 说明 | 安全 |
|------|------|------|
| `delete_file` | 单文件删除,`dry_run` 返回将被删的路径 | 先快照到检查点,可 /undo;目录禁止(要求逐文件) |
| `move_file` | 改名/移动,跨目录受 sandbox 约束 | 目标已存在 → 除非 `overwrite`,否则报错回传 |
| 图片附件(`@img.png`) | 作为**消息附件**发给模型,不新增同名工具 —— 文本工具无法传图 | 只读、上限 5 MiB、png/jpg/webp/gif 白名单 |
| `fetch_url` 增强 | `Content-Type: application/pdf` → 抽取文本(零依赖:只支持文本型 PDF) | 仍拒绝内网;超限截断并注明 |

- 附件走 `Message`,这是 wire 层改动:`Message.parts: Vec<Part>`,其中
  `Part::Image { media_type, base64 }`;OpenAI 发 `image_url` data URL,Anthropic 发
  `{type:"image", source:{type:"base64", ...}}`,Ollama 走 OpenAI 形态。老会话 JSON 不含
  `parts` 时按纯文本读(向后兼容,不加 version 字段)。
- 不支持视觉的模型收到图片时,**不发请求就报错**:「当前模型不接受图片输入,已移除附件;
  可用 /model 换成支持视觉的模型」。绝不静默丢。
- 测试:`image_parts_roundtrip_both_wires`、`old_session_without_parts_still_loads`、
  `visionless_model_refuses_with_actionable_text`。

### 5.7 会话检索(把文件变成资料库)

- 写会话时同步维护 `<data-dir>/kazi/index.json`(name、mtime、消息数、cwd、首条任务、
  token 总量),浏览/列表 O(1);索引缺失或版本不符时回退到现在的全量扫描。
- `/find <text>` 跨会话搜索(只搜 user/assistant 文本,不搜工具输出),命中行显示
  `session · 时间 · 片段`,Enter 打开并跳到该条。`kazi sessions --find` 同一实现。
- 测试:`index_survives_a_corrupt_file`、`find_reports_session_and_offset`。

### 5.8 0.19 的界面收尾

- 头部:模型后加档位徽标;成本(已知时)与缓存命中率进 /status。
- 帮助浮层 `KEY_BINDINGS`(src/agent/tui/overlay.rs:22)与 `COMMANDS` 表同步新增项;
  `key_help_and_commands_stay_in_sync` 已锁定这个不变量,新增命令必须一起改。
- 新命令:`/effort`、`/cost`、`/pin`、`/find`;全部进 Tab 补全与 `works_while_busy` 白名单
  (后三个只读 UI,可在任务进行中执行)。

### 5.9 0.19 验收

```text
cargo fmt --check                                  零 diff
cargo clippy --all-targets -- -D warnings          零警告
cargo test                                         全绿(预计 249 → ~300)
cargo package --no-verify                          0 警告
手工:review 模式改 3 个文件 → 只批 2 个 → /undo 恢复被批的 2 个
手工:delegate 跑完只读子任务后,父会话仍能写文件(0.18 的回归)
手工:断网启动 → /model、/sessions、/cost 都不卡(原则 2)
```

## 6. 0.20 / 0.21 概要

### 0.20 外部世界的接口
- **后台作业**:`run_command { background: true }` → job id;`/jobs` 列表(运行中/退出码/
  输出尾部),`job_output` / `job_kill` 工具;TUI 一条常驻状态栏。解决「起个 dev server
  就把整个循环卡死」。
- **MCP 补全**:`resources/list` + `resources/read` 作为只读工具;`prompts/list` 作为
  `/mcp-prompt` 的来源;`tools/list_changed` 收到后热更新目录(现在是握手一次定终身)。
- **图片/PDF 的第二种来源**:`fetch_url` 直接返回附件而不只是文本。
- **跨会话检索**升级为全文索引(仍零依赖:按行分块 + 倒排落盘),/find 支持正则。

### 0.21 可扩展的工作台(也是 1.0 的 API 契约)
- **项目约定**:从 cwd 向上找 `KAZI.md` / `AGENTS.md`,再加 `~/.config/kazi/KAZI.md`;
  以「来源标注 + 分段」注入 system,`/instructions` 查看与就地编辑。
- **自定义命令**:`.kazi/commands/<name>.md`,front-matter 声明参数与描述,
  正文里 `$ARGUMENTS` / `$1` 模板;自动进入命令面板的第二段(内置 / 项目 / 技能)。
- **技能**:`.kazi/skills/<name>/SKILL.md`(描述 + 触发条件 + 步骤),`/skills` 列出,
  模型可通过 `use_skill` 工具按需加载正文 —— 与 DSH 的 skill 模型同构,用户换工具不用换写法。
- **hook**:`[[hooks]] event = "pre_tool" | "post_tool" | "post_task"`,命令 + 超时 + 失败策略;
  `pre_tool` 返回非零即拒绝(把审批策略交给项目)。
- **profile**:`--profile work` 叠一层 config,解决「家里 Ollama、公司网关」的切换。
- **库 API 冻结**:公开 `kazi::prelude`,明确 Agent / Provider / Tool / Approver 的兼容承诺,
  写进 docs/architecture.md 与 README。

## 7. 明确不做(以及为什么)

| 不做 | 理由 |
|------|------|
| 图形界面 / 常驻 daemon | 违背「一个二进制、终端里的工具」的定位;需要 GUI 就用别的产品 |
| 内置向量检索 / 嵌入模型 | 依赖重、离线不可控;结构化摘要 + 全文检索已覆盖绝大多数场景 |
| 自动生成并执行迁移脚本 | 不可审计;`run_command` 的审批是最后一道人类关卡 |
| 默认开启遥测 | 与「密钥只在本地」的承诺冲突;要统计就显式 `--telemetry` |
| 把 API key 写进 config.toml | 现有 auth store + env 的双通道已经够,且可被审计 |
| 为兼容保留 REPL / `--tui` 开关 | 0.17 已证明:砍干净比留影子模式便宜 |

## 8. 风险与开放问题

1. **review 模式会不会拖慢常用流?** 缓解:只在「一轮改动 ≥2 个文件」或显式开启时弹出,
   单文件小改仍直接落盘;`/permission` 一键切回 edit。
2. **缓存断点会不会让不支持字段的网关 400?** 缓解:仅对 `type = "anthropic"` 发送;
   OpenAI 兼容侧只做「前缀稳定」,不发新字段。
3. **价格表由谁维护?** 用户自己填,项目只提供 `[usage.prices]` 的样例与校验;
   绝不内置会过期的数字(§3 原则 5)。
4. **0.19 的体量**:六个支柱一起做约 3.5k 行,接近 0.18 的规模。若必须拆,
   先 P0(§5.1)+ 工具面(§5.6),其余按 0.19.1、0.19.2 走 —— 但 PATCH 不夹带功能,
   所以拆也只能拆 MINOR。

## 9. 一页总结

0.18 把「能看见」做完了,0.19 要把「能相信」做完:
审批必须答对问题,权限不能被偷偷改写,改动可以先看再批,参数和花费可以核对,
上下文裁了什么要说得出来。这五件事做完,后面加多少能力都不会放大风险。

## 10. 收尾:0.26 之后还剩什么,以及为什么先不做

0.19 到 0.26 把这份文档里**承诺过的主题全部交付**了(0.19.0 → 0.26.0,10 个版本)。
只剩 §6 的两条尾巴,它们被**有意推迟**,不是遗漏:

| 待办 | 为什么现在还值得缓一缓 |
|------|------------------------|
| `fetch_url` 直接返回图片/附件 | 本地图片已经通过 `@shot.png` 进入消息(走 `Message::images`,一条干净的通道)。让**工具结果**携带 image part 需要把 `ToolOutcome` 改成多模态载荷,牵动每个工具和每次事件回放;而从 URL 抓图的收益小于 `@file` + `fetch_url` 文本 + `read_file` 的现成组合。真要做,就做成"附件"这一等公民,而不是给 fetch 开特例。 |
| 会话全文索引 + 正则 `/find` | 现在的 `/find` 线性扫会话文件,几百个会话仍是毫秒级;而索引是**要版本化的磁盘格式**(损坏、迁移、并发写),正则要引入新依赖。等有人抱怨慢、或子串语义真的不够用时再做 —— 那时才知道该建什么索引。 |

**1.0 之前真正值得做的**,按顺序:双模型委派、显式 `--telemetry`、会话格式迁移
工具、把 §13「为什么不用框架」写成可跑的基准。1.0 的判据不是功能数量,而是:
`kazi run` 的 JSON 契约、`kazi::prelude`、配置文件与 hook 协议在连续两个 MINOR
里**零破坏**。
