# kazi 架构与原理

> 这份文档回答一个的问题:**一个 AI Agent 到底是什么,它是怎么跑起来的**。
> kazi 的全部实现 (~3k 行 Rust) 都为了把答案讲清楚,不用任何 agent 框架——
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

内置工具 (`tools/builtin()`):`list_dir` / `read_file` (行号分页+二进制拒绝) /
`write_file` / `run_command` (§8) / `fetch_url` (§8)。

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
存 `system + messages`。`--resume <name>` 恢复;REPL 每轮任务后自动保存 (best-effort,
失败只提示不打断)。

## 8. fetch_url 的网络边界

- 只允许 https;localhost/回环/私网 IP (RFC1918/5737/6598…) 一律拒绝 (基础 SSRF 防护)
- 1 MiB 字节上限 + 30 秒墙钟上限,双保险读流
- HTML 粗蜕壳成文本 (剥 script/style + 标签),模型消费的是文字不是浏览器

## 9. 添加一个新工具 (30 行以内)

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

## 10. 为什么不用框架

现有的 agent 框架 (LangChain/rig/Swarm…) 提供的是**包装**,购买的是接入速度,
付出的代价是:层数深了以后,你不知道一次 422 是哪里产生的、流式碎片在哪里被拼坏、
 tool 结果为什么没进历史。kazi 证明这些"平台能力"其实只有 ~3k 行——
协议翻译 (§2)、SSE 解析 (§3)、循环 (§1)、安全门控 (§5)、上下文 (§6)。

当框架的抽象挡住你的时候,读 kazi 的对应部分,然后自己抄/改都不用付学费。
