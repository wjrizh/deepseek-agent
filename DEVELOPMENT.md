好，给你 **DEVELOPMENT.md 的完整修订版**，直接整份覆盖即可。

```markdown
# DeepSeek Agent (Rust) — 开发文档

> 版本 0.2.0 | 2026-09-10 | 纯 Rust 实现的 DeepSeek Agent
> 代码 ~2600 行 | 二进制 13MB | 编译 `cargo build --release`

---

## 一、项目定位

纯 HTTP 实现的 DeepSeek 客户端 + Agent 框架，**无需浏览器点击**即可完成：
对话、多轮上下文、文件上传、token 自动管理、**本地命令执行（PTY 交互）**。
核心是逆向复现了 DeepSeek 网页端的 PoW（工作量证明）算法，使纯脚本调用官方
API 成为可能。

---

## 二、快速开始

```bash
cd /home/z/deepseek-agent
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release

# 单次问答
./target/release/deepseek-agent "你好"

# 带文件问答
./target/release/deepseek-agent "总结这份文档" --file /path/doc.pdf

# 交互式多轮对话
./target/release/deepseek-agent

# 手动指定 token
./target/release/deepseek-agent --token "xxx" "问题"
```

交互模式内置命令：

| 命令 | 作用 |
|---|---|
| `/free` | 自动批准后续所有命令（免确认） |
| `/safe` | 恢复命令确认 |
| `exit` / `quit` / `退出` | 退出 |

---

## 三、架构总览

```
┌─────────────────────────────────────────────┐
│  cli.rs / main.rs      薄入口（参数解析+组装）  │
├─────────────────────────────────────────────┤
│  lib.rs  Runtime       依赖注入容器            │
├──────────────┬──────────────────────────────┤
│  agent/      │  api/                        │
│  ├ core.rs   │  ├ types.rs   Schema         │
│  ├ model.rs  │  ├ file.rs    上传            │
│  ├ memory.rs │  └ chat.rs    会话+对话       │
│  ├ parser.rs │  ← 工具调用解析               │
│  ├ prompt.rs │  ← 系统提示词                 │
│  └ tool.rs   │  ← 工具执行（PTY）            │
├──────────────┴──────────────────────────────┤
│  client/      http.rs / pow.rs / sse.rs      │
├─────────────────────────────────────────────┤
│  ui.rs  终端 UI（logo/思考区/TagFilter）      │
│  auth.rs token 管理   config.rs   error.rs   │
└─────────────────────────────────────────────┘
        ↓ 依赖
   wasm/sha3.wasm    scripts/extract_token.py
```

### 数据流

```
用户输入
  → cli::run
    → Runtime::new（加载 token + 编译 wasm）
    → Agent::run
      → ensure_session（建会话）
      → CompletionReq（含 parent_message_id）
      → Model::complete
        → api::chat::completion
          → make_pow_header（取挑战→wasmi 求解→base64）
          → POST /api/v0/chat/completion（SSE 流）
          → sse::parse_data_line 逐块解析
      → parser::parse（工具调用？）
        ├ Text     → 结束
        ├ Call     → 审批 → PTY 执行 → <tool_result> 回注 → 下一轮
        └ Malformed→ 有界重试 → 降级为文本
      → 更新 parent_message_id + Memory
```

---

## 四、模块详解

| 文件 | 行数 | 职责 | 关键点 |
|---|---|---|---|
| `src/main.rs` | 38 | CLI 入口 | clap 解析，`--file`/`--token`/`--thinking` |
| `src/lib.rs` | 55 | 库入口 + Runtime | 依赖注入，`Runtime::new` async |
| `src/config.rs` | 54 | 配置 | env 覆盖，默认路径 |
| `src/error.rs` | 34 | 错误类型 | `AgentError` + `Result` |
| `src/auth.rs` | 220 | **token 管理** | 三级优先级 + 校验 + 刷新 |
| `src/cli.rs` | ~75 | 交互循环 | 单次/交互双模式，`/free` `/safe` |
| `src/ui.rs` | ~520 | **终端 UI** | logo/信任菜单/思考活区/TagFilter |
| `src/client/http.rs` | 80 | HTTP 封装 | 统一鉴权头 |
| `src/client/pow.rs` | 145 | **PoW 求解** | wasmi + TypedFunc |
| `src/client/sse.rs` | 138 | SSE 解析 | 处理 DeepSeek 特有格式 |
| `src/api/types.rs` | 137 | Schema | serde 请求/响应 |
| `src/api/file.rs` | 108 | 文件上传 | multipart + PoW |
| `src/api/chat.rs` | 137 | 会话+对话 | SSE 流式接收 |
| `src/agent/core.rs` | ~230 | **Agent 主循环** | 工具循环 + 有界重试 + /free |
| `src/agent/model.rs` | 62 | Model trait | LLM 后端抽象 |
| `src/agent/memory.rs` | 46 | Memory trait | 对话历史 |
| `src/agent/parser.rs` | ~750 | **工具解析** | 双格式 + DSML 归一化 + 20 测试 |
| `src/agent/prompt.rs` | ~50 | 系统提示词 | 行为准则 + 工具格式 |
| `src/agent/tool.rs` | ~330 | **工具执行** | PTY 交互 + 流式 + 超时 |

---

## 五、API 接口规范（逆向自网页端）

### 5.1 PoW 挑战

```
POST /api/v0/chat/create_pow_challenge
body: {"target_path": "/api/v0/file/upload_file"}
→ {"data":{"biz_data":{"challenge":{algorithm,challenge,salt,signature,difficulty,expire_at}}}}
```

### 5.2 文件上传

```
POST /api/v0/file/upload_file
Content-Type: multipart/form-data  (字段名: "file")
Headers: x-ds-pow-response, x-file-size
→ {"data":{"biz_data":{"id":"file-xxx","file_name":...,"is_image":...}}}
```

### 5.3 会话创建

```
POST /api/v0/chat_session/create
body: {}
→ {"data":{"biz_data":{"chat_session":{"id":"xxx"}}}}   ← 注意嵌套
```

### 5.4 对话（SSE）

```
POST /api/v0/chat/completion
body: {chat_session_id, parent_message_id, model_type, prompt,
       ref_file_ids, thinking_enabled, search_enabled, action, preempt}
```

**SSE 格式（关键，与常见格式不同）：**
```
event: ready
data: {"response_message_id":2,...}                    ← 下一轮 parent 用它
data: {"v":{"response":{...}}}                          ← 消息对象（跳过）
data: {"p":".../content","o":"APPEND","v":"正文"}       ← 增量
data: {"v":"正文"}                                       ← 增量
data: {"p":"response/status","o":"SET","v":"FINISHED"}  ← 结束（跳过）
event: close
```

### 5.5 PoW 算法（DeepSeekHashV1）

```
prefix = salt + "_" + expire_at + "_"
answer = wasm_solve(challenge, prefix, difficulty)  # wasmi 调用
x-ds-pow-response = base64(JSON{
  algorithm, challenge, salt, answer(i64!), signature, target_path
})
```

**⚠️ 三个易错点：**
1. `answer` 必须是 **整数 i64**，不是 f64（否则 `40301 INVALID_POW_RESPONSE`）
2. `wasm_solve` 第 6 个参数是 **f64**（difficulty 以浮点传入）
3. `validate` 必须解析 body 的 `code`，不能只看 HTTP 200

---

## 六、Token 管理机制

### 三级优先级

```
1. 环境变量 DS_TOKEN          （最高，也持久化一份）
2. 持久化文件 ~/.deepseek-agent/token.json
     → validate() 校验
       → 有效：直接用
       → 无效：往下
3. 浏览器提取（headless 静默）
   scripts/extract_token.py
```

### Token 生命周期

```text
userToken 会变（重登/切换账号/服务端刷新），但非每次
正常启动：1 次轻量校验（~50ms）
失效时：headless 静默提取（~1.25s，复用登录态，无弹窗）
首次/登录态失效：才弹窗要求登录
```

### 持久化位置

```
~/.deepseek-agent/token.json          (0600 权限)
~/.deepseek-agent/browser/            (Chromium profile, 8.7MB, 含登录态)
```

---

## 七、工具系统

### 7.1 调用格式（DeepSeek 原生）

模型输出：

```xml
<tool_calls>
<invoke name="execute_command">
<parameter name="command">ls -la</parameter>
</invoke>
</tool_calls>
```

> 也兼容简易格式 `<execute_command><command>..</command></execute_command>`。

### 7.2 执行流程

```
模型回复 → parser::parse
  Call      → 审批（/free 则跳过）→ PTY 执行 → <tool_result> 回注 → 下一轮
  Malformed → 有界重试（≤2 次）→ 降级为文本
  Text      → 结束
```

### 7.3 关键设计

| 点 | 做法 |
|---|---|
| 单工具 | `execute_command` 走天下（`bash -c`） |
| PTY 执行 | `portable-pty`，命令以为在真终端 → 进度条/颜色/交互 |
| 流式 | 边读边 `on_output` 回调；`strip_ansi` 后喂模型 |
| 交互 | TTY 下 raw mode 转发按键（Ctrl+C 可中断） |
| 审批 | `/free` 自动批准，`/safe` 恢复确认 |
| 防挂起 | `PAGER=cat`、`GIT_PAGER=cat`、`DEBIAN_FRONTEND=noninteractive` |
| 超时 | 非交互 300s 空闲 / 交互 1800s |
| 截断 | 工具输出 ≤8000 字符 |
| 轮数上限 | 单次请求 ≤12 轮工具调用 |

### 7.4 解析鲁棒性（重点）

模型会输出**全角竖线包裹的 DSML 厂商标记**，如 `</｜｜DSML｜｜ parameter>`。

- `parser.rs`：`normalize_tag_prefix()` 剥离 `｜｜DSML｜｜` / `||DSML||`
- `ui.rs`：`TagFilter` 同样归一化后匹配，抑制 XML 显示
- **两处必须同步**（`ui.rs` 复用 `parser::normalize_tag_prefix`）

### 7.5 排查经验（血泪）

| 现象 | 根因 | 修复 |
|---|---|---|
| 参数提取失败 | 闭合标签 `</｜｜DSML｜｜ parameter>` | 归一化 DSML 前缀 |
| 模型说"输出为空" | `collapse_progress` 吃掉 `\r\n` | 先归一 `\r\n`→`\n` |
| XML 泄漏终端 | TagFilter 不认 DSML 前缀 | 同步 `normalize_tag_prefix` |
| 模型重复调同一工具 | 上轮 result 为空 | 修捕获逻辑 |

---

## 八、扩展点（为后续开发预留）

### 8.1 新增 LLM 后端 → 实现 `Model` trait

```rust
// src/agent/model.rs
#[async_trait]
pub trait Model: Send + Sync {
    async fn complete(&self, req: &CompletionReq,
        on_delta: &mut (dyn for<'a> FnMut(DeltaKind, &'a str) + Send)) -> Result<ModelReply>;
    async fn new_session(&self) -> Result<String>;
}
// 现有: DeepSeekModel
// 未来: OpenAIModel / LocalModel / ...
```

### 8.2 新增工具 → 实现 `Tool` trait

```rust
// src/agent/tool.rs
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    async fn call(
        &self,
        params: &HashMap<String, String>,
        live: bool,
        on_output: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<String>;
}
// 步骤：
// 1. 实现 Tool
// 2. 在 parser::KNOWN_TOOLS 登记工具名
// 3. ui.rs 的 TagFilter 自动同步（依赖 KNOWN_TOOLS）
// 4. Agent::new 里 register_tool
```

### 8.3 持久化记忆 → 实现 `Memory` trait

```rust
// src/agent/memory.rs
pub trait Memory: Send + Sync {
    fn push(&mut self, msg: Message);
    fn history(&self) -> &[Message];
    fn clear(&mut self);
}
// 现有: InMemory
// 未来: SqliteMemory / RedisMemory
```

### 8.4 新增 API → 在 `src/api/` 加模块

参考 `file.rs` 的结构：定义 types → 调 `make_pow_header` → 发请求 → 解析 `ApiResponse<T>`。

---

## 九、依赖说明

| crate | 用途 |
|---|---|
| `tokio` | 异步运行时（full） |
| `reqwest` (rustls) | HTTP（json/stream/multipart） |
| `wasmi` | WASM 解释器（跑 sha3.wasm） |
| `serde` / `serde_json` | 序列化 |
| `clap` | CLI 参数 |
| `thiserror` / `anyhow` | 错误处理 |
| `base64` | PoW 头编码 |
| `futures-util` | 流处理 |
| `dirs` | 跨平台路径 |
| `portable-pty` | PTY 交互式执行 |
| `crossterm` | 终端 raw mode / 按键转发 |
| `ratatui` | 思考区 Inline 渲染 |
| `rustyline` | 交互式输入 |
| `unicode-width` | 显示宽度计算 |

**注意**：reqwest 0.13 的 TLS feature 名是 `rustls`（不是 `rustls-tls`）。

---

## 十、构建与测试

```bash
# 构建
cargo build --release          # 13MB

# 静态检查（须 0 警告）
cargo clippy --all-targets

# 测试
cargo test --release           # 21 个（parser 20 + pow 1）

# 单独验证 token
python3 scripts/extract_token.py       # 应 1~2s 输出 token
```

### 性能基线

```
PoW 求解:      0.17s (release) / 29s (debug)
二进制:        13MB
正常启动:      2.08s（含对话）
失效刷新启动:  3.46s（含校验+提取+对话）
```

---

## 十一、故障排查

| 现象 | 原因 | 解决 |
|---|---|---|
| `40301 INVALID_POW_RESPONSE` | answer 是 f64 | 改 i64（`answer.round() as i64`） |
| `wasm_solve 签名不匹配` | 参数类型错 | 第 6 个是 f64 |
| `missing field id` | 会话响应嵌套 | 用 `ChatSessionWrapper` |
| 响应体非 SSE | 业务错误 | 解析 `code`，检查 PoW/token |
| 假 token 未被拦截 | validate 只看 HTTP | 必须解析 body 的 `code==0` |
| 每次弹浏览器 | 脚本 headless=False | 先 headless 试，失败才弹窗 |
| 参数解析失败 | DSML 全角标记 | `normalize_tag_prefix` |
| 命令输出丢失 | `\r\n` 被吃 | `collapse_progress` 先归一 |
| XML 显示出来 | TagFilter 旧版 | 同步 parser 的归一化 |
| 交互按键无反应 | stdin 转发未启动 | 确认 `live=true` |

---

## 十二、后续开发路线

- [x] 工具系统落地（execute_command + PTY + /free）
- [ ] 更多工具（read_file / write_file / search）
- [ ] 持久化记忆（SQLite）
- [ ] 流式输出可选关闭（`--no-stream`）
- [ ] 配置文件支持（`~/.deepseek-agent/config.toml`）
- [ ] 多模型后端（OpenAI 兼容接口）
- [ ] 会话历史导出/导入

---

## 附：文件清单

```
deepseek-agent/
├── Cargo.toml / Cargo.lock
├── DEVELOPMENT.md          ← 本文档
├── HANDOFF.md              交接文档
├── wasm/sha3.wasm          PoW 核心（勿删）
├── scripts/extract_token.py token 提取
├── tests/pow_test.rs       PoW 回归测试
└── src/                    19 个源文件，见第四节
```
```

---

**改动要点**（相对旧版）：

1. 版本 → `0.2.0`，行数/二进制更新
2. 架构图补 `parser.rs` / `prompt.rs` / `ui.rs`
3. 数据流补工具循环分支
4. 模块表更新行数 + 新增 4 行（parser/prompt/ui/tool）
5. **新增第七节「工具系统」**（格式/流程/设计/鲁棒性/排查）
6. 扩展点 `Tool` 签名更新 + 新增工具步骤
7. 依赖表补 `portable-pty` 等
8. 故障排查补 4 条
9. 路线图勾选工具系统
