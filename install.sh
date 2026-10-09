#!/bin/bash

set -e

echo "=== 安装阿里云DDNS ==="

# 以原始用户编译：sudo 下 root 通常没有 cargo，且避免 root 污染 target/ 属主
build_release() {
    local cargo_bin home_dir
    cargo_bin="$(command -v cargo 2>/dev/null || true)"
    if [ -z "$cargo_bin" ] && [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != "root" ]; then
        home_dir="$(eval echo "~$SUDO_USER")"
        [ -x "$home_dir/.cargo/bin/cargo" ] && cargo_bin="$home_dir/.cargo/bin/cargo"
    fi
    if [ -z "$cargo_bin" ]; then
        echo "错误: 找不到 cargo，请先安装 Rust (https://rustup.rs)"
        exit 1
    fi
    echo "编译中..."
    if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != "root" ]; then
        home_dir="$(eval echo "~$SUDO_USER")"
        sudo -u "$SUDO_USER" env HOME="$home_dir" "$cargo_bin" build --release
    else
        "$cargo_bin" build --release
    fi
}

case "$(uname -s)" in
Linux)
    if [ "$(id -u)" -ne 0 ]; then
        echo "错误: Linux 请使用 sudo ./install.sh"
        exit 1
    fi

    INSTALL_DIR="/usr/local/bin"
    CONFIG_DIR="/etc/alidns-ddns"
    SERVICE_FILE="/etc/systemd/system/alidns-ddns.service"

    build_release
    upx --best target/release/alidns-ddns 2>/dev/null || true

    echo "创建配置目录..."
    mkdir -p "$CONFIG_DIR"

    echo "安装二进制文件..."
    cp target/release/alidns-ddns "$INSTALL_DIR/"

    if [ ! -f "$CONFIG_DIR/config.json" ]; then
        cp config.example.json "$CONFIG_DIR/config.json"
        # 服务以 DynamicUser 的临时 UID 运行，config.json 必须 world-readable
        # 才能被读取；真实 AccessKey 请放 /etc/alidns-ddns/env（600），勿写入此文件
        chmod 644 "$CONFIG_DIR/config.json"
        echo "已复制配置文件到 $CONFIG_DIR/config.json"
    fi

    if [ ! -f "$CONFIG_DIR/env" ]; then
        cp env.example "$CONFIG_DIR/env"
        chmod 600 "$CONFIG_DIR/env"
        echo "请编辑 $CONFIG_DIR/env 填入你的AccessKey"
    fi

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
    ;;
Darwin)
    if [ "$(id -u)" -eq 0 ]; then
        # --- 系统级 LaunchDaemon（推荐：开机即启，不依赖图形登录/SSH 登录） ---
        RUN_USER="${SUDO_USER:-root}"
        INSTALL_DIR="/usr/local/bin"
        CONFIG_DIR="/etc/alidns-ddns"
        LOG_DIR="/usr/local/var/log"
        PLIST_FILE="/Library/LaunchDaemons/com.tongtf.alidns-ddns.plist"
        LABEL="system/com.tongtf.alidns-ddns"

        build_release

        echo "创建配置目录..."
        mkdir -p "$INSTALL_DIR" "$CONFIG_DIR" "$LOG_DIR"

        echo "安装二进制文件..."
        cp target/release/alidns-ddns "$INSTALL_DIR/"

        if [ ! -f "$CONFIG_DIR/config.json" ]; then
            cp config.example.json "$CONFIG_DIR/config.json"
            echo "已复制配置文件到 $CONFIG_DIR/config.json"
        fi
        if [ "$RUN_USER" != "root" ]; then
            chown "$RUN_USER" "$CONFIG_DIR/config.json"
        fi
        chmod 600 "$CONFIG_DIR/config.json"

        # 卸载可能残留的用户级 agent，避免与系统级服务双实例运行
        if [ "$RUN_USER" != "root" ]; then
            run_uid="$(id -u "$RUN_USER")"
            launchctl bootout "gui/$run_uid/com.tongtf.alidns-ddns" 2>/dev/null || true
            launchctl bootout "user/$run_uid/com.tongtf.alidns-ddns" 2>/dev/null || true
            rm -f "$(eval echo "~$RUN_USER")/Library/LaunchAgents/com.tongtf.alidns-ddns.plist"
        fi

        echo "安装launchd服务..."
        sed -e "s|@INSTALL_DIR@|$INSTALL_DIR|g" \
            -e "s|@CONFIG_DIR@|$CONFIG_DIR|g" \
            -e "s|@LOG_DIR@|$LOG_DIR|g" \
            -e "s|@USERNAME@|$RUN_USER|g" \
            alidns-ddns-daemon.plist > "$PLIST_FILE"
        chown root:wheel "$PLIST_FILE"
        chmod 644 "$PLIST_FILE"

        launchctl bootout "$LABEL" 2>/dev/null || true
        launchctl bootstrap system "$PLIST_FILE"
        launchctl enable "$LABEL" 2>/dev/null || true

        echo ""
        echo "=== 安装完成（系统级服务，开机自启） ==="
        echo ""
        echo "使用方法:"
        echo "  1. 编辑配置: sudo vim $CONFIG_DIR/config.json"
        echo "  2. 重启服务: sudo launchctl kickstart -k $LABEL"
        echo "  3. 查看日志: tail -f $LOG_DIR/alidns-ddns.log"
        echo "  4. 卸载服务: sudo launchctl bootout $LABEL && sudo rm $PLIST_FILE"
    else
        # --- 用户级 LaunchAgent（免 sudo；SSH 下也可安装） ---
        if [ -f /Library/LaunchDaemons/com.tongtf.alidns-ddns.plist ]; then
            echo "错误: 检测到已安装系统级服务，用户级安装会造成双实例。"
            echo "请使用 sudo ./install.sh 覆盖安装。"
            exit 1
        fi

        INSTALL_DIR="$HOME/.local/bin"
        CONFIG_DIR="$HOME/.config/alidns-ddns"
        LOG_DIR="$HOME/Library/Logs"
        LAUNCH_AGENTS="$HOME/Library/LaunchAgents"
        PLIST_FILE="$LAUNCH_AGENTS/com.tongtf.alidns-ddns.plist"

        # 优先加载到 gui 域（图形登录会话）；SSH / 无图形会话时回退 user 域
        if launchctl print "gui/$(id -u)" >/dev/null 2>&1; then
            DOMAIN="gui/$(id -u)"
        else
            DOMAIN="user/$(id -u)"
            echo "提示: 当前无图形会话，服务将加载到 user 域"
        fi
        LABEL="$DOMAIN/com.tongtf.alidns-ddns"

        build_release

        echo "创建配置目录..."
        mkdir -p "$INSTALL_DIR" "$CONFIG_DIR" "$LOG_DIR" "$LAUNCH_AGENTS"

        echo "安装二进制文件..."
        cp target/release/alidns-ddns "$INSTALL_DIR/"

        if [ ! -f "$CONFIG_DIR/config.json" ]; then
            cp config.example.json "$CONFIG_DIR/config.json"
            chmod 600 "$CONFIG_DIR/config.json"
            echo "已复制配置文件到 $CONFIG_DIR/config.json"
        fi

        echo "安装launchd服务..."
        sed -e "s|@INSTALL_DIR@|$INSTALL_DIR|g" \
            -e "s|@CONFIG_DIR@|$CONFIG_DIR|g" \
            -e "s|@LOG_DIR@|$LOG_DIR|g" \
            alidns-ddns.plist > "$PLIST_FILE"

        # 重复安装时先卸载再加载（gui/user 两域都尝试，避免残留实例）
        launchctl bootout "gui/$(id -u)/com.tongtf.alidns-ddns" 2>/dev/null || true
        launchctl bootout "user/$(id -u)/com.tongtf.alidns-ddns" 2>/dev/null || true
        launchctl bootstrap "$DOMAIN" "$PLIST_FILE"
        launchctl enable "$LABEL" 2>/dev/null || true

        echo ""
        echo "=== 安装完成（用户级服务） ==="
        echo ""
        echo "使用方法:"
        echo "  1. 编辑配置: vim $CONFIG_DIR/config.json"
        echo "  2. 重启服务: launchctl kickstart -k $LABEL"
        echo "  3. 查看日志: tail -f $LOG_DIR/alidns-ddns.log"
        echo "  4. 卸载服务: launchctl bootout $LABEL && rm $PLIST_FILE"
        echo "说明: 用户级服务在图形登录后自动加载；纯 SSH 场景建议用 sudo ./install.sh"
    fi
    ;;
*)
    echo "错误: 不支持的操作系统 $(uname -s)（仅支持 Linux / macOS）"
    exit 1
    ;;
esac
