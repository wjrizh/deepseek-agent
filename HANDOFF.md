# DeepSeek Agent (Rust) — 交接文档

> 用于在新对话中继续开发。请先读这份，再看 `DEVELOPMENT.md`。

---

## 一、项目位置与构建

```bash
cd /home/z/deepseek-agent
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release        # 12MB
cargo test --release         # PoW + SSE 解析测试
./target/release/deepseek-agent [--thinking] ["问题"] [--file 文件]
```

- 语言：Rust edition 2024，1827 行
- 依赖：tokio / reqwest(rustls) / wasmi / serde / clap / crossterm / ratatui / rustyline / unicode-width
- token：`~/.deepseek-agent/token.json`（0600），浏览器登录态 `~/.deepseek-agent/browser/`

---

## 二、当前功能状态

| 功能 | 状态 |
|---|---|
| 对话（SSE 流式） | ✅ 可用 |
| 多轮上下文（parent_message_id） | ✅ 可用 |
| 文件上传 + 基于文件问答 | ✅ 可用 |
| token 三级优先级 + 校验 + 静默刷新 | ✅ 可用 |
| PoW 求解（wasmi + sha3.wasm） | ✅ 可用，测试通过 |
| 思考/答案区分（THINK/RESPONSE） | ✅ 解析正确 |
| 启动 logo + 信任菜单 | ✅ 可用 |
| **终端流式 UI（思考区 + 答案区）** | ⚠️ **未完成，见第四节** |

---

## 三、已确认的技术事实（逆向所得，勿重复踩坑）

### 3.1 DeepSeek API

```
建会话:  POST /api/v0/chat_session/create  {}  → biz_data.chat_session.id（嵌套！）
PoW:     POST /api/v0/chat/create_pow_challenge  {target_path}
上传:    POST /api/v0/file/upload_file  multipart 字段名 "file"
对话:    POST /api/v0/chat/completion  （SSE）
```

请求头：`Authorization: Bearer <userToken>`，`x-ds-pow-response` 等。

### 3.2 PoW（DeepSeekHashV1）

```
prefix = salt + "_" + expire_at + "_"
answer = wasm_solve(challenge, prefix, difficulty)   // wasmi 调 wasm/sha3.wasm
x-ds-pow-response = base64(JSON{algorithm,challenge,salt,answer,signature,target_path})
```

**三个致命易错点（已解决）**：
1. `answer` 必须是 **i64**（不能 f64，否则 40301 INVALID_POW_RESPONSE）
2. `wasm_solve` 第 6 参数是 **f64**（difficulty 以浮点传入）
3. `validate` 必须解析 body 的 `code`，HTTP 200 不代表有效

### 3.3 SSE 流格式（关键）

```
event: ready
data: {"response_message_id":2,...}                        ← 下一轮 parent
data: {"v":{"response":{"fragments":[{"type":"THINK",...}]}}}   ← 初始对象
data: {"p":"response/fragments","o":"APPEND","v":[{"type":"RESPONSE","content":"首个字"}]}  ← 新 fragment（注意自带 content！）
data: {"p":"response/fragments/-1/content","o":"APPEND","v":"追加"}    ← 追加
data: {"v":"追加"}                                          ← 追加（当前 fragment）
data: {"p":"response/status","o":"SET","v":"FINISHED"}      ← 状态（跳过）
```

**THINK/RESPONSE 区分机制**：维护"当前 fragment 类型"，由 `fragments` 数组的 `type` 字段决定。
- `SseParser`（`src/client/sse.rs`）已正确实现，测试通过
- `ChatEvent::Delta(DeltaKind, String)`，`DeltaKind::{Thinking, Answer}`

**model_type 取值**：`default` / `expert` / `vision`（**没有 deepseek-reasoner**）。
带思考要 `model_type=expert` + `thinking_enabled=true`。

---

## 四、当前卡点（需要新对话解决）

### 目标

用户要的终端效果：

```
❯ 用户输入问题
（思考过程：暗灰色，在"思考区"实时滚动，可超长）
（思考结束 → 思考区消失）
ligong > 答案逐字流式输出（像普通 print 一样，不套框、不突然弹出）
❯ 下一个问题
```

### 现状

`src/ui.rs` 的 `LiveUi`（ratatui `Viewport::Inline(12)`）：
- ✅ 思考区渲染正常（暗灰、滚动）
- ❌ **答案输出问题**：答案流式时被限制在固定框里，或突然消失后从头完整弹出

### 已尝试的方案

1. 备用屏幕（`EnterAlternateScreen`）→ 切新全屏，不符预期，已弃
2. 行号回退清除 → 长思考失效，已弃
3. ratatui `Viewport::Inline` → 思考 OK，答案输出不对（当前）
4. 想做的：**思考用活区，答案一出现就释放 ratatui、回归普通 `print!` 流式**

### 关键代码（`src/ui.rs` 当前 LiveUi）

```rust
pub struct LiveUi {
    terminal: Option<Terminal<CrosstermBackend<std::io::Stdout>>>,  // 思考阶段 Some
    thinking: String,
    answer_started: bool,
    tick: usize,
}

// 答案首个字符：
DeltaKind::Answer => {
    if !self.answer_started {
        self.answer_started = true;
        self.end_thinking();   // 想在这里释放 ratatui 活区
    }
    print!("{text}");          // 回归普通流式
    let _ = std::io::stdout().flush();
}

fn end_thinking(&mut self) {
    if let Some(mut t) = self.terminal.take() {
        let _ = t.clear();
        let _ = std::io::stdout().flush();
        drop(t);
    }
    print!("\x1b[1;32mligong > \x1b[0m");
    let _ = std::io::stdout().flush();
}
```

### 问题现象（用户反馈）

- 答案和思考之间隔了 12 行空白，或答案位置错乱
- ratatui 的 Inline viewport 在 drop 时**没有**把光标复位到活区顶部，导致后续 `print!` 位置不对

### 待查方向

1. **ratatui Inline viewport 的正确收尾**：`terminal.clear()` + drop 后，光标到底在哪？是否需要手动 `MoveTo` 到活区顶部？
   - 官方 inline 示例：https://ratatui.rs/examples/apps/inline/
   - 它用 `terminal.insert_before()` 输出，**从不用裸 `print!` 混合** —— 这可能是关键
2. **是否应该全程用 `insert_before`**：把思考也当作临时内容，答案用 `insert_before` 逐行推入
3. **是否放弃 ratatui**：改用纯 ANSI 手绘固定区域（DECSTBM 滚动区 + 手动行管理），完全可控
4. **参考实现**：Claude Code / Codex CLI / aider 的终端流式 UI 是怎么做的（搜索其开源代码）

### 备选方案（如果 ratatui 搞不定）

用 ANSI 的 **DECSTBM**（设置滚动区域）+ 光标控制：
- 底部 N 行设为"思考区"，用 `\x1b[{top};{bottom}r` 限制
- 思考直接在该区滚动
- 答案开始时清除该区，用普通输出

或**最简单**：思考时用一个独立子区域，答案阶段完全不用 TUI，直接 `println!`。

---

## 五、文件职责

| 文件 | 职责 | 状态 |
|---|---|---|
| `src/main.rs` | CLI 入口（clap） | ✅ |
| `src/cli.rs` | 交互循环（rustyline 输入） | ✅ |
| `src/ui.rs` | **终端 UI（logo/菜单/LiveUi）** | ⚠️ 卡点在此 |
| `src/auth.rs` | token 管理 | ✅ |
| `src/config.rs` | 配置 | ✅ |
| `src/error.rs` | 错误类型 | ✅ |
| `src/client/http.rs` | HTTP 封装 | ✅ |
| `src/client/pow.rs` | PoW（wasmi） | ✅ 测试通过 |
| `src/client/sse.rs` | SSE 解析（THINK/RESPONSE） | ✅ 测试通过 |
| `src/api/types.rs` | Schema（含 DeltaKind） | ✅ |
| `src/api/file.rs` | 文件上传 | ✅ |
| `src/api/chat.rs` | 会话+对话 | ✅ |
| `src/agent/core.rs` | Agent 主循环 | ✅ |
| `src/agent/model.rs` | Model trait | ✅ |
| `src/agent/memory.rs` | Memory trait | ✅ |
| `src/agent/tool.rs` | Tool trait（预留） | ✅ |
| `scripts/extract_token.py` | 浏览器提取 token | ✅ |
| `wasm/sha3.wasm` | PoW 核心（勿删） | ✅ |

---

## 六、测试命令

```bash
# 单元测试（PoW + SSE）
cargo test --release

# 非 TTY 验证内容正确性（思考+答案原文都能看到）
echo "9.11和9.9哪个大？" | DS_TOKEN="$TOKEN" ./target/release/deepseek-agent --thinking

# TTY 验证 UI（要看效果必须用真终端）
./target/release/deepseek-agent --thinking

# token 提取测试（应 1~2s 静默返回）
python3 scripts/extract_token.py
```

---

## 七、给新对话的开场建议

```
我在开发一个 Rust 写的 DeepSeek Agent（/home/z/deepseek-agent），
用 ratatui Viewport::Inline 做终端流式 UI。

目标：
- 思考（THINK）在"活区"暗灰实时滚动
- 思考结束后活区消失，答案（RESPONSE）回归普通 print! 流式输出

当前问题：答案输出位置错乱/被框住/突然弹出。

请阅读 /home/z/deepseek-agent/HANDOFF.md 和 src/ui.rs，
研究 ratatui Inline viewport 的正确收尾方式（或给出替代方案），
让答案能像普通程序一样流式打印到终端。
```

---

## 八、未做的功能（后续）

- 工具系统（`Tool` trait 已预留，主循环检测点已留）
- 持久化记忆（`Memory` trait 已预留）
- 会话历史导出
- 多模型后端（`Model` trait 已预留）