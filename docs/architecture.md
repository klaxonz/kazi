# kazi 架构与原理

> 这份文档回答一个的问题:**一个 AI Agent 到底是什么,它是怎么跑起来的**。
> kazi 的全部实现都为了把答案讲清楚,不用任何 agent 框架——
> 你读完本文 + 对照源码,应该能自己写一个。

## 1. Agent 的本质:一个循环

LLM 本身只会"输入文本 → 输出文本",它不能真正"执行"任何事。
Agent 的全部魔法来自一个朴素的循环:

```text
用户任务
   ↓
┌────────────────────────────────────────────┐
│  ① 把「历史 + 工具清单」发给模型              │
│  ② 模型回复: 纯文本?→ 结束,返回答案          │
│     工具调用?→ ③                            │
│  ③ 每个 tool_call: 本地执行 → 结果回填历史     │
│  ④ 回到 ① (模型看到结果后决定下一步)           │
└────────────────────────────────────────────┘
```

代码位置:`src/agent/mod.rs` 的 `Agent::run_task`。就是这个循环,其余都是工程质量问题:

- **termination**: 轮数上限 (`agent.max_rounds`,默认 30),防止模型无限循环调工具
- **失败自纠**: 工具报错不中断循环,把错误文本以 `is_error` 标记回传,模型下一轮自己改参数重试
- **上下文膨胀**: 每轮追加消息会无限膨胀,见 §6 的裁剪

## 2. 多协议:为什么 Provider 要抽象

三种"模型服务"对"工具调用"的表述完全不同。kazi 内部用一套统一类型
(`src/llm/types.rs`),每个 provider 负责自己的双向翻译:

### OpenAI 兼容协议 (`/chat/completions`)

```json
{
  "messages": [
    {"role": "system", "content": "..."},
    {"role": "user", "content": "..."},
    {"role": "assistant", "content": null,
     "tool_calls": [{"id": "call_x", "type": "function",
                     "function": {"name": "read_file",
                                  "arguments": "{\"path\":\"a.txt\"}"}}]},
    {"role": "tool", "tool_call_id": "call_x", "content": "1: hello"}
  ],
  "tools": [{"type": "function", "function": {"name": "...", "parameters": {...}}}]
}
```

注意坑:**arguments 是 JSON 字符串,不是对象**——模型经常写错,解析失败时
kazi 保留原始字符串,让工具报错驱动模型重试。

### Anthropic 协议 (`/v1/messages`)

```json
{
  "system": "...",                      ← 顶层参数,不是消息!
  "messages": [
    {"role": "user", "content": [{"type": "text", "text": "..."}]},
    {"role": "assistant", "content": [
       {"type": "text", "text": "checking"},
       {"type": "tool_use", "id": "tu1", "name": "read_file", "input": {"path": "a.txt"}}
    ]},
    {"role": "user", "content": [      ← tool_result 必须在 user 消息里!
       {"type": "tool_result", "tool_use_id": "tu1",
        "content": [{"type": "text", "text": "1: hello"}], "is_error": false}
    ]}
  ],
  "tools": [{"name": "...", "input_schema": {...}}],
  "max_tokens": 4096                    ← 必填!
}
```

三个协议级差异是 `src/llm/anthropic.rs` `wire()` 的复杂度来源:

1. system 提升为顶层参数
2. 工具调用是 assistant 消息的 content block (tool_use)
3. 工具结果必须合进**一条 user 消息**——kazi 的历史是逐工具追加一条 Tool 消息,
   转 wire 时要把连续的 Tool 消息**分组合并** (否则 422 报错)
4. 严格的 user/assistant 交替——恢复的会话若以 assistant 开头,补一个 stub user

往返正确性由测试锁定:`anthropic.rs` 中的 wire 单测断言了以上每一种形态。

### 模型发现与选择器

`Provider` trait 上还有一个带默认实现的 `list_models()`(默认返回空):
OpenAI 兼容走 `GET {base}/models`,Anthropic 走 `GET {base}/v1/models`,
Ollama 复用前者;`RetryProvider` 直接转发。`/provider`、`/model` 和首次引导
都不再要求手输名字,而是从候选列表里选。

候选由 `llm::merge_catalog()` 合并三处来源并去重(优先级 config > 实时 > 内置):

1. `config.toml` 里该 provider 的 `models = [...]`(用户显式列出)
2. 端点实时返回的模型 id
3. `llm::default_models(kind)` 的内置回退清单

取候选的入口是 `console::model_catalog()`(带 ~3s 超时,任何一个来源失败都
静默退化,离线也能用)。TUI 把列表画成输入框上方的浮层(`tui.rs::draw_picker`,
方向键移动 + 输入即时过滤 + Enter 选中);`/model <provider>/<model>` 是直接形
式。候选与参数解析分别是 `Picker`(纯逻辑,可单测)、`console::model_choices_static()`
/ `console::resolve_model_arg()` / `cli::resolve_pick()`。

## 3. 流式输出

两个协议都用 SSE,但格式不同 (`src/llm/sse.rs` 是共用的 ~80 行解码器,含义注释齐全):

- **OpenAI**: 每个数据块是一个 `chunk`,内容在 `choices[0].delta.content`;
  工具调用是**增量碎片**——`delta.tool_calls[].function.arguments` 按序号分片拼接,
  结束标记 `data: [DONE]`
- **Anthropic**: 带名字的事件 (`event: content_block_delta`),
  文本增量 `{delta:{type:"text_delta", text}}`,
  工具参数增量 `{delta:{type:"input_json_delta", partial_json}}` 按 block index 累积

Provider trait 只要求 `complete_stream(req, tx)`——内部发 `StreamEvent::TextDelta`,
结束时一次性返回完整 `LlmResponse` (含重组好的 tool_calls)。
核心循环把 delta 转发给 UI (`AgentEvent`),拿到完整响应后走工具执行。

## 4. 工具系统

`src/tools/mod.rs`:

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;   // 手写 JSON Schema,一个可见处
    async fn run(&self, args: &Value, ctx: &ToolCtx) -> Result<String, ToolError>;
}
```

约定:

- 工具返回 `String` 永远是给模型看的文本;错误 `Err(ToolError)` 会被标记 `is_error`
- **错误是数据不是异常**: 不知情就中断的 agent 永远学不会自纠
- schema 手写不代码生成——一个工具的自契约只在一个地方

全部注册在 `tools/builtin()`,与模型可见的 catalog 同源:
`list_dir` / `glob` (按 `*`/`?`/`**` 找文件) / `read_file` (行号分页+二进制拒绝) /
`apply_patch` (唯一性 find-replace) / `write_file` / `run_command` (§5) /
`fetch_url` (§8),外加由 MCP 服务器注入的工具 (§9)。

## 5. run_command 的安全模型

模型会请求执行真正的 shell 命令——这是最小权限设计的重点:

```text
命令请求 (如 "cargo test")
  ├─ 命中 allowlist 且无 shell 分隔符 (; & | < > ` 换行 → 自动放行)
  ├─ 否则终端确认 y/N (kazi chat) 或拒绝 (headless 未加 --yolo)
  └─ 拒绝的信息也回传模型:「不要重试,换一种方法」
```

执行细节: Windows 走 `cmd /C`,Unix 走 `sh -c`;超时强杀 (`tools.command_timeout_secs`),
stdout/stderr 各截断一半输出预算,非零退出码 → `is_error=true` (模型会感知失败)。

`--yolo` 是唯一跳过确认的途径,写进了 README 的风险提示——**默认拒绝是底线**。

## 6. 上下文管理

Token 计数用**字符估算** (4 chars ≈ 1 token + 每条消息 32 overhead):
决策精度足够,零依赖 (`src/agent/context.rs`)。

裁剪策略:超过 `agent.max_context_tokens` 丢**整轮对话** (user + 它的所有
assistant/tool 往返),而不是丢单条消息——因为 Anthropic 拒绝「tool_result 没有对应
tool_use 父消息」的历史,单条裁剪会制造幽灵。最新一轮永不丢弃。

`/compact` 手动触发同一逻辑。

## 7. 会话持久化

`session.rs`: `dirs::data_dir()/kazi/sessions/<name>.json`,
存 `system + messages`。`--resume <name>` 恢复;TUI chat 每轮任务后自动保存
(best-effort,失败只提示不打断)。

## 8. fetch_url 的网络边界

- 只允许 https;localhost/回环/私网 IP (RFC1918/5737/6598…) 一律拒绝 (基础 SSRF 防护)
- 1 MiB 字节上限 + 30 秒墙钟上限,双保险读流
- HTML 粗蜕壳成文本 (剥 script/style + 标签),模型消费的是文字不是浏览器

## 9. 并行工具与 MCP(stdio + Streamable HTTP)

**并行工具调用**。`Agent::run_task` 的工具轮执行顺序:

```text
① 预检(串行):对每个 call 问 Tool::approval_summary —— 需要审批的逐个问
   (run_command 非白名单),拒绝 → 直接产生 is_error 结果,不再分发
② 分发(并发):其余 call 一起进 JoinSet,Semaphore(max_parallel_tools,默认 4)
③ 完成:JoinSet 完成一个发一个 ToolEnd(UI 即时可见)
④ 回填:最终按【调用顺序】写回历史——OpenAI/Claude 的 tool_call id
   必须与 tool_result 顺序对应,乱序会被 Anthropic 拒绝
```

关键取舍:**审批询问必须串行**(终端问答不能互相穿插),而执行可以并发——
所以把审批逻辑从 `Tool::run` 内部提出来(`approval_summary`),执行时假定已批准。

**MCP 客户端**。MCP = JSON-RPC 2.0;kazi 手写两个约 300 行的传输,共用一个
`McpSession` 抽象(initialize / tools/list / tools/call 三个调用,桥接层零感知):

- **stdio** (`client.rs`):本地子进程,换行分隔 JSON-RPC;后台 reader 按响应 id
  路由到 pending 表 → **同一服务器天然支持并发在途**(引擎的
  max_parallel_tools 决定实际上限);服务器主动 ping 由 reader 应答;
  子进程 kill_on_drop,Arc 归零即收割
- **streamable HTTP** (`http.rs`):远程服务器;每条 JSON-RPC 消息一次 POST,
  响应可以是纯 JSON 或 SSE 流(复用 llm 的 SseDecoder 按 id 扫描);
  `Mcp-Session-Id` 捕获后回带;`Authorization: Bearer`(key 只在环境变量);
  notifications → 202 丢弃;**不实现 GET 推送流**(规范可选——代价仅是不收
  主动通知,请求/响应流完整);每次请求独立超时——握手阶段没到响应头或
  工具调用挂起,统一报 Timeout

测试全部 in-process:dulex 伪 stdio 服务器、手写 HTTP/1.1 mock 服务器,
覆盖握手/列表/调用/isError/超时/会话与鉴权断言/乱序并发回复(服务器
先答第二个请求)。

## 10. 添加一个新工具 (30 行以内)

```rust
// src/tools/weather.rs
pub struct GetWeather;

#[async_trait]
impl Tool for GetWeather {
    fn name(&self) -> &'static str { "get_weather" }
    fn description(&self) -> &str { "Get current weather for a city" }
    fn parameters(&self) -> Value {
        json!({"type": "object",
               "properties": {"city": {"type": "string"}},
               "required": ["city"]})
    }
    async fn run(&self, args: &Value, _ctx: &ToolCtx) -> Result<String, ToolError> {
        let city = super::arg_str(args, "city")?;
        Ok(format!("{city}: 22C cloudy"))  // 接真 API 时替换
    }
}

// 注册: ToolRegistry::builtin() 里加一行
//      .register(Arc::new(weather::GetWeather))
```

schema 会自动进入下一轮请求;模型调用它时循环零改动。

## 11. TUI:结构化转录 + 渲染管线

旧 TUI 只有 `Vec<(kind, String)>` 的行列表;模型答案里的 markdown、工具输出、
diff 全被压成一行文本。现在分成**状态**与**渲染**两层:

```text
AgentEvent ──► Transcript (Entry: User/Assistant/Tool/Notice/Rule)
                    │  rev++（只有内容变化才失效缓存）
                    ▼
   渲染缓存 (rev, width, theme, details) ──► Vec<Line<'static>> + owner[]
                    │                              │
        markdown.rs / highlight.rs         鼠标点击命中测试
                    │                              │
                    └──────── ratatui Frame ◄──────┘
```

- **transcript.rs** 是唯一的状态:助手答案是 markdown 原文,工具调用是
  `ToolCard`(id、参数摘要、状态、完整输出、diff、耗时、是否展开)。流式答案
  不进缓存(每帧重渲染),冻结部分按 `(rev,width,theme,details)` 缓存,
  所以滚动/缩放只重排一次。
- **markdown.rs** 把标题、列表、引用、表格、代码块和内联样式变成带样式的行;
  换行在这里完成(而不是交给 `Paragraph`),因为滚动位置要按真实屏幕行计算。
  宽字符按显示宽度处理,未知语法原样保留。
- **highlight.rs** 是逐行 tokenizer(关键字/字符串/注释/数字/函数/JSON key,
  含跨行块注释),零依赖。
- **theme.rs** 把颜色和字形集中管理(`dark`/`light`/`nord`/`solarized`/`mono`,
  `KAZI_ASCII`),`Ctrl+T` 或 `[ui] theme` 切换;切换后用
  `config.rs::set_ui_setting` 按行回写 `[ui]`(注释与排版保留),所以下次启动
  保持同一外观;`/details` 同理。没有 config 文件(库内嵌)时只改当前会话。
- **overlay.rs** 是模态层:命令面板(模糊匹配)、帮助、会话浏览(宽屏带预览
  面板)、转录搜索、diff 阅读器、provider/model 选择器、审批框。打开时独占
  键盘,永不占用布局行;列表把可见行区间交给事件循环,所以滚轮/点击与键盘
  共用同一套游标。
- **编辑器**(`tui.rs::Input`)自己维护按「编辑段」合并的 undo/redo、按词移动、
  软换行与点击命中(`caret_at_visual` 与 `visual_rows` 共用 `wrap_ranges`,保证
  点击位置和实际绘制的行一致);命令只写了一半时在光标后画暗色 ghost 补全。
- 转录搜索命中后跳转的 Entry 会用选中色高亮一帧(下一次按键清除);`/usage`
  的会话累计 token 由 `finish_task_usage` 在每个任务结束时并入总量。
- 每次绘制把「屏幕行 → Entry 下标」记进 `hit_owner`,点击据此展开/收起对应
  卡片,右键复制该条;输入框、列表和 diff 各自的命中矩形同理。滚动条、上下文
  仪表、plan 面板、输入框和 footer 都是同一帧布局的一部分;终端过小时按优先级
  先砍 plan/footer 再砍输入框,绝不越界绘制。
- 任务运行期间 agent 被移出共享槽,命令按「是否需要 agent」分流:只读 UI 的
  `/status`、`/context`、`/plan`、`/allow`、`/diff` 等立即执行,其余提示稍后。

### 事件里的工具信息

为了画出卡片,`AgentEvent` 从「名字 + 一行预览」扩展为:

```rust
ToolStart { id, name, summary }                 // summary = tools::call_summary()
ToolEnd   { id, name, is_error, preview,
            output,      // 完整输出(封顶 32k),供展开查看
            diff,        // write_file/apply_patch 的 unified diff
            elapsed_ms }
```

`id` 是 wire 上的 call id——并行工具因此不会串卡。**diff 在引擎里生成**:
执行前按 `tools::edited_path(call)` 读旧内容,成功后读新内容,交给
`src/diff.rs`(去公共前后缀 + LCS,失败退化为整块替换)输出带 `@@` 的 hunk。
TUI 只负责上色;`kazi run --output json` 也顺带得到全部信息。

## 12. 0.19 的三处结构性改动

这一版没有新增协议,但改了三件事的**所有权**,值得单独说明。

### 12.1 权限单元:clone 不等于共享

`ToolCtx` 是 `Clone`,但里面的 `permission` 是 `Arc<AtomicU8>`。0.18 之前,
`delegate` 克隆上下文后直接把副本设为只读——因为共享同一个原子量,**父会话也被
静默切成只读**。修法是 `ToolCtx::derive(mode)`:`/delegate`、子代理这类
"另一个作用域"必须走它,副本拿到新的权限单元;其余字段仍共享(审批器、检查点、
待办本来就应该共享)。回归测试 `derived_ctx_does_not_touch_the_parent_permission`
锁死这条。

### 12.2 审批的身份:队列,而不是最后一个

审批请求 `ApprovalAsk` 带 `id` / `task` / `origin`(主任务或 `[sub-agent]`)。
TUI 端从"一个槽位"改成 `VecDeque`:先到的先答,弹窗上写清是哪条任务、哪个来源、
还有几条在等。任务结束时,仍然挂着的审批统一 `Deny("task already finished")`,
不会留下一个没人回答的 oneshot。

### 12.3 审阅模式:写工具只"登记",引擎统一落盘

`review` 权限下,`write_file` / `apply_patch` 不碰磁盘:它们把
`StagedChange { path, before, after }` 放进 `ctx.staged`(`Staging`,同一路径后写覆盖
前写,后续 patch 基于**已登记的内容**而不是旧盘面),然后返回 "staged"。

一轮工具结束后,引擎(`run_task`)取走整批:

```text
staging.take() → Reviewer::review(cwd, changes)
                   ├─ TUI: 弹 changeset 浮层,逐文件取舍
                   └─ 无 UI: AutoReviewer 全接受(headless)
→ apply_reviewed():接受的文件先 checkpoint 再写;拒绝的文件根本不写
→ 每个 staged call 的工具结果被改写成「reviewed + written」或「NOT written: 原因」
```

被拒绝的路径以 `is_error` 回传,模型因此知道"这部分没发生",而不是以为自己写成功了。
提交后才计算 `wrote`,所以 `feedback_command` 不会为一次被拒绝的改动白跑一遍。

### 12.4 成本与缓存:数字必须可核对

- `ProviderSettings.tuning` 携带 `/effort` 档位(temperature / max_tokens /
  reasoning_effort),两个 wire 只发送 `Some` 的字段。
- `Usage` 增加 `cache_read_tokens` / `cache_write_tokens`;Anthropic 的
  `cache_control` 打在**稳定的 system 块**和**工具目录的最后一项**上,压缩摘要单独
  成块、不进缓存前缀,所以一次 `/compact` 不会让整段前缀失效。
- 价格来自用户配置的 `[usage.prices]`;没配置就只显示 token。`/cost`、页脚、
  `kazi run` 和 `--output json` 用同一份数字,`agent.max_usd_per_task` 在轮与轮
  之间中断(`AgentError::BudgetExceeded`,历史保留)。

## 13. 为什么不用框架

现有的 agent 框架 (LangChain/rig/Swarm…) 提供的是**包装**,购买的是接入速度,
付出的代价是:层数深了以后,你不知道一次 422 是哪里产生的、流式碎片在哪里被拼坏、
 tool 结果为什么没进历史。kazi 证明这些"平台能力"其实只有 ~3k 行——
协议翻译 (§2)、SSE 解析 (§3)、循环 (§1)、安全门控 (§5)、上下文 (§6)。

当框架的抽象挡住你的时候,读 kazi 的对应部分,然后自己抄/改都不用付学费。
