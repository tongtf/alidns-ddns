#!/bin/sh
# 环境适配安装检测：按当前系统生成最优配置与启动脚本（不安装、不启动、不写密钥）。
#
# 用法：
#   ./adapt-install.sh [--output DIR] [--force] [--dry-run]
#
# 默认输出到 ./deploy。生成内容：
#   config.json              不含真实 AK/SK，需自行填写
#   env                      非敏感设置（600）
#   alidns-ddns.service      Linux/systemd：基于仓库模板生成
#   run-alidns-ddns.sh       Linux/无 systemd：前台运行脚本
#   com.tongtf.alidns-ddns.plist  macOS：launchd 模板已替换路径
#   README.generated.txt     按检测结果给出的填写/安装步骤

set -u

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
OUTPUT_DIR="$SCRIPT_DIR/deploy"
FORCE=no
DRY_RUN=no

while [ "$#" -gt 0 ]; do
    case "$1" in
        --output)
            [ "$#" -ge 2 ] || { echo "错误: --output 缺少目录" >&2; exit 2; }
            OUTPUT_DIR=$2
            shift 2
            ;;
        --force)
            FORCE=yes
            shift
            ;;
        --dry-run)
            DRY_RUN=yes
            shift
            ;;
        -h|--help)
            sed -n '2,12p' "$0"
            exit 0
            ;;
        *)
            echo "未知参数: $1" >&2
            exit 2
            ;;
    esac
done

info() { printf '%s\n' "$*"; }
warn() { printf '警告: %s\n' "$*" >&2; }

# ---------- 环境检测 ----------

OS=$(uname -s)
ARCH=$(uname -m)
SYSTEMD=no
SYSTEMD_VER=0
CONTAINER=no
HAS_IPV6=no
CREDS_TOOL=no

if [ "$OS" = "Linux" ]; then
    if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; then
        SYSTEMD=yes
        SYSTEMD_VER=$(systemctl --version 2>/dev/null | awk 'NR==1 {print $2}')
        case "$SYSTEMD_VER" in
            ''|*[!0-9]*) SYSTEMD_VER=0 ;;
        esac
    fi
    if [ -f /.dockerenv ] || grep -Eq 'docker|containerd|kubepods|lxc' /proc/1/cgroup 2>/dev/null; then
        CONTAINER=yes
    fi
    if command -v ip >/dev/null 2>&1 && ip -6 addr show scope global 2>/dev/null | grep -q inet6; then
        HAS_IPV6=yes
    elif grep -qE '^[^0].*' /proc/net/if_inet6 2>/dev/null; then
        HAS_IPV6=yes
    fi
    command -v systemd-creds >/dev/null 2>&1 && CREDS_TOOL=yes
elif [ "$OS" = "Darwin" ]; then
    if ifconfig -a 2>/dev/null | grep 'inet6 ' | grep -qv 'fe80::'; then
        HAS_IPV6=yes
    fi
else
    warn "未识别的操作系统 $OS，按无 systemd 的独立脚本方案生成"
fi

# ---------- 方案决策 ----------

PLAN=standalone
CRED_MODE=envfile
EXTRA_ARGS=""
STATE_FILE=""

setup_launchd_paths() {
    if [ "$(id -u)" -eq 0 ]; then
        INSTALL_DIR=/usr/local/bin
        CONFIG_DIR=/etc/alidns-ddns
        LOG_DIR=/usr/local/var/log
        TARGET_USER=${SUDO_USER:-$(id -un)}
        USER_HOME=$(eval echo "~$TARGET_USER")
        STATE_FILE="$USER_HOME/.local/state/alidns-ddns/state.json"
        PLIST_SRC="$SCRIPT_DIR/alidns-ddns-daemon.plist"
    else
        INSTALL_DIR="$HOME/.local/bin"
        CONFIG_DIR="$HOME/.config/alidns-ddns"
        LOG_DIR="$HOME/Library/Logs"
        STATE_FILE="$HOME/.local/state/alidns-ddns/state.json"
        PLIST_SRC="$SCRIPT_DIR/alidns-ddns.plist"
    fi
}

if [ "$OS" = "Darwin" ]; then
    PLAN=launchd
elif [ "$SYSTEMD" = "yes" ]; then
    PLAN=systemd
    if [ "$SYSTEMD_VER" -ge 247 ]; then
        CRED_MODE=loadcredential
        if [ "$CREDS_TOOL" = yes ] && [ "$SYSTEMD_VER" -ge 250 ]; then
            CRED_MODE=encrypted
        fi
    else
        CRED_MODE=envfile
        warn "systemd $SYSTEMD_VER 较旧，改用 EnvironmentFile；状态目录需显式指定"
        STATE_FILE="/var/lib/alidns-ddns/state.json"
    fi
else
    PLAN=standalone
    STATE_FILE="${XDG_STATE_HOME:-$HOME/.local/state}/alidns-ddns/state.json"
fi

# 允许显式覆盖检测结果（用于测试/特殊环境）
[ -n "${ADAPT_FORCE_PLAN:-}" ] && PLAN=$ADAPT_FORCE_PLAN
[ -n "${ADAPT_FORCE_CRED:-}" ] && CRED_MODE=$ADAPT_FORCE_CRED
[ -n "${ADAPT_FORCE_CONTAINER:-}" ] && CONTAINER=$ADAPT_FORCE_CONTAINER
[ -n "${ADAPT_FORCE_IPV6:-}" ] && HAS_IPV6=$ADAPT_FORCE_IPV6
[ "$PLAN" = launchd ] && setup_launchd_paths
# 非 systemd 平台没有 LoadCredential 机制
[ "$PLAN" != systemd ] && CRED_MODE=envfile

# 容器里 netlink 多播常被禁；事件订阅虽已有降级，安装时直接关掉更省心
if [ "$CONTAINER" = yes ] && [ "$PLAN" != launchd ]; then
    EXTRA_ARGS="--no-events"
fi

if [ "$HAS_IPV6" = yes ]; then
    IPV_MODE=46
else
    IPV_MODE=4
    warn "未检测到全局 IPv6，配置默认只维护 A 记录（可后续改 DDNS_IPV=46）"
fi

case "$CRED_MODE" in
    encrypted) CRED_DESC="LoadCredentialEncrypted（systemd-creds，密文落盘）" ;;
    loadcredential) CRED_DESC="LoadCredential（明文凭据文件 600）" ;;
    *) CRED_DESC="EnvironmentFile（600）" ;;
esac

info "=== 环境检测 ==="
info "系统: $OS $ARCH"
info "systemd: $SYSTEMD (版本 ${SYSTEMD_VER:-0})"
info "容器: $CONTAINER"
info "全局 IPv6: $HAS_IPV6"
info "systemd-creds: $CREDS_TOOL"
info "=== 选定方案 ==="
info "启动方式: $PLAN"
info "凭证方式: $CRED_DESC"
info "IP 模式: $IPV_MODE"
info "附加参数: ${EXTRA_ARGS:-无}"
info "输出目录: $OUTPUT_DIR"
info ""

if [ "$DRY_RUN" = yes ]; then
    info "--dry-run：仅打印方案，不写文件"
    exit 0
fi

# ---------- 写文件 ----------

write_file() {
    path=$1
    if [ -e "$path" ] && [ "$FORCE" != yes ]; then
        warn "已存在，跳过（用 --force 覆盖）: $path"
        return 0
    fi
    cat > "$path"
    info "已生成: $path"
}

mkdir -p "$OUTPUT_DIR"

write_file "$OUTPUT_DIR/config.json" <<EOF
{
    "domain": "",
    "rr": "@",
    "ipv": "$IPV_MODE",
    "interval": 300,
    "verify_interval": 21600,
    "ttl": 600,
    "line": "default",
    "ipv6_prefer": "auto",
    "allow_ula": false,
    "interface": "",
    "on_change": "",
    "on_change_timeout": 60
}
EOF

if [ "$PLAN" = systemd ] && [ "$SYSTEMD_VER" -ge 247 ]; then
    STATE_LINE="# DDNS_STATE_FILE  # systemd StateDirectory 自动推导"
else
    STATE_LINE="DDNS_STATE_FILE=$STATE_FILE"
fi

write_file "$OUTPUT_DIR/env" <<EOF
# 由 adapt-install.sh 生成（$OS / $PLAN / $CRED_MODE）
# 凭证（只放这里或 LoadCredential，不要写进 config.json）
# ALIBABA_CLOUD_ACCESS_KEY_ID=
# ALIBABA_CLOUD_ACCESS_KEY_SECRET=

# 部署覆盖（可选）
$STATE_LINE
# DDNS_NO_EVENTS=0
# DDNS_VERBOSE=0
EOF
chmod 600 "$OUTPUT_DIR/env" 2>/dev/null || true

if [ "$PLAN" = systemd ]; then
    if [ -f "$SCRIPT_DIR/alidns-ddns.service" ]; then
        tmp="$OUTPUT_DIR/alidns-ddns.service"
        cp "$SCRIPT_DIR/alidns-ddns.service" "$tmp"
        if [ -n "$EXTRA_ARGS" ]; then
            sed -i.bak "s|^ExecStart=\(.*\)$|ExecStart=\1 $EXTRA_ARGS|" "$tmp"
            rm -f "$tmp.bak"
        fi
        case "$CRED_MODE" in
            loadcredential)
                sed -i.bak 's|^#LoadCredential=|LoadCredential=|' "$tmp"
                rm -f "$tmp.bak"
                ;;
            encrypted)
                sed -i.bak 's|^#LoadCredential=alidns-access-key-id:.*$|LoadCredentialEncrypted=alidns-access-key-id:/etc/alidns-ddns/alidns-access-key-id.cred|; s|^#LoadCredential=alidns-access-key-secret:.*$|LoadCredentialEncrypted=alidns-access-key-secret:/etc/alidns-ddns/alidns-access-key-secret.cred|' "$tmp"
                rm -f "$tmp.bak"
                ;;
        esac
        [ -e "$OUTPUT_DIR/alidns-ddns.service" ] && [ "$FORCE" != yes ] || cp "$tmp" "$OUTPUT_DIR/alidns-ddns.service"
        info "已生成: $OUTPUT_DIR/alidns-ddns.service"
    else
        warn "找不到模板 alidns-ddns.service，跳过服务单元生成"
    fi
elif [ "$PLAN" = launchd ]; then
    if [ -f "$PLIST_SRC" ]; then
        dest="$OUTPUT_DIR/com.tongtf.alidns-ddns.plist"
        sed -e "s|@INSTALL_DIR@|$INSTALL_DIR|g" \
            -e "s|@CONFIG_DIR@|$CONFIG_DIR|g" \
            -e "s|@LOG_DIR@|$LOG_DIR|g" \
            -e "s|@USERNAME@|${TARGET_USER:-}|g" \
            "$PLIST_SRC" > "$dest"
        info "已生成: $dest"
    else
        warn "找不到 launchd 模板，跳过 plist 生成"
    fi
elif [ "$PLAN" = standalone ]; then
    BIN=$(command -v alidns-ddns 2>/dev/null || echo /usr/local/bin/alidns-ddns)
    write_file "$OUTPUT_DIR/run-alidns-ddns.sh" <<EOF
#!/bin/sh
# 前台运行，适合 runit / supervisord / Docker ENTRYPOINT
set -eu
BASE_DIR=\$(CDPATH= cd -- "\$(dirname -- "\$0")" && pwd)
set -a
. "\$BASE_DIR/env"
set +a
exec "$BIN" --config "\$BASE_DIR/config.json" $EXTRA_ARGS
EOF
    chmod +x "$OUTPUT_DIR/run-alidns-ddns.sh" 2>/dev/null || true
fi

write_file "$OUTPUT_DIR/README.generated.txt" <<EOF
由 adapt-install.sh 生成：OS=$OS, ARCH=$ARCH, systemd=$SYSTEMD/$SYSTEMD_VER,
容器=$CONTAINER, IPv6=$HAS_IPV6, 方案=$PLAN, 凭证=$CRED_MODE。

下一步（通用）：
1) 编辑 config.json，填入 domain（和需要的 rr / ipv 等）。
   密钥不要写进 config.json，只从 env 或 LoadCredential 提供。
2) 把生成的文件复制到目标位置（见下方对应方案）。

systemd（$PLAN=systemd）：
  sudo cp $OUTPUT_DIR/config.json /etc/alidns-ddns/config.json
  sudo cp $OUTPUT_DIR/env /etc/alidns-ddns/env
  sudo cp $OUTPUT_DIR/alidns-ddns.service /etc/systemd/system/alidns-ddns.service
  sudo systemctl daemon-reload
  sudo systemctl enable --now alidns-ddns

凭证（$CRED_MODE）：
  envfile: 在 /etc/alidns-ddns/env 里填 ALIBABA_CLOUD_ACCESS_KEY_ID/SECRET；
           域名写在 config.json 的 domain 字段
  loadcredential:
    printf '%s' 'AKID' | sudo tee /etc/alidns-ddns/access_key_id >/dev/null
    printf '%s' 'SECRET' | sudo tee /etc/alidns-ddns/access_key_secret >/dev/null
    sudo chmod 600 /etc/alidns-ddns/access_key_*
  encrypted (systemd >= 250):
    sudo systemd-creds setup
    printf '%s' 'AKID' | sudo systemd-creds encrypt --with-key=host \\
      --name=alidns-access-key-id - /etc/alidns-ddns/alidns-access-key-id.cred
    printf '%s' 'SECRET' | sudo systemd-creds encrypt --with-key=host \\
      --name=alidns-access-key-secret - /etc/alidns-ddns/alidns-access-key-secret.cred
    详见 README「可选：加密凭据」。

launchd（macOS）：
  把 plist 放到 ~/Library/LaunchAgents/（用户级）或 /Library/LaunchDaemons/（root），
  二进制与配置放到 plist 里写的路径，然后 launchctl bootstrap / kickstart。
  详见 README macOS 部署章节。

无 systemd（standalone）：
  把目录放到固定位置，直接执行 run-alidns-ddns.sh（前台），
  或交给 runit / supervisord / Docker 管理。状态文件由 DDNS_STATE_FILE 指定。
EOF

info ""
info "完成。请先阅读 $OUTPUT_DIR/README.generated.txt"
