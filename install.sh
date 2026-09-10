#!/usr/bin/env bash
# lg2 / deepseek-agent 一键安装脚本
# 用法：
#   ./install.sh              # 装到 ~/.local/bin（用户级）
#   sudo ./install.sh         # 装到 /usr/local/bin（全局）
set -euo pipefail

SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_NAME="deepseek-agent"
CMD_NAME="lg2"

# ---- 选择安装目录 ----
if [[ "${EUID:-$(id -u)}" -eq 0 ]]; then
    INSTALL_DIR="/usr/local/bin"
else
    INSTALL_DIR="$HOME/.local/bin"
fi
mkdir -p "$INSTALL_DIR"

say()  { printf '\033[1;32m[install]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[warn]\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m[error]\033[0m %s\n' "$*" >&2; exit 1; }

# ---- 1. 检查 Rust ----
if ! command -v cargo >/dev/null 2>&1; then
    if [[ -f "$HOME/.cargo/env" ]]; then
        # shellcheck disable=SC1091
        source "$HOME/.cargo/env"
    fi
fi
command -v cargo >/dev/null 2>&1 || die "未找到 cargo。请先安装 Rust：
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"

say "cargo: $(cargo --version)"

# ---- 2. 编译 ----
say "编译 release（首次较慢，约 30s+）..."
( cd "$SRC_DIR" && cargo build --release )

BIN_PATH="$SRC_DIR/target/release/$BIN_NAME"
[[ -x "$BIN_PATH" ]] || die "编译产物不存在：$BIN_PATH"

# ---- 3. 安装启动器（wrapper，抗 cargo clean）----
WRAPPER="$INSTALL_DIR/$CMD_NAME"
cat > "$WRAPPER" <<EOF
#!/usr/bin/env bash
# $CMD_NAME — DeepSeek Agent 启动器（由 install.sh 生成）
BIN="$BIN_PATH"
if [[ ! -x "\$BIN" ]]; then
    echo "[$CMD_NAME] 未找到二进制：\$BIN" >&2
    echo "[$CMD_NAME] 请重新构建：cd "$SRC_DIR" && cargo build --release" >&2
    exit 1
fi
exec "\$BIN" "\$@"
EOF
chmod +x "$WRAPPER"
say "已安装启动器：$WRAPPER"

# ---- 4. PATH 提示 ----
if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
    warn "$INSTALL_DIR 不在 PATH 中。请加入 shell 配置："
    echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
fi

# ---- 5. 可选依赖提示（token 提取用 Playwright）----
if command -v python3 >/dev/null 2>&1; then
    if ! python3 -c "import playwright" 2>/dev/null; then
        warn "未安装 Playwright（仅在 token 失效、需从浏览器提取时需要）："
        echo "    pip install playwright && playwright install chromium"
    fi
else
    warn "未找到 python3（token 提取脚本需要它）"
fi

# ---- 6. token 准备提示 ----
echo
say "安装完成 ✅"
echo "  启动：$CMD_NAME"
echo "  首次使用若报 token 错误，可任选其一："
echo "    1) export DS_TOKEN=<你的 userToken>"
echo "    2) 安装 Playwright 并登录 https://chat.deepseek.com（脚本自动提取）"