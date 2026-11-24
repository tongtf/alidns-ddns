# IPv6 地址选取设计：区分临时/稳定地址，并在轮换期选新弃旧

状态：已实现（Linux 用 netlink，macOS 用 `ifconfig -a -L`）
实现文件：`src/select.rs`（纯函数选取）、`src/discover/ipv6.rs`（Linux netlink）、
`src/discover/ipv6_macos.rs`（macOS ifconfig 解析）、`src/config.rs`（`--ipv6-prefer` / `--allow-ula`）
依赖变化：`neli` 提为 Linux 直接依赖（本就在依赖树中）；Linux 移除 `local-ip-address`，
macOS 保留作为 `ifconfig` 解析失败时的回退

---

## 1. 背景

当前实现（`src/main.rs:417-424`）：

```rust
fn select_ipv6(addrs: &[IpAddr]) -> Option<String> {
    addrs.iter()
        .find(|ip| is_global_ipv6(ip))          // 取第一个全局地址
        .or_else(|| addrs.iter().find(|ip| is_ula_ipv6(ip)))
        .map(|ip| ip.to_string())
}
```

数据源是 `local_ip_address::list_afinet_netifas()`，返回 `Vec<(网卡名, IpAddr)>`。

两个缺陷：

1. **不带地址属性**。该 crate 的 `AfInetInfo` 只有 `addr / iname / is_loopback`，丢弃了 netlink 里的 `ifa_flags` 和 `IFA_CACHEINFO`。无法区分临时地址、稳定地址、deprecated、tentative。
2. **`find()` 取第一个**。地址顺序由内核返回顺序决定，不代表任何语义。系统同时存在临时与稳定地址时，取到的可能是临时地址。

后果（针对邮件服务器）：

- 临时地址（RFC 4941）preferred lifetime 默认 86400s（本机实测），每约 24h 轮换一次
- 轮换期间旧地址变 deprecated、新地址 preferred，两者并存
- AAAA 若指向旧地址，valid lifetime 到期后入站邮件直接断

## 2. 目标 / 非目标

### 目标

- 能识别地址的**类别**（临时 / 稳定 / 其他）与**状态**（preferred / deprecated / tentative）
- 轮换窗口内**自动选择新地址**，不选即将过期的旧地址
- 无临时地址（纯稳定）时正常工作
- 无稳定地址（纯临时）时降级工作并告警
- 稳定地址随**前缀变更**轮换时同样正确处理（同一套规则）
- 选取失败时**保留旧 DNS 记录**，绝不清空
- 纯函数化选择逻辑，可单元测试

### 非目标

- 不实现 DHCPv6-PD 前缀申请
- 不实现 netlink 事件订阅（列入 §13 二期）
- 不改 IPv4 逻辑
- 不改阿里云 API 签名

## 3. IPv6 地址模型

### 3.1 Linux 地址标志（`/usr/include/linux/if_addr.h`，已核对）

| 常量 | 值 | 含义 |
|---|---|---|
| `IFA_F_TEMPORARY` | `0x01` | RFC 4941 隐私临时地址 |
| `IFA_F_DADFAILED` | `0x08` | 重复地址检测失败 |
| `IFA_F_DEPRECATED` | `0x20` | 已弃用：不再用于新建连接，仍可收包 |
| `IFA_F_TENTATIVE` | `0x40` | DAD 进行中，**不可用** |
| `IFA_F_MANAGETEMPADDR` | `0x100` | 稳定地址，负责派生临时地址 |
| `IFA_F_STABLE_PRIVACY` | `0x800` | RFC 7217 稳定隐私地址 |

### 3.2 时间线与状态机

RFC 4941 轮换（`temp_prefered_lft=86400`、`temp_valid_lft=172800` 为常见默认）：

```
T0        新临时地址 B 生成（preferred=86400, valid=172800）
T0        旧临时地址 A 变为 deprecated（preferred=0, valid 仍 >0）
T0..T0+2d A 仍可接收入站，但不再作为源地址；B 为 preferred
T0+2d     A valid 到期，内核删除
```

即：**轮换期存在两个地址，判据是 `preferred_lft > 0`**。选 preferred 即选新。

稳定地址随 ISP 前缀变更时分两种形态，**不能一概而论**。

**形态 A：路由器主动 deprecate 旧前缀**（发送 `preferred_lft=0` 的 PIO）

```
2408:OLD::x/64  global dynamic mngtmpaddr              valid_lft 7200   preferred_lft 0
2408:NEW::x/64  global dynamic mngtmpaddr noprefixroute valid_lft 2592000 preferred_lft 2592000
```

旧地址变 deprecated，"过滤 deprecated"规则直接生效。

**形态 B：路由器只是停止宣告旧前缀**（不发送 deprecate）

RFC 4862 §5.5.3 的 lifetime 更新规则只允许延长、不允许缩短（并有 2 小时下限保护），
因此内核不会立即废弃旧地址，而是按最后一次收到的值继续倒数：

```
2408:OLD::x/64  global dynamic mngtmpaddr              valid_lft 518400  preferred_lft 518400  ← 仍是 preferred
2408:NEW::x/64  global dynamic mngtmpaddr noprefixroute valid_lft 604800  preferred_lft 604800
```

**两个地址同为 stable + preferred，可能并存数小时到数天**。此时仅靠 deprecated 过滤
无法区分，必须引入"新鲜度"判据（见 §6.3）。运营商前缀重委派时形态 B 很常见——
很多路由器不做 deprecation，这是本设计必须覆盖的场景，而非边角情况。

### 3.3 关键区分：deprecated ≠ 不可用

- **deprecated**：仍能接收入站连接，只是不作为新建出站连接的源地址
- **valid_lft = 0**：已删除，任何方向都不可用

因此对 DDNS 而言，deprecated 是"应尽快替换但短期可用"；valid=0 是"绝对不可用"。设计上把 deprecated 放到最后的兜底桶，而不是直接丢弃。

### 3.4 重要陷阱：`getifaddrs` 的 `ifa_flags` 不是地址标志

`/usr/include/ifaddrs.h:34`：

```c
unsigned int ifa_flags;   /* Flags as from SIOCGIFFLAGS ioctl. */
```

它是**接口**标志（`IFF_UP`/`IFF_LOOPBACK`…），**不含** `IFA_F_TEMPORARY`/`IFA_F_DEPRECATED`。
所以不能通过 `getifaddrs`（或 `getifaddrs` crate）拿地址类别。地址标志只有两条来源：

- Linux：netlink `RTM_GETADDR` 的 `ifaddrmsg.ifa_flags`
- BSD/macOS：对 `ifa_addr` 调 `SIOCGIFAFLAG_IN6`，返回 `IN6_IFF_*`

### 3.5 为什么 `/proc` 拿不到生命周期

netlink 与 procfs 是同一个内核结构 `inet6_ifaddr` 的**两个独立出口**，各自手写序列化。

netlink（`net/ipv6/addrconf.c` `inet6_fill_ifaddr`，已核对）：

```c
preferred = READ_ONCE(ifa->prefered_lft);
valid     = READ_ONCE(ifa->valid_lft);
long tval = (jiffies - READ_ONCE(ifa->tstamp)) / HZ;   /* 发送时现算剩余值 */
if (preferred > tval) preferred -= tval; else preferred = 0;
...
put_cacheinfo(skb, ifa->cstamp, ifa->tstamp, preferred, valid);
```

procfs（同文件 `if6_seq_show`）：

```c
seq_printf(seq, "%pi6 %02x %02x %02x %02x %8s\n",
           &ifp->addr, ifp->idev->dev->ifindex, ifp->prefix_len,
           ifp->scope, (u8) ifp->flags, ifp->idev->dev->name);
```

两个函数拿到的是**同一个 `ifp`**。procfs 的打印函数是 1996 年给 `ipv6` 工具写的，
只输出 6 个字段，flags 还强制截断成 u8，之后没有扩展过。`addrconf.c` 里也只注册了
一个 proc 文件：

```c
proc_create_net("if_inet6", 0444, net->proc_net, &if6_seq_ops, ...)
```

`/proc/net/`、`/sys/class/net/` 下没有任何其他文件暴露 IPv6 地址生命周期（已实测）。

结论：**不是 /proc 读不出来，是内核根本没往外写**。这是输出格式的选择，不是能力
边界；用户态无法改变，也没有可开启的开关。要 `preferred_lft` / `valid_lft` /
`cstamp`，只能走 netlink。

## 4. 方案选型

| 方案 | 数据完整度 | 新依赖 | 跨平台 | 代码量 | 评价 |
|---|---|---|---|---|---|
| **B′. `neli` 直连 netlink** | flags(u32) + 生命周期 + cstamp | **0**（已在树中） | Linux only | ~70 行 | **推荐**；safe Rust，数据最全 |
| **F. 读 `/proc/net/if_inet6`** | flags（无生命周期） | **0** | Linux only | ~50 行 | 简单，但无 lifetime/cstamp，形态 B 只能靠启发式 |
| **A. 解析 `ip -6 addr`** | flags + 生命周期 | 0 | Linux only | ~60 行 | 需 spawn 进程 + 文本解析，且不输出 cstamp |
| **C. 手写 libc netlink** | flags + 生命周期 + cstamp | `libc`（已在树中） | Linux only | ~200 行 + unsafe | 等价于 B′ 但多了 unsafe，无收益 |
| **D. UDP connect 源地址选择** | 仅"内核会用的源地址" | 0 | 全平台 | ~25 行 | 只作 tie-break；`use_tempaddr=2` 时会返回临时地址 |
| **E. `getifaddrs` crate** | ❌ 只有接口标志 | +1 | 全平台 | — | **不可行**，见 §3.4 |

### 推荐：B′（neli）+ D（tie-break）+ macOS 回退

**`neli` 已经在这棵依赖树里。** `local-ip-address` 0.6.13 在 Linux 上的
`list_afinet_netifas()` 就是 neli 实现的，`Cargo.lock` 里同时有：

```
local-ip-address 0.6.13
├── libc 0.2.189
├── neli 0.7.4          ← 已编译、已链接进产物
└── windows-sys 0.61.2  (仅 Windows)
```

把 `neli` 提为直接依赖**不新增任何编译单元、不增加产物体积**，"不引入新依赖"的约束
依然成立。并且 `local_ip_address` 在 `src/main.rs` 只有 `get_ipv6()` 一处调用
（第 427 行），Linux 上替换后可以整个移除，依赖树反而少一个 crate。

neli 0.7.4 提供 `Ifa::Cacheinfo` 属性类型（对应 `libc::IFA_CACHEINFO`），payload 是
`struct ifa_cacheinfo { u32 ifa_prefered; u32 ifa_valid; u32 cstamp; }`，neli 没有
typed struct，需手工解 12 字节。

- **Linux**：`NlRouter::connect(NlFamily::Route, ...)` → `Rtm::Getaddr` dump →
  按 `ifaddrmsg` + 属性组装 `V6Cand`。
- **netlink 不可用**（容器 seccomp 等）→ 回退 `kernel_source_addr()`（UDP connect
  取内核源地址，单候选），打 warning 说明判据不足。
- **macOS**：无 netlink，但 `/sbin/ifconfig -a -L` 提供 `temporary` / `deprecated` /
  `tentative` / `dadfailed` / `permanent` 标志与 `pltime` / `vltime` 剩余寿命。
  `src/discover/ipv6_macos.rs` 用 `std::process::Command` 解析（零新增依赖），映射到同一套
  `V6Cand`；`ifconfig` 不可用时告警并回退 `local-ip-address`。
- **其他非 Linux/macOS**：无 netlink 也无 ifconfig 解析器，回退 `local-ip-address`（不分类别）。
- **D 仅作 tie-break**：**不能只用 D**，`use_tempaddr=2` 时内核会优先选临时地址。

## 5. 数据结构与解析

```rust
#[derive(Clone, Copy, PartialEq, Eq)]
enum V6Kind { Stable, Temporary, Other }

#[derive(Clone, Copy, PartialEq, Eq)]
enum V6State { Preferred, Deprecated }

struct V6Cand {
    ifindex: u32,
    ifname: String,
    addr: Ipv6Addr,
    kind: V6Kind,
    state: V6State,
    // scope 不存字段，由 scope_of(&addr) 实时推导：Global | Ula | Skip
    usable: bool,          // !tentative && !dadfailed && !optimistic
    permanent: bool,       // IFA_F_PERMANENT：手工/静态配置
    preferred_lft: u64,    // 剩余秒数；0xFFFFFFFF 表示 forever
    valid_lft: u64,
    cstamp: u32,           // 创建时刻（开机后 1/100 秒），判新旧用（u32 回绕安全）
}
```

### 5.1 Linux 主路径：netlink `RTM_GETADDR`

```rust
let (sock, _) = NlRouter::connect(NlFamily::Route, None, Groups::empty())?;
let msg = IfaddrmsgBuilder::default()
    .ifa_family(RtAddrFamily::Unspecified)
    .build()?;
let recv = sock.send(Rtm::Getaddr, NlmF::REQUEST | NlmF::DUMP,
                     NlPayload::Payload(msg))?;

for rsp in recv {
    let hdr: Nlmsghdr<Rtm, Ifaddrmsg> = rsp?;
    if *hdr.nl_type() != Rtm::Newaddr { continue; }
    let p = hdr.get_payload().ok_or(/* ... */)?;      // &Ifaddrmsg
    if *p.ifa_family() != RtAddrFamily::Inet6 { continue; }
    let ifindex = p.ifa_index();
    for a in p.rtattrs().iter() {
        match *a.rta_type() {
            Ifa::Address   => { /* a.get_payload_as::<u128>() → 16 字节 → Ipv6Addr */ }
            Ifa::Flags     => { /* a.get_payload_as::<u32>() → 完整 IFA_F_* */ }
            Ifa::Cacheinfo => { /* a.payload() → 12 字节手解 */ }
            _ => {}
        }
    }
}
```

用到的属性：

| 属性 | 内容 |
|---|---|
| `Ifa::Address` | 16 字节地址（`get_payload_as::<u128>()`） |
| `Ifa::Flags` | `IFA_F_*` 完整 u32（内核 `inet6_fill_ifaddr` 末尾 `nla_put_u32(IFA_FLAGS)`，不像 `/proc` 被截断成 u8） |
| `Ifa::Cacheinfo` | `preferred_lft` / `valid_lft` / `cstamp`，各 u32 native-endian，共 12 字节 |

`ifaddrmsg` 本身提供 `ifa_family` / `ifa_index` / `ifa_prefixlen` / `ifa_scope` / `ifa_flags`（u8 截断版，忽略）。

`ifa_prefered` / `ifa_valid` 是**内核在发送时现算的剩余秒数**（§3.5 那段代码），
`0xFFFFFFFF` 表示永久。`cstamp` 是开机后的 1/100 秒计数，同一次开机内单调递增，
可直接比较地址新旧——这正是 §3.2 形态 B 需要的权威判据，不依赖任何启发式。

### 5.2 （参考）`/proc/net/if_inet6` 格式

```
24098a444cb971007f591e416ca6bd11 03 40 00 00   wlp5s0
fe80000000000000d374fbdc02930cd5 03 40 20 80   wlp5s0
^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ ^^ ^^ ^^ ^^   ^^^^^^
addr(32 hex)                     |  |  |  |    ifname
                                 |  |  |  flags(hex, u8)
                                 |  |  scope(hex)
                                 |  prefix_len(hex)
                                 ifindex(hex)
```

内核生成代码（`net/ipv6/addrconf.c` `if6_seq_show`，已核对）：

```c
seq_printf(seq, "%pi6 %02x %02x %02x %02x %8s\n",
           &ifp->addr, ifp->idev->dev->ifindex, ifp->prefix_len,
           ifp->scope, (u8) ifp->flags, ifp->idev->dev->name);
```

**`(u8) ifp->flags` 是截断到 8 位的**，因此：

| 标志 | 值 | 可见 | 本设计是否需要 |
|---|---|---|---|
| `IFA_F_TEMPORARY` | `0x01` | ✅ | 需要（分类） |
| `IFA_F_OPTIMISTIC` | `0x04` | ✅ | 需要（排除） |
| `IFA_F_DADFAILED` | `0x08` | ✅ | 需要（排除） |
| `IFA_F_DEPRECATED` | `0x20` | ✅ | 需要（轮换判据） |
| `IFA_F_TENTATIVE` | `0x40` | ✅ | 需要（排除） |
| `IFA_F_PERMANENT` | `0x80` | ✅ | 可选（静态地址标记） |
| `IFA_F_MANAGETEMPADDR` | `0x100` | ❌ 截断 | **不需要**（"非临时"即稳定） |
| `IFA_F_NOPREFIXROUTE` | `0x200` | ❌ 截断 | 不需要 |
| `IFA_F_STABLE_PRIVACY` | `0x800` | ❌ 截断 | 不需要 |

关键结论：**本设计所需的 5 个标志全部落在低 8 位，截断不影响判断**。

映射规则：

- `flags & 0x01` → `Kind::Temporary`；否则 `Kind::Stable`
- `flags & 0x20` → `State::Deprecated`；否则 `State::Preferred`
- `flags & (0x04 | 0x08 | 0x40)` → `usable = false`
- `flags & 0x80` → `permanent = true`
- `scope`：`0x00` → Global；`0x10` → Host（回环）；`0x20` → Link；`0x40` → Site
- 地址本身落在 `fc00::/7` → `Scope::Ula`；其余全局 → `Scope::Global`

解析实现注意：

- 地址是 32 个十六进制字符、无冒号，需每 4 字符转 1 个字节，拼成 `[u8; 16]`
- 字段以任意空白分隔，用 `split_whitespace()` 即可
- 行数通常 < 20，每次全量读取，无性能问题
- 读取失败（容器限制、非 Linux）→ 回退旧逻辑 + 告警，**不可 panic**

macOS 分支：`ifconfig -a -L` 文本按"接口头行 + `inet6` 缩进行"解析，`pltime`/`vltime`
直接作为生命周期，关键字映射到地址状态；失败回退 `local-ip-address` 并告警。
macOS 没有 `cstamp`，同桶排序退化为比较 `valid_lft`（新 SLAAC 地址寿命更长）。

## 6. 选择算法

```rust
fn select(cands: &[V6Cand], cfg: &Cfg, kernel_pick: Option<Ipv6Addr>) -> Option<Pick>
```

### 6.1 硬过滤（顺序无关）

1. `!usable` → 丢弃
2. 回环 / 链路本地 / 组播 / 未指定 → 丢弃
3. 若指定 `--interface`，只保留该网卡（无匹配则告警并回退全量，保持现有行为）

> `valid_lft == 0` 的地址内核会直接删除，正常情况下不会出现在 dump 里；仍显式丢弃，
> 防止与删除操作竞态时读到残值。

### 6.2 分桶与优先级

| 桶 | 条件 |
|---|---|
| `SG` | Global + Stable + Preferred |
| `TG` | Global + Temporary + Preferred |
| `SU` | ULA + Stable + Preferred |
| `TU` | ULA + Temporary + Preferred |
| `D`  | 任意 + Deprecated（仍 valid） |

按 `--ipv6-prefer` 决定桶顺序：

- `auto`（默认）：`SG > TG > SU > TU > D`
- `stable`：`SG > SU > TG > TU > D`
- `temporary`：`TG > TU > SG > SU > D`

> 默认 `auto` 把稳定地址排在临时之前。理由：服务器不需要隐私地址；稳定地址的变更只由前缀变化驱动，频率远低于临时地址的每日轮换。用户若明确想用临时地址，`temporary` 可覆盖。

### 6.3 桶内 tie-break

1. 桶内仅一个 → 选中
2. **`cstamp` 最新者优先**（最晚创建 = 最新前缀）
   - 目的：覆盖 §3.2 形态 B（旧前缀未 deprecated、与新前缀并存且都为 preferred）。
   - 进程重启不影响：`cstamp` 来自内核，不依赖本进程历史。
   - `cstamp` 是 u32（1/100 秒），约 497 天回绕，**不能直接比大小**；两两比较用
     `a.wrapping_sub(b) as i32 > 0` 判 a 更新。
3. 仍并列（`cstamp` 相同，同一批 RA 创建）→ **剩余 `valid_lft` 更大者**
4. 仍并列 → `kernel_source_addr()`（UDP connect，不发包）命中桶内某地址 → 选它
5. 仍并列 → 按 `(ifindex, addr)` 升序取最小，保证确定性

> 第 3~5 步在实践中几乎不会触发：同一 RA 事件创建的多地址，其 `cstamp` 通常仍有
> 微小差异（逐个地址处理，`jiffies` 会前进）。
>
> 若某平台/内核不提供 `IFA_CACHEINFO`（理论上不会），退化为第 4/5 步并打 warning；
> 不再使用"跨轮差分"启发式——那需要进程内状态，重启后失效。

### 6.4 兜底

- 所有桶为空 → 返回 `None`，**不更新 DNS**，保留上一条记录，打 warning
- 只有 `D` 桶非空 → 返回该地址并打 warning（"地址已 deprecated，即将失效"）
- 只有 ULA → 返回并打 warning（"ULA 公网不可达，AAAA 无意义"）

## 7. 轮换窗口行为

```
轮询 N      : A(preferred)              -> 写入 A
轮询 N+1    : A(deprecated) B(preferred) -> 过滤 A(deprecated)，写 B   ← 关键
轮询 N+2..  : B(preferred)              -> 无需更新
T0+2d       : A 被内核删除              -> 无影响
```

现有 `update_dns()` 已做值比较，地址未变时只打印"无需更新"，不会重复调用 API。

### 7.1 竞态

`get_ipv6()` 与 `update_dns()` 之间有窗口，地址可能刚被替换。处理：

- 在调用 `update_dns()` 前**重新读取一次**候选，若与待写值不一致则用新值
- 若重读为空 → 跳过本轮（不写空值）

### 7.2 防抖（**二期设想，尚未实现**）

> 当前版本没有防抖：`--ipv6-settle` / `ALIDNS_IPV6_SETTLE` / `Ipv6Settle` 均不存在。
> 变更后的加速复查由 `schedule()` 的 60s 窗口实现。

`--ipv6-settle <N>`，默认 `1`（规划中）：

- `1`：立即切换（满足"尽快用上新 IP"）
- `>1`：要求连续 N 轮结果一致才切换，用于抑制短暂双 preferred 造成的抖动

## 8. 边界情况清单

| # | 场景 | 期望行为 |
|---|---|---|
| 1 | 临时 + 稳定并存 | 选稳定（`auto`） |
| 2 | 轮换期：旧 temp deprecated + 新 temp preferred | 选新 temp |
| 3 | 前缀变更（路由器 deprecate 旧前缀） | 旧地址 deprecated → 选新 stable |
| 3b | 前缀变更（路由器仅停止宣告旧前缀，不 deprecate） | 两者均 stable+preferred → 按 `cstamp` 选较新前缀 |
| 3c | `cstamp` 与 lifetime 都相同（理论上） | 视为等效，按 §6.3 第 4/5 步确定性选取 |
| 4 | 纯稳定（`use_tempaddr=0`） | 正常，无告警 |
| 5 | 纯临时（无稳定地址） | 使用并告警，建议关隐私扩展 |
| 6 | 全部 deprecated、新地址未就绪 | 用最后一个 deprecated + 告警；不写空 |
| 7 | 地址 tentative（DAD 中） | 排除，等下一轮 |
| 8 | DAD 失败（dadfailed） | 排除 |
| 9 | 仅 ULA | 使用 + 告警（公网不可达） |
| 10 | 无任何全局/ULA | 跳过更新，保留旧记录 |
| 11 | 多网卡均有全局地址 | `--interface` 优先；否则 UDP-connect 裁决；再否则确定性排序 |
| 12 | `use_tempaddr=2`（出站优先临时） | 仍按标志分类，不受影响；不得单靠 UDP-connect |
| 13 | 接口 down / 链路抖动 | 地址消失 → 跳过本轮，保留旧记录 |
| 14 | 容器内仅 lo | 跳过 + 告警 |
| 15 | netlink 不可用（容器 seccomp）/ dump 失败 | 回退 UDP-connect 单候选 + 告警（不崩） |
| 16 | macOS 无 netlink | 解析 `ifconfig -a -L` 拿到标志与 pltime/vltime，复用同一选取算法；失败回退 + 告警 |
| 17 | 读取与 API 调用之间地址变更 | §7.1 重读 |
| 18 | IPv6 被内核禁用 | 返回空，跳过 |
| 19 | 记录类型为 CNAME 或存在多条 AAAA | 沿用现有 `update_dns` 语义（取第一条匹配），本次不改 |
| 20 | TTL 过大导致客户端缓存旧地址 | 文档建议把 AAAA TTL 调小；受阿里云账号档位最小值限制 |

## 9. 接口变更

新增（CLI / env / config.json 三级优先级，沿用 `resolve()`）：

| CLI | 环境变量 | config.json | 默认 | 说明 |
|---|---|---|---|---|
| `--ipv6-prefer` | `DDNS_IPV6_PREFER` | `ipv6_prefer` | `auto` | `auto` / `stable` / `temporary` |
| ~~`--ipv6-settle`~~ | ~~`ALIDNS_IPV6_SETTLE`~~ | ~~`Ipv6Settle`~~ | — | **二期设想，未实现** |

`--ipv6-prefer` 非法值 → 报错退出（与 `--interval` 一致的处理风格）。
防抖选项下限 1（未实现）。

## 10. 日志

```
IPv6 候选(eth0):
  2408:...:aaaa  stable     preferred  lft=2592000/2592000
  2408:...:bbbb  temporary  preferred  lft=86400/172800
  2408:...:cccc  temporary  deprecated lft=0/100000   -> 跳过(轮换旧地址)
选定: 2408:...:bbbb (temporary/preferred)
```

只在**关键决策**打日志（选定、跳过 deprecated、回退、无候选），避免每轮刷屏。
现有 `IPv4: x | IPv6: y` 行保留，追加类别标注。

## 11. 测试

项目当前无测试基础设施（`[dev-dependencies]` 为空）。本设计的核心收益是把**解析**与**选择**分离，使选择逻辑成为纯函数，可零依赖单测：

```rust
#[cfg(test)]
mod tests {
    // 用构造的 Vec<V6Cand> 覆盖 §8 的 1~12
}
```

必须覆盖的用例：

1. temp + stable → stable
2. rotation：deprecated(temp,old) + preferred(temp,new) → new
3. prefix change（形态 A）：deprecated(stable,old) + preferred(stable,new) → new
3b. prefix change（形态 B）：preferred(stable,old,cstamp=100) + preferred(stable,new,cstamp=900) → new
3c. 形态 B 且 `cstamp` 相同 → 按 lifetime / addr 确定性选取（不随枚举顺序变化）
4. stable only
5. temp only
6. all deprecated → 仍返回一个 + 标记告警
7. tentative → 排除
8. ULA only
9. empty → None
10. multi-iface + `--interface`
11. multi-iface 无 `--interface` → 确定性结果
12. `prefer=temporary` 覆盖默认顺序

解析层用固定字节 fixture 测试属性解包（`V6Cand` 组装），不依赖真实网卡。

## 12. 工作量与风险

| 项 | 估算 |
|---|---|
| netlink `RTM_GETADDR` → `V6Cand`（含 cacheinfo 解包） | ~70 行 |
| macOS/非 Linux 回退（保留现状 + 告警一次） | ~10 行 |
| 选择算法 + tie-break | ~80 行 |
| 配置项 + usage + 校验 | ~50 行 |
| 单测 | ~120 行 |
| **合计** | **~330 行**（`main.rs` 614 → ~945） |

依赖变化：**净减少**。`neli` 0.7.4 本就由 `local-ip-address` 引入，提为直接依赖不新增
编译单元；Linux 上移除 `local-ip-address` 后依赖树少一个 crate。不 spawn 子进程，
不引入 unsafe。

风险：

- `neli` 的 `Ifa::Cacheinfo` 需要手工解 12 字节（无 typed struct），字节序为 native
- `neli` 是 blocking API，与现有同步轮询模型一致，无额外复杂性
- `cstamp` 只在单次开机内可比较；进程/设备重启后归零，但那时地址也已重建，无影响
- `cstamp` 是 u32、约 497 天回绕，必须用 `wrapping_sub` 比较（见 §6.3）
- 若某内核/安全模块禁止 `NETLINK_ROUTE`，回退 UDP-connect 并告警，功能降级但不失败
- macOS 无 netlink，无法判类别，退化为现有行为 + 告警一次
- 若 ISP 前缀变化频繁且 TTL 较大，切换仍有 DNS 缓存延迟，属外部限制

## 13. 分阶段落地

**一期（本设计）**：netlink `RTM_GETADDR` dump 解析 + 选择 + 配置 + 单测。

**二期（可选）**：在同一 socket 上订阅 `RTM_NEWADDR`/`RTM_DELADDR`，地址变化时立即触发更新，把发现延迟从"一个轮询周期"降到秒级。届时 `--interval` 仅作为兜底心跳。

## 14. 备选：零代码方案

若目标只是"邮件服务器稳定可用"，最省事的做法是关闭隐私扩展：

```bash
cat >/etc/sysctl.d/99-ipv6-stable.conf <<'EOF'
net.ipv6.conf.all.use_tempaddr = 0
net.ipv6.conf.default.use_tempaddr = 0
net.ipv6.conf.eth0.use_tempaddr = 0
EOF
sysctl --system
```

之后系统只剩稳定地址，现有 `find()` 恰好正确。

**但仍建议实施本设计**，原因：

- 前缀变更时稳定地址同样会 deprecated/重建（场景 3）
- 多网卡/多前缀时 `find()` 仍无确定性（场景 11）
- tentative 地址仍可能被选中（场景 7）
- 依赖运维配置而非代码保证，换机/重装容易遗漏
