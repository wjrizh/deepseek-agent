# deepseek-agent

纯 Rust 实现的 DeepSeek 命令行 Agent。无需浏览器点击，通过逆向复现网页端
PoW（工作量证明）算法直接调用官方 API，支持：

- 多轮对话（SSE 流式）
- 会话持久化 + 多会话切换（`/sessions` `/switch` `/new`）
- 文件上传 + 基于文件问答
- 本地命令执行（PTY 交互，`execute_command` 工具）
- token 自动管理（三级优先级 + 静默刷新）

## 安装

```bash
git clone git@github.com:wjrizh/deepseek-agent.git
cd deepseek-agent
./install.sh
```

默认安装到 `~/.local/bin/lg2`（用户级）。若用 `sudo ./install.sh`，
则装到 `/usr/local/bin/lg2`（全局）。

> 首次运行若 `~/.local/bin` 不在 PATH，按脚本提示加入 shell 配置。

## 用法

```bash
lg2                      # 交互模式
lg2 "你好"               # 单次问答
lg2 --thinking "9.11 和 9.9 哪个大"
lg2 "总结这份文档" --file doc.pdf
```

交互模式命令：

| 命令 | 作用 |
|---|---|
| `/help` | 显示帮助 |
| `/new` | 开新会话 |
| `/sessions` | 列出历史会话 |
| `/switch <id前缀>` | 切换会话 |
| `/delete <id...>` | 删除会话 |
| `/free` | 自动批准命令（免确认） |
| `/safe` | 恢复命令确认 |

Tab 可补全命令与会话 id。

## 依赖

- **Rust**（cargo）：`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- **Python3 + Playwright**（可选，仅 token 失效时从浏览器自动提取）：

  ```bash
  pip install playwright && playwright install chromium
  ```

## Token

首次使用需提供 token，任选其一：

1. `export DS_TOKEN=<你的 userToken>`
2. 安装 Playwright 并登录 https://chat.deepseek.com，脚本会自动提取并缓存到
   `~/.deepseek-agent/token.json`

## 开发

详见 [DEVELOPMENT.md](DEVELOPMENT.md)（改代码前必读）与 [HANDOFF.md](HANDOFF.md)。

```bash
cargo build --release
cargo test --release
cargo clippy --all-targets
```