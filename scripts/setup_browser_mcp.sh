#!/usr/bin/env bash
# 安装 @playwright/mcp 到 ~/.ligong-mcp，供 deepseek-agent 的 browser 工具使用。
set -euo pipefail

MCP_DIR="${HOME}/.ligong-mcp"
PKG="@playwright/mcp"

echo "[1/3] 安装目录: ${MCP_DIR}"
mkdir -p "${MCP_DIR}"
cd "${MCP_DIR}"

if [ ! -f package.json ]; then
  echo "初始化 package.json"
  npm init -y >/dev/null 2>&1
fi

echo "[2/3] 安装 ${PKG} ..."
npm install "${PKG}" --no-audit --no-fund

CLI="${MCP_DIR}/node_modules/@playwright/mcp/cli.js"
if [ ! -f "${CLI}" ]; then
  echo "错误: 未找到 ${CLI}" >&2
  exit 1
fi
echo "  cli.js: ${CLI}"

echo "[3/3] 检查 Chromium ..."
CHROME=""
for d in "${HOME}"/.cache/ms-playwright/chromium-*; do
  if [ -x "${d}/chrome-linux64/chrome" ]; then
    CHROME="${d}/chrome-linux64/chrome"
  elif [ -x "${d}/chrome-linux/headless_shell" ]; then
    CHROME="${d}/chrome-linux/headless_shell"
  fi
done

if [ -z "${CHROME}" ]; then
  echo "  未在 ms-playwright 缓存中找到 Chromium，尝试 npx playwright install chromium"
  npx --yes playwright install chromium
  for d in "${HOME}"/.cache/ms-playwright/chromium-*; do
    if [ -x "${d}/chrome-linux64/chrome" ]; then
      CHROME="${d}/chrome-linux64/chrome"
    elif [ -x "${d}/chrome-linux/headless_shell" ]; then
      CHROME="${d}/chrome-linux/headless_shell"
    fi
  done
fi

if [ -z "${CHROME}" ]; then
  echo "警告: 仍未找到 Chromium，请设置 LIGONG_CHROMIUM 环境变量指向可执行文件。" >&2
else
  echo "  chromium: ${CHROME}"
fi

echo ""
echo "完成。deepseek-agent 将自动在 ${MCP_DIR} 下查找 @playwright/mcp。"
echo "可用环境变量覆盖:"
echo "  LIGONG_MCP_CLI            - cli.js 路径"
echo "  LIGONG_CHROMIUM          - Chromium 可执行文件路径"
echo "  LIGONG_BROWSER_HEADLESS  - 1/0 强制无头/有头（默认: 有显示则可见）"
echo "  LIGONG_BROWSER_PROFILE   - 持久化 profile 目录（默认 ~/.ligong-mcp/profile）"
echo "  LIGONG_BROWSER_EPHEMERAL - 1 关闭持久化（改用内存隔离）"
echo "  LIGONG_BROWSER_OUTPUT_DIR - MCP 输出目录（默认 \$TMPDIR/ligong-browser-mcp）"
