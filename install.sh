#!/bin/bash

set -e

INSTALL_DIR="/usr/local/bin"
CONFIG_DIR="/etc/alidns-ddns"
SERVICE_FILE="/etc/systemd/system/alidns-ddns.service"

echo "=== 安装阿里云DDNS ==="

# 编译
echo "编译中..."
cargo build --release
upx --best target/release/alidns-ddns 2>/dev/null || true

# 创建配置目录
echo "创建配置目录..."
mkdir -p "$CONFIG_DIR"

# 复制二进制文件
echo "安装二进制文件..."
cp target/release/alidns-ddns "$INSTALL_DIR/"

# 复制配置文件
if [ ! -f "$CONFIG_DIR/config.json" ]; then
    cp config.json "$CONFIG_DIR/"
    echo "已复制配置文件到 $CONFIG_DIR/config.json"
fi

if [ ! -f "$CONFIG_DIR/env" ]; then
    cp env.example "$CONFIG_DIR/env"
    echo "请编辑 $CONFIG_DIR/env 填入你的AccessKey"
fi

# 安装服务
echo "安装systemd服务..."
cp alidns-ddns.service "$SERVICE_FILE"
systemctl daemon-reload

echo ""
echo "=== 安装完成 ==="
echo ""
echo "使用方法:"
echo "  1. 编辑配置: sudo vim $CONFIG_DIR/env"
echo "  2. 启动服务: sudo systemctl start alidns-ddns"
echo "  3. 开机自启: sudo systemctl enable alidns-ddns"
echo "  4. 查看日志: sudo journalctl -u alidns-ddns -f"
