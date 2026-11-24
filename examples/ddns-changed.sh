#!/bin/sh
# alidns-ddns 的 --on-change 钩子样例。
#
# 以 DynamicUser 身份运行，无法直接 reload root 服务（postfix/dovecot）。
# 这里只在 systemd StateDirectory 里记录变更并 touch 标志文件，
# 由 root 的 alidns-ddns-reload.path + .service 触发真正的 reload。
#
# 安装：
#   sudo install -m 0755 examples/ddns-changed.sh /usr/local/bin/ddns-changed
#   在 /etc/alidns-ddns/env 中配置：DDNS_ON_CHANGE=/usr/local/bin/ddns-changed

set -eu

: "${STATE_DIRECTORY:?此脚本需要 systemd 的 StateDirectory=alidns-ddns 提供 STATE_DIRECTORY}"

{
    printf 'time=%s count=%s targets=%s v4=%s v6=%s\n' \
        "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" \
        "${DDNS_CHANGED_COUNT:-0}" \
        "${DDNS_CHANGED_TARGETS:-}" \
        "${DDNS_IPV4:-}" \
        "${DDNS_IPV6:-}"
} >> "$STATE_DIRECTORY/ip-change.log"

# PathChanged= 监听这个文件；不要用 rm，保持文件存在只更新 mtime
: > "$STATE_DIRECTORY/ip-changed.flag"
