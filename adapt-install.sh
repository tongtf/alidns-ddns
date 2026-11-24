#!/bin/sh
# 环境适配安装检测/迁移：检测当前系统、读取已安装配置，生成最新版配置文件与启动脚本，
# 并尽可能自动把 AK/SK 转成 systemd 凭据（明文 600 或 systemd-creds 加密）。
#
# 默认只生成不安装；--apply 才复制文件并重启服务。
#
# 用法：
#   ./adapt-install.sh [--output DIR] [--source DIR] [--force] [--dry-run]
#                      [--apply] [--yes] [--no-encrypt]
#
# --source DIR   从 DIR 读取旧的 config.json / env（默认自动探测）
# --apply        把生成结果安装到系统目录并重启服务（需相应权限）
# --yes          --apply 时跳过确认
# --no-encrypt   即使支持 systemd-creds 也只写明文凭据（600）

set -u

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
OUTPUT_DIR="$SCRIPT_DIR/deploy"
SOURCE_DIR=""
FORCE=no
DRY_RUN=no
APPLY=no
ASSUME_YES=no
ENCRYPT_AUTO=yes

while [ "$#" -gt 0 ]; do
    case "$1" in
        --output)
            [ "$#" -ge 2 ] || { echo "错误: --output 缺少目录" >&2; exit 2; }
            OUTPUT_DIR=$2
            shift 2
            ;;
        --source)
            [ "$#" -ge 2 ] || { echo "错误: --source 缺少目录" >&2; exit 2; }
            SOURCE_DIR=$2
            shift 2
            ;;
        --force) FORCE=yes; shift ;;
        --dry-run) DRY_RUN=yes; shift ;;
        --apply) APPLY=yes; shift ;;
        --yes) ASSUME_YES=yes; shift ;;
        --no-encrypt) ENCRYPT_AUTO=no; shift ;;
        -h|--help)
            sed -n '2,16p' "$0"
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
die() { printf '错误: %s\n' "$*" >&2; exit 1; }

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

# ---------- 读取旧配置 ----------

SRC_CONFIG=""
SRC_ENV=""
DEPLOYED_UNIT=""
DEPLOYED_PLIST=""
SUDO=""

find_source() {
    if [ -n "$SOURCE_DIR" ]; then
        [ -f "$SOURCE_DIR/config.json" ] && SRC_CONFIG="$SOURCE_DIR/config.json"
        [ -f "$SOURCE_DIR/env" ] && SRC_ENV="$SOURCE_DIR/env"
        return
    fi
    if [ "$SYSTEMD" = yes ]; then
        frag=$(systemctl show alidns-ddns.service -p FragmentPath --value 2>/dev/null || true)
        [ -f "$frag" ] && DEPLOYED_UNIT=$frag
        unit_dir=$(dirname "$frag" 2>/dev/null || true)
        envfiles=$(systemctl show alidns-ddns.service -p EnvironmentFile --value 2>/dev/null || true)
        envfile=${envfiles%% *}
        envfile=${envfile#-}
        [ -f "$envfile" ] && SRC_ENV=$envfile
        [ -n "$unit_dir" ] && [ -f "$unit_dir/config.json" ] && SRC_CONFIG="$unit_dir/config.json"
    fi
    for c in /etc/alidns-ddns/config.json "$HOME/.config/alidns-ddns/config.json" ./config.json; do
        [ -z "$SRC_CONFIG" ] && [ -f "$c" ] && SRC_CONFIG=$c
    done
    for e in /etc/alidns-ddns/env "$HOME/.config/alidns-ddns/env" ./env; do
        [ -z "$SRC_ENV" ] && [ -f "$e" ] && SRC_ENV=$e
    done
    # 旧单元可能把配置放在 /etc/alidns-ddns，但 EnvironmentFile 为空
    if [ -f /etc/alidns-ddns/config.json ]; then
        SRC_CONFIG=/etc/alidns-ddns/config.json
    fi
    if [ -f /etc/alidns-ddns/env ]; then
        SRC_ENV=/etc/alidns-ddns/env
    fi
}

# 从 JSON 取简单字符串/数字/布尔值（旧/新配置都是扁平结构）
json_get() {
    key=$1
    file=$2
    [ -f "$file" ] || return 0
    sed -n "s/.*\"$key\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" "$file" | head -1
}

json_get_raw() {
    key=$1
    file=$2
    [ -f "$file" ] || return 0
    sed -n "s/.*\"$key\"[[:space:]]*:[[:space:]]*\([^,}]*\).*/\1/p" "$file" \
        | head -1 | sed 's/[[:space:]]//g'
}

env_get() {
    key=$1
    file=$2
    [ -f "$file" ] || return 0
    sed -n "s/^[[:space:]]*$key=//p" "$file" | tail -1 \
        | sed -e 's/^"//' -e 's/"$//' -e "s/^'//" -e "s/'\$//"
}

# 按优先级从多个来源取第一个非空值：JSON 键… | ENV 键…
pick_old() {
    file=$1
    shift
    # 环境变量优先于 config.json（与运行时优先级一致）
    for item in "$@"; do
        case "$item" in
            e:*) v=$(env_get "${item#e:}" "$SRC_ENV") ;;
            *) v= ;;
        esac
        if [ -n "$v" ]; then printf '%s' "$v"; return; fi
    done
    for item in "$@"; do
        case "$item" in
            j:*) v=$(json_get "${item#j:}" "$file") ;;
            *) v= ;;
        esac
        if [ -n "$v" ]; then printf '%s' "$v"; return; fi
    done
}

pick_old_raw() {
    file=$1
    shift
    for item in "$@"; do
        case "$item" in
            e:*) v=$(env_get "${item#e:}" "$SRC_ENV") ;;
            *) v= ;;
        esac
        if [ -n "$v" ]; then printf '%s' "$v"; return; fi
    done
    for item in "$@"; do
        case "$item" in
            j:*) v=$(json_get_raw "${item#j:}" "$file") ;;
            *) v= ;;
        esac
        if [ -n "$v" ]; then printf '%s' "$v"; return; fi
    done
}

find_source

M_DOMAIN=$(pick_old "$SRC_CONFIG" j:domain j:DomainName e:DDNS_DOMAIN e:ALIDNS_DOMAIN)
M_RR=$(pick_old "$SRC_CONFIG" j:rr j:RR e:DDNS_RR e:ALIDNS_RR)
M_IPV=$(pick_old "$SRC_CONFIG" j:ipv j:IPv e:DDNS_IPV)
M_INTERFACE=$(pick_old "$SRC_CONFIG" j:interface j:Interface e:DDNS_INTERFACE e:ALIDNS_INTERFACE)
M_PREFER=$(pick_old "$SRC_CONFIG" j:ipv6_prefer j:Ipv6Prefer e:DDNS_IPV6_PREFER e:ALIDNS_IPV6_PREFER)
M_LINE=$(pick_old "$SRC_CONFIG" j:line j:Line e:DDNS_LINE)
M_ONCHANGE=$(pick_old "$SRC_CONFIG" j:on_change j:OnChange e:DDNS_ON_CHANGE)
M_INTERVAL=$(pick_old_raw "$SRC_CONFIG" j:interval j:Interval e:DDNS_INTERVAL)
M_VERIFY=$(pick_old_raw "$SRC_CONFIG" j:verify_interval j:VerifyInterval e:DDNS_VERIFY_INTERVAL)
M_TTL=$(pick_old_raw "$SRC_CONFIG" j:ttl j:Ttl e:DDNS_TTL)
M_ONCHANGETIMEOUT=$(pick_old_raw "$SRC_CONFIG" j:on_change_timeout j:OnChangeTimeout e:DDNS_ON_CHANGE_TIMEOUT)
M_STATEFILE=$(pick_old "$SRC_CONFIG" j:StateFile e:DDNS_STATE_FILE)
M_AK=$(pick_old "$SRC_CONFIG" j:AccessKeyID e:ALIBABA_CLOUD_ACCESS_KEY_ID)
M_SK=$(pick_old "$SRC_CONFIG" j:AccessKeySecret e:ALIBABA_CLOUD_ACCESS_KEY_SECRET)
M_ALLOWULA=$(pick_old_raw "$SRC_CONFIG" j:allow_ula j:AllowUla e:DDNS_ALLOW_ULA)

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
        PLIST_FILE=/Library/LaunchDaemons/com.tongtf.alidns-ddns.plist
    else
        INSTALL_DIR="$HOME/.local/bin"
        CONFIG_DIR="$HOME/.config/alidns-ddns"
        LOG_DIR="$HOME/Library/Logs"
        STATE_FILE="$HOME/.local/state/alidns-ddns/state.json"
        PLIST_SRC="$SCRIPT_DIR/alidns-ddns.plist"
        PLIST_FILE="$HOME/Library/LaunchAgents/com.tongtf.alidns-ddns.plist"
    fi
}

if [ "$OS" = "Darwin" ]; then
    PLAN=launchd
elif [ "$SYSTEMD" = "yes" ]; then
    PLAN=systemd
    if [ "$SYSTEMD_VER" -ge 247 ]; then
        CRED_MODE=loadcredential
        if [ "$CREDS_TOOL" = yes ] && [ "$SYSTEMD_VER" -ge 250 ] && [ "$ENCRYPT_AUTO" = yes ]; then
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

[ -n "${ADAPT_FORCE_PLAN:-}" ] && PLAN=$ADAPT_FORCE_PLAN
[ -n "${ADAPT_FORCE_CRED:-}" ] && CRED_MODE=$ADAPT_FORCE_CRED
[ -n "${ADAPT_FORCE_CONTAINER:-}" ] && CONTAINER=$ADAPT_FORCE_CONTAINER
[ -n "${ADAPT_FORCE_IPV6:-}" ] && HAS_IPV6=$ADAPT_FORCE_IPV6
[ -n "${ADAPT_FORCE_UNIT:-}" ] && DEPLOYED_UNIT=$ADAPT_FORCE_UNIT
[ -z "$DEPLOYED_UNIT" ] && [ -f /etc/systemd/system/alidns-ddns.service ] \
    && DEPLOYED_UNIT=/etc/systemd/system/alidns-ddns.service
[ -n "${ADAPT_FORCE_PLIST:-}" ] && DEPLOYED_PLIST=$ADAPT_FORCE_PLIST
[ "$PLAN" = launchd ] && setup_launchd_paths
[ "$PLAN" = launchd ] && [ -f "$PLIST_FILE" ] && DEPLOYED_PLIST=$PLIST_FILE
[ "$PLAN" != systemd ] && CRED_MODE=envfile

# 非 root 时尝试免密 sudo：systemd-creds 的 host key 只有 root 可读写
if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then
    sudo -n true >/dev/null 2>&1 && SUDO="sudo -n"
fi

if [ "$CONTAINER" = yes ] && [ "$PLAN" != launchd ]; then
    EXTRA_ARGS="--no-events"
fi

# 迁移值优先于环境探测；都没有才用探测默认
[ -n "$M_IPV" ] && IPV_MODE=$M_IPV || IPV_MODE=""
[ -z "$IPV_MODE" ] && IPV_MODE=$([ "$HAS_IPV6" = yes ] && echo 46 || echo 4)
[ -n "$M_STATEFILE" ] && STATE_FILE=$M_STATEFILE

# 没有可迁移的密钥时，凭据模式降级为 env，避免生成引用不存在凭据文件的单元
if [ "$CRED_MODE" != envfile ] && { [ -z "$M_AK" ] || [ -z "$M_SK" ]; }; then
    warn "未找到 AK/SK，凭证方式降为 EnvironmentFile（请手工填写 env）"
    CRED_MODE=envfile
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
info "=== 来源 ==="
info "config: ${SRC_CONFIG:-未找到}"
info "env:    ${SRC_ENV:-未找到}"
info "=== 选定方案 ==="
info "启动方式: $PLAN"
info "凭证方式: $CRED_DESC"
info "IP 模式: $IPV_MODE"
info "附加参数: ${EXTRA_ARGS:-无}"
info "输出目录: $OUTPUT_DIR"
if [ -n "$M_AK" ] && [ -n "$M_SK" ]; then
    info "AK/SK: 已从旧配置读取，将自动写入 $CRED_MODE"
else
    warn "未从旧配置读到 AK/SK；生成文件会留占位符，需手工填写"
fi
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

# 重写单元里的 LoadCredential 行，兼容仓库模板和已部署单元
rewrite_service_credentials() {
    src=$1
    dst=$2
    mode=$3
    awk -v mode="$mode" '
        /^[[:space:]]*#?[[:space:]]*LoadCredential(Encrypted)?=/ { next }
        /^\[Install\]/ && !done {
            if (mode == "encrypted") {
                print "LoadCredentialEncrypted=alidns-access-key-id:/etc/alidns-ddns/alidns-access-key-id.cred"
                print "LoadCredentialEncrypted=alidns-access-key-secret:/etc/alidns-ddns/alidns-access-key-secret.cred"
            } else if (mode == "loadcredential") {
                print "LoadCredential=alidns-access-key-id:/etc/alidns-ddns/access_key_id"
                print "LoadCredential=alidns-access-key-secret:/etc/alidns-ddns/access_key_secret"
            }
            done=1
        }
        { print }
    ' "$src" > "$dst"
}

json_default() { [ -n "$1" ] && printf '%s' "$1" || printf '%s' "$2"; }
bool_default() { case "$1" in true|false) printf '%s' "$1" ;; *) printf '%s' "$2" ;; esac; }

write_file "$OUTPUT_DIR/config.json" <<EOF
{
    "domain": "$(json_default "$M_DOMAIN" '')",
    "rr": "$(json_default "$M_RR" '@')",
    "ipv": "$IPV_MODE",
    "interval": $(json_default "$M_INTERVAL" 300),
    "verify_interval": $(json_default "$M_VERIFY" 21600),
    "ttl": $(json_default "$M_TTL" 600),
    "line": "$(json_default "$M_LINE" 'default')",
    "ipv6_prefer": "$(json_default "$M_PREFER" 'auto')",
    "allow_ula": $(bool_default "$M_ALLOWULA" false),
    "interface": "$(json_default "$M_INTERFACE" '')",
    "on_change": "$(json_default "$M_ONCHANGE" '')",
    "on_change_timeout": $(json_default "$M_ONCHANGETIMEOUT" 60)
}
EOF

# 状态路径：现代 systemd 自动推导，其余写入 env
if [ -z "$M_STATEFILE" ] && [ "$PLAN" = systemd ] && [ "$SYSTEMD_VER" -ge 247 ]; then
    STATE_LINE="# DDNS_STATE_FILE  # systemd StateDirectory 自动推导"
else
    STATE_LINE="DDNS_STATE_FILE=$STATE_FILE"
fi

# 凭证：密文/明文凭据文件，或写入 env（绝不进 config.json）
CRED_LINES="ALIBABA_CLOUD_ACCESS_KEY_ID=
ALIBABA_CLOUD_ACCESS_KEY_SECRET="
case "$CRED_MODE" in
    encrypted|loadcredential)
        if [ -n "$M_AK" ] && [ -n "$M_SK" ]; then
            ( umask 077
              printf '%s' "$M_AK" > "$OUTPUT_DIR/access_key_id"
              printf '%s' "$M_SK" > "$OUTPUT_DIR/access_key_secret"
            )
            if [ "$CRED_MODE" = encrypted ]; then
                if [ "$(id -u)" -ne 0 ] && [ -z "$SUDO" ]; then
                    warn "systemd-creds 加密需要 root（host key 仅 root 可读）；退回明文凭据文件（600）。请用 sudo 重跑以启用加密"
                    rm -f "$OUTPUT_DIR"/*.cred
                    CRED_MODE=loadcredential
                elif printf '%s' "$M_SK" | $SUDO systemd-creds encrypt --with-key=host \
                    --name=alidns-access-key-secret - "$OUTPUT_DIR/alidns-access-key-secret.cred" \
                   && printf '%s' "$M_AK" | $SUDO systemd-creds encrypt --with-key=host \
                    --name=alidns-access-key-id - "$OUTPUT_DIR/alidns-access-key-id.cred"; then
                    $SUDO chown "$(id -u):$(id -g)" "$OUTPUT_DIR"/*.cred 2>/dev/null || true
                    rm -f "$OUTPUT_DIR/access_key_id" "$OUTPUT_DIR/access_key_secret"
                    CRED_LINES="# AK/SK 已加密为 *.cred（见输出目录）"
                else
                    warn "systemd-creds 加密失败（见上方错误），退回明文凭据文件（600）"
                    rm -f "$OUTPUT_DIR"/*.cred
                    CRED_MODE=loadcredential
                fi
            fi
            [ "$CRED_MODE" = loadcredential ] && CRED_LINES="# AK/SK 已写入明文凭据文件（600）"
        fi
        ;;
    envfile)
        if [ -n "$M_AK" ] && [ -n "$M_SK" ]; then
            CRED_LINES="ALIBABA_CLOUD_ACCESS_KEY_ID=$M_AK
ALIBABA_CLOUD_ACCESS_KEY_SECRET=$M_SK"
        fi
        ;;
esac

write_file "$OUTPUT_DIR/env" <<EOF
# 由 adapt-install.sh 生成（$OS / $PLAN / $CRED_MODE）
# 域名在 config.json；这里只放凭证与部署覆盖
$CRED_LINES

$STATE_LINE
# DDNS_NO_EVENTS=0
# DDNS_VERBOSE=0
EOF
chmod 600 "$OUTPUT_DIR/env" 2>/dev/null || true

if [ "$PLAN" = systemd ]; then
    SERVICE_TMPL="$SCRIPT_DIR/alidns-ddns.service"
    if [ ! -f "$SERVICE_TMPL" ] && [ -n "$DEPLOYED_UNIT" ] && [ -f "$DEPLOYED_UNIT" ]; then
        SERVICE_TMPL=$DEPLOYED_UNIT
        info "仓库模板缺失，改用已部署单元作为模板: $DEPLOYED_UNIT"
    fi
    if [ -f "$SERVICE_TMPL" ]; then
        tmp="$OUTPUT_DIR/.alidns-ddns.service.tmp"
        cp "$SERVICE_TMPL" "$tmp"
        if [ -n "$EXTRA_ARGS" ]; then
            sed -i.bak "s|^ExecStart=\(.*\)$|ExecStart=\1 $EXTRA_ARGS|" "$tmp"
            rm -f "$tmp.bak"
        fi
        rewrite_service_credentials "$tmp" "$OUTPUT_DIR/alidns-ddns.service" "$CRED_MODE"
        rm -f "$tmp"
        info "已生成: $OUTPUT_DIR/alidns-ddns.service"
    else
        warn "找不到 alidns-ddns.service 模板，跳过服务单元生成"
    fi
elif [ "$PLAN" = launchd ]; then
    if [ ! -f "$PLIST_SRC" ] && [ -n "$DEPLOYED_PLIST" ] && [ -f "$DEPLOYED_PLIST" ]; then
        PLIST_SRC=$DEPLOYED_PLIST
        info "仓库模板缺失，改用已部署 plist 作为模板: $DEPLOYED_PLIST"
    fi
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
else
    BIN=$(command -v alidns-ddns 2>/dev/null || echo /usr/local/bin/alidns-ddns)
    write_file "$OUTPUT_DIR/run-alidns-ddns.sh" <<EOF
#!/bin/sh
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
由 adapt-install.sh 生成：OS=$OS, systemd=$SYSTEMD/$SYSTEMD_VER,
来源 config=${SRC_CONFIG:-无}, env=${SRC_ENV:-无}
方案=$PLAN, 凭证=$CRED_MODE, IP 模式=$IPV_MODE。

生成文件：
  config.json        新格式业务配置（密钥已移除）
  env                凭证（$CRED_MODE）+ 部署覆盖，600
  $([ "$CRED_MODE" = encrypted ] && echo '*.cred  AK/SK 密文（systemd-creds）' || true)
  $([ "$PLAN" = systemd ] && echo 'alidns-ddns.service' || true)
  $([ "$PLAN" = launchd ] && echo 'com.tongtf.alidns-ddns.plist' || true)
  $([ "$PLAN" = standalone ] && echo 'run-alidns-ddns.sh' || true)

--apply 会把以上文件复制到系统目录、备份旧文件、重载 systemd 并重启服务。
若无 AK/SK，请先填写 env 或重新用 systemd-creds 生成 .cred 后再启动。
EOF

# ---------- 可选安装 ----------

if [ "$APPLY" = yes ]; then
    if [ "$PLAN" = standalone ]; then
        warn "standalone 模式没有系统服务可安装；请把输出目录放到固定位置并运行 run-alidns-ddns.sh"
        exit 0
    fi
    if [ "$ASSUME_YES" != yes ]; then
        printf '将安装到系统目录并重启 alidns-ddns 服务，继续？[y/N] '
        read -r answer
        case "$answer" in y|Y|yes|YES) ;; *) info "已取消"; exit 0 ;; esac
    fi
    if [ "$PLAN" = systemd ]; then
        TARGET_CONFIG=/etc/alidns-ddns/config.json
        TARGET_ENV=/etc/alidns-ddns/env
        mkdir -p /etc/alidns-ddns
    else
        TARGET_CONFIG=$CONFIG_DIR/config.json
        TARGET_ENV=$CONFIG_DIR/env
        mkdir -p "$CONFIG_DIR" "$LOG_DIR" "$INSTALL_DIR"
    fi
    for f in "$TARGET_CONFIG" "$TARGET_ENV"; do
        [ -f "$f" ] && cp -p "$f" "$f.bak.$(date +%Y%m%d%H%M%S)"
    done
    cp "$OUTPUT_DIR/config.json" "$TARGET_CONFIG"
    cp "$OUTPUT_DIR/env" "$TARGET_ENV"
    chmod 644 "$TARGET_CONFIG"
    chmod 600 "$TARGET_ENV"

    if [ "$CRED_MODE" = encrypted ] || [ "$CRED_MODE" = loadcredential ]; then
        for cred in "$OUTPUT_DIR"/*.cred "$OUTPUT_DIR/access_key_id" "$OUTPUT_DIR/access_key_secret"; do
            [ -f "$cred" ] || continue
            cp "$cred" "/etc/alidns-ddns/$(basename "$cred")"
            chmod 600 "/etc/alidns-ddns/$(basename "$cred")"
        done
    fi

    if [ "$PLAN" = systemd ]; then
        cp "$OUTPUT_DIR/alidns-ddns.service" /etc/systemd/system/alidns-ddns.service
        systemctl daemon-reload
        if systemctl is-enabled --quiet alidns-ddns.service 2>/dev/null; then
            systemctl restart alidns-ddns.service
        else
            systemctl enable --now alidns-ddns.service
        fi
        info "已安装并启动 alidns-ddns.service"
    else
        mkdir -p "$(dirname "$PLIST_FILE")"
        cp "$OUTPUT_DIR/com.tongtf.alidns-ddns.plist" "$PLIST_FILE"
        info "已安装 launchd plist：$PLIST_FILE（请按 README 执行 bootstrap/kickstart）"
    fi
fi

# 若通过 sudo 运行，把生成文件还给原始用户，避免后续无法编辑
if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ -d "$OUTPUT_DIR" ]; then
    chown -R "$SUDO_USER:$(id -gn "$SUDO_USER")" "$OUTPUT_DIR" 2>/dev/null || true
fi

info ""
info "完成。请先阅读 $OUTPUT_DIR/README.generated.txt"
