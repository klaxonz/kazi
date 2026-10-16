# 版本与发布规则

kazi 处于 `0.x` 阶段,采用「pre-1.0 语义化版本」:
**MINOR 位暴露新功能,PATCH 位只装修复,MAJOR 位置冻结到 1.0**。

## 判定表(以一次提交批次里「最大」的内容为准)

| 内容                                   | 版本动作        |
|----------------------------------------|-----------------|
| 纯 bug 修复(不改 CLI/配置/库可见行为)  | PATCH:0.x.Y+1  |
| 只有文档/CHANGELOG/注释/测试            | 不升版本,记 Unreleased |
| 新功能:工具、CLI 命令/flag、配置段、库 API 向后兼容 | MINOR:0.X+1.0 |
| 破坏性变更(配置格式不兼容、CLI 语义变化、库公开 API 修改) | 仍旧 MINOR +1,但 CHANGELOG 里单独标 `**BREAKING**`;跳 MAJOR 仅发生在 1.0 之后 |
| 一次批次里既有修复又有功能               | 统一按 MINOR 升级,CHANGELOG 的 Added/Fixed 分节写清——**patch 不得夹带新功能** |

## 明确的例子(对本仓库既往的校准)

- `apply_patch` + `--output json` → **0.1.1**(功能批,MINOR 粒度用语义判断而非随手):
  单批只有一个 feature 集合且影响面小,允许 MINOR+?否——规则生效后,同类批次归 MINOR。
  此例历史已定 0.1.1,保留不改,今后按表执行。
- 运行次数固化的参考线:`0.1.x` 引擎与工具、`0.2.x` MCP/并行、`0.3.x` 远程 MCP、
  `0.4.x` 可靠性与会话内容、`0.5.x` 会话体验……依此类推,MINOR 号代表「主题增量」。
- REPL 斜杠命令失效 → **0.4.1**(纯 PATCH,规则正例)。

## 发布流程(每次打 tag 前,机械执行,不得跳步)

1. CHANGELOG:把 Unreleased 移入新版本号小节(Keep a Changelog 词汇)。
2. `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`(全绿)。
3. `cargo package --no-verify`(0 警告)。
4. `git commit` + `git tag -a vX.Y.Z`。
5. 用户侧 `cargo install -q --path .`(本机 CLI 换版)。

## 版本号焦虑的解药

拿不准升多少时,**永远选 MINOR**;宁可版本号显得「宽松」也不要把新功能偷塞进
PATCH——PATCH 是承诺「行为不变」的护栏,也是排查回归时的最低成本回退点。
