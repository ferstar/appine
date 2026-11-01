//! 网络命名空间的 sysctl（`/proc/sys/net`）：接口的配置 `ipv4/conf/<接口>`、
//! `ipv6/conf/<接口>`（含 `all`、`default`），以及 `ipv4/ip_forward`、
//! `ipv4/ip_local_port_range`、`ipv4/ip_unprivileged_port_start`（bind 低于它的端口要
//! `CAP_NET_BIND_SERVICE`）、`ipv4/ping_group_range`（只记下：ping socket 交给宿主，
//! Android 允许所有组）、`core/somaxconn`。
//!
//! 项和默认值照 Linux 6.1（`devinet_sysctl_table`、`ipv4_devconf_dflt`，`addrconf_sysctl`、
//! `ipv6_devconf_dflt`）。值都是整数（`stable_secret` 除外，没有设过时读是 EIO），写入时按
//! `proc_dointvec` 解析。起作用的只有几项（[`crate::netdev`] 里处理）：`disable_ipv6`
//! 去掉、恢复接口的 IPv6，`forwarding` 在 `all`（`ip_forward` 是它的别名）上写时改到每个
//! 接口，IPv4 的 `default` 改到还没有单独设过这一项的接口。别的只记下来：appine 不转发、
//! 不收发 ARP、路由通告。
//!
//! 与 Linux 一样，新建的接口照抄所在命名空间的 `default`；新建的命名空间的 IPv4 部分照抄
//! 开机时的那个现在的值，IPv6 部分是默认值（`devconf_inherit_init_net` 为 0 时）。接口移到
//! 别的命名空间时按那边的 `default` 重来。

use std::collections::BTreeMap;

use crate::errno::Errno;

/// 接口配置的一项：名字、默认值（`default` 和接口的）、`all` 的值。
type Item = (&'static str, i64, i64);

/// IPv4 的项（按名字排序）。`all` 与 `default` 只有 `accept_source_route` 不同。
pub(crate) const V4: [Item; 33] = [
    ("accept_local", 0, 0),
    ("accept_redirects", 1, 1),
    ("accept_source_route", 1, 0),
    ("arp_accept", 0, 0),
    ("arp_announce", 0, 0),
    ("arp_evict_nocarrier", 1, 1),
    ("arp_filter", 0, 0),
    ("arp_ignore", 0, 0),
    ("arp_notify", 0, 0),
    ("bc_forwarding", 0, 0),
    ("bootp_relay", 0, 0),
    ("disable_policy", 0, 0),
    ("disable_xfrm", 0, 0),
    ("drop_gratuitous_arp", 0, 0),
    ("drop_unicast_in_l2_multicast", 0, 0),
    ("force_igmp_version", 0, 0),
    ("forwarding", 0, 0),
    ("igmpv2_unsolicited_report_interval", 10000, 10000),
    ("igmpv3_unsolicited_report_interval", 1000, 1000),
    ("ignore_routes_with_linkdown", 0, 0),
    ("log_martians", 0, 0),
    ("mc_forwarding", 0, 0),
    ("medium_id", 0, 0),
    ("promote_secondaries", 0, 0),
    ("proxy_arp", 0, 0),
    ("proxy_arp_pvlan", 0, 0),
    ("route_localnet", 0, 0),
    ("rp_filter", 0, 0),
    ("secure_redirects", 1, 1),
    ("send_redirects", 1, 1),
    ("shared_media", 1, 1),
    ("src_valid_mark", 0, 0),
    ("tag", 0, 0),
];

/// IPv6 的项（按名字排序）。`all` 与 `default` 只有 `accept_dad` 不同；`mtu` 在接口上是
/// 接口的 MTU（[`Conf::mtu6`]），`stable_secret` 另外记。
pub(crate) const V6: [Item; 58] = [
    ("accept_dad", 1, 0),
    ("accept_ra", 1, 1),
    ("accept_ra_defrtr", 1, 1),
    ("accept_ra_from_local", 0, 0),
    ("accept_ra_min_hop_limit", 1, 1),
    ("accept_ra_mtu", 1, 1),
    ("accept_ra_pinfo", 1, 1),
    ("accept_ra_rt_info_max_plen", 0, 0),
    ("accept_ra_rt_info_min_plen", 0, 0),
    ("accept_ra_rtr_pref", 1, 1),
    ("accept_redirects", 1, 1),
    ("accept_source_route", 0, 0),
    ("accept_untracked_na", 0, 0),
    ("addr_gen_mode", 0, 0),
    ("autoconf", 1, 1),
    ("dad_transmits", 1, 1),
    ("disable_ipv6", 0, 0),
    ("disable_policy", 0, 0),
    ("drop_unicast_in_l2_multicast", 0, 0),
    ("drop_unsolicited_na", 0, 0),
    ("enhanced_dad", 1, 1),
    ("force_mld_version", 0, 0),
    ("force_tllao", 0, 0),
    ("forwarding", 0, 0),
    ("hop_limit", 64, 64),
    ("ignore_routes_with_linkdown", 0, 0),
    ("ioam6_enabled", 0, 0),
    ("ioam6_id", 65535, 65535),
    ("ioam6_id_wide", 4_294_967_295, 4_294_967_295),
    ("keep_addr_on_down", 0, 0),
    ("max_addresses", 16, 16),
    ("max_desync_factor", 600, 600),
    ("mc_forwarding", 0, 0),
    ("mldv1_unsolicited_report_interval", 10000, 10000),
    ("mldv2_unsolicited_report_interval", 1000, 1000),
    ("mtu", 1280, 1280),
    ("ndisc_evict_nocarrier", 1, 1),
    ("ndisc_notify", 0, 0),
    ("ndisc_tclass", 0, 0),
    ("optimistic_dad", 0, 0),
    ("proxy_ndp", 0, 0),
    ("ra_defrtr_metric", 1024, 1024),
    ("regen_max_retry", 3, 3),
    ("router_probe_interval", 60, 60),
    ("router_solicitation_delay", 1, 1),
    ("router_solicitation_interval", 4, 4),
    ("router_solicitation_max_interval", 3600, 3600),
    ("router_solicitations", -1, -1),
    ("rpl_seg_enabled", 0, 0),
    ("seg6_enabled", 0, 0),
    ("seg6_require_hmac", 0, 0),
    ("stable_secret", 0, 0),
    ("suppress_frag_ndisc", 1, 1),
    ("temp_prefered_lft", 86400, 86400),
    ("temp_valid_lft", 604_800, 604_800),
    ("use_oif_addrs_only", 0, 0),
    ("use_optimistic", 0, 0),
    ("use_tempaddr", 0, 0),
];

/// IPv6 的最小 MTU。
pub(crate) const IPV6_MIN_MTU: i64 = 1280;

/// 一族的表。
pub(crate) fn table(v6: bool) -> &'static [Item] {
    if v6 { &V6 } else { &V4 }
}

/// 项 `name` 在表里的位置。
pub(crate) fn position(v6: bool, name: &[u8]) -> Option<usize> {
    table(v6).iter().position(|(n, ..)| n.as_bytes() == name)
}

/// 只读的项（`mc_forwarding`）。
pub(crate) fn read_only(name: &[u8]) -> bool {
    name == b"mc_forwarding"
}

/// 一组接口配置（一族的）：每一项的值，和单独设过哪些（IPv4 的 `default` 只改没设过的）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Conf {
    pub vals: Vec<i64>,
    pub set: Vec<bool>,
    /// IPv6 的 `stable_secret`：没有设过时读是 EIO。
    pub secret: Option<[u8; 16]>,
    /// IPv6 的 `mtu`：设过的值；接口上没设过时是接口的 MTU。
    pub mtu6: Option<i64>,
}

impl Conf {
    /// 默认值：`all` 的或者 `default`（接口）的。
    pub fn new(v6: bool, all: bool) -> Conf {
        let t = table(v6);
        Conf {
            vals: t.iter().map(|&(_, d, a)| if all { a } else { d }).collect(),
            set: vec![false; t.len()],
            secret: None,
            mtu6: None,
        }
    }

    /// 新接口照抄的：`default` 的值，都算没设过。
    pub fn inherit(&self) -> Conf {
        Conf {
            set: vec![false; self.set.len()],
            mtu6: None,
            ..self.clone()
        }
    }

    pub fn get(&self, v6: bool, name: &str) -> i64 {
        position(v6, name.as_bytes()).map_or(0, |i| self.vals[i])
    }
}

/// 一个网络命名空间的 sysctl。
#[derive(Debug, Clone)]
pub(crate) struct NetConf {
    pub all4: Conf,
    pub dflt4: Conf,
    pub all6: Conf,
    pub dflt6: Conf,
    /// 不是虚拟设备的接口（新命名空间的 lo、开机时的命名空间里宿主的接口）的配置，按接口
    /// 编号；没有时是 `default` 的。虚拟设备的记在设备上。
    pub other: BTreeMap<u32, (Conf, Conf)>,
    /// `core/somaxconn`。
    pub somaxconn: i64,
    /// `ipv4/ip_local_port_range`。
    pub port_range: (i64, i64),
    /// `ipv4/ip_unprivileged_port_start`（Linux 的 `sysctl_ip_prot_sock`）。
    pub prot_sock: i64,
    /// `ipv4/ping_group_range`。
    pub ping_group: (i64, i64),
    /// 写过的 [`SYSCTLS`]（没写过的是默认值）。
    pub sysctls: BTreeMap<&'static str, String>,
}

impl Default for NetConf {
    fn default() -> NetConf {
        NetConf {
            all4: Conf::new(false, true),
            dflt4: Conf::new(false, false),
            all6: Conf::new(true, true),
            dflt6: Conf::new(true, false),
            other: BTreeMap::new(),
            somaxconn: 4096,
            port_range: (32768, 60999),
            prot_sock: 1024,
            // 开机时的命名空间与 Android（和 systemd 的默认配置）一样允许所有组；新建的
            // 命名空间是 Linux 的默认值，谁也不允许（只记下，不限制）
            ping_group: (0, 2_147_483_647),
            sysctls: BTreeMap::new(),
        }
    }
}

impl NetConf {
    /// 新建的命名空间的：IPv4 照抄 `init` 现在的 `all`、`default`，别的是默认值。
    pub fn for_new_ns(init: &NetConf) -> NetConf {
        NetConf {
            all4: init.all4.inherit(),
            dflt4: init.dflt4.inherit(),
            ping_group: (1, 0),
            ..NetConf::default()
        }
    }

    /// 不是虚拟设备的接口 `index` 的配置（没有时照抄 `default`）。
    pub fn other_mut(&mut self, index: u32) -> &mut (Conf, Conf) {
        let (d4, d6) = (self.dflt4.inherit(), self.dflt6.inherit());
        self.other.entry(index).or_insert((d4, d6))
    }
}

/// 按 `proc_dointvec` 解析写入的整数：可以有前导空白、正负号，后面可以跟空白（换行）。
pub(crate) fn parse_int(data: &[u8]) -> Result<i64, Errno> {
    let s = std::str::from_utf8(data).map_err(|_| Errno::EINVAL)?;
    let s = s.trim_start();
    let end = s
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || (i == 0 && (c == '-' || c == '+'))))
        .map_or(s.len(), |(i, _)| i);
    if !s[end..].trim().is_empty() {
        return Err(Errno::EINVAL);
    }
    s[..end].parse().map_err(|_| Errno::EINVAL)
}

/// 一项能取的值：大多是 int，几项有范围（Linux 用 `proc_dointvec_minmax`、`proc_douintvec`
/// 的那些）。`mtu` 的上限是接口的 MTU，在 [`crate::netdev`] 里查。
pub(crate) fn check(v6: bool, name: &str, v: i64) -> Result<(), Errno> {
    let (lo, hi) = match (v6, name) {
        (true, "addr_gen_mode") => (0, 3),
        (true, "ioam6_id") => (0, 65535),
        (true, "ioam6_id_wide") => (0, u32::MAX as i64),
        (true, "accept_untracked_na") => (0, 2),
        (true, "ndisc_tclass") => (0, 255),
        (true, "ra_defrtr_metric") => (1, u32::MAX as i64),
        (true, "mtu") => (IPV6_MIN_MTU, i32::MAX as i64),
        _ => (i32::MIN as i64, i32::MAX as i64),
    };
    if (lo..=hi).contains(&v) {
        Ok(())
    } else {
        Err(Errno::EINVAL)
    }
}

/// `stable_secret` 的读：没有设过是 EIO。
pub(crate) fn show_secret(s: Option<[u8; 16]>) -> Result<String, Errno> {
    let s = s.ok_or(Errno::EIO)?;
    Ok(format!("{}\n", std::net::Ipv6Addr::from(s)))
}

/// `stable_secret` 的写：一个 IPv6 地址。
pub(crate) fn parse_secret(data: &[u8]) -> Result<[u8; 16], Errno> {
    let s = std::str::from_utf8(data).map_err(|_| Errno::EINVAL)?;
    s.trim()
        .parse::<std::net::Ipv6Addr>()
        .map(|a| a.octets())
        .map_err(|_| Errno::EINVAL)
}

/// 两个整数（`ip_local_port_range`、`ping_group_range` 的写）。
fn parse_pair(data: &[u8]) -> Result<(i64, i64), Errno> {
    let s = std::str::from_utf8(data).map_err(|_| Errno::EINVAL)?;
    let mut it = s.split_ascii_whitespace().map(|w| w.parse::<i64>());
    let (Some(Ok(a)), Some(Ok(b)), None) = (it.next(), it.next(), it.next()) else {
        return Err(Errno::EINVAL);
    };
    Ok((a, b))
}

/// 写 `ipv4/ip_local_port_range`：两个数，都在 1 到 65535 之间、前一个不大于后一个，并且
/// 不小于 `ip_unprivileged_port_start`（`prot_sock`），不然 EINVAL（Linux 6.1 的
/// `ipv4_local_port_range`）。
pub(crate) fn parse_port_range(data: &[u8], prot_sock: i64) -> Result<(i64, i64), Errno> {
    let (lo, hi) = parse_pair(data)?;
    if !(1..=65535).contains(&lo) || !(1..=65535).contains(&hi) || lo > hi || lo < prot_sock {
        return Err(Errno::EINVAL);
    }
    Ok((lo, hi))
}

/// 写 `ipv4/ip_unprivileged_port_start`：0 到 65535，不能大于本地端口范围的下限
/// （`ipv4_privileged_ports`）。
pub(crate) fn parse_prot_sock(data: &[u8], range_low: i64) -> Result<i64, Errno> {
    let v = parse_int(data)?;
    if !(0..=65535).contains(&v) || range_low < v {
        return Err(Errno::EINVAL);
    }
    Ok(v)
}

/// 写 `ipv4/ping_group_range`：两个组 ID（`ipv4_ping_group_range`）；后一个小于前一个时
/// 换成谁也不允许的 1、0。
pub(crate) fn parse_ping_group(data: &[u8]) -> Result<(i64, i64), Errno> {
    let (lo, hi) = parse_pair(data)?;
    let gid = 0..=i64::from(u32::MAX - 1);
    if !gid.contains(&lo) || !gid.contains(&hi) {
        return Err(Errno::EINVAL);
    }
    Ok(if hi < lo { (1, 0) } else { (lo, hi) })
}

/// `/proc/sys/net` 下别的 sysctl（docs/rounds/177-net-sysctls.md）：路径（`core/`、`ipv4/`、`ipv6/`、
/// `unix/` 下）、默认值（几个数之间是 tab）、权限位，以及是不是只在开机时的命名空间里有。
/// 照 Linux 6.1 在新的网络命名空间里给出的那些（ipv4/conf、route 这些子目录除外）。写入的值
/// 只记下，不起作用。
pub(crate) const SYSCTLS: [(&str, &str, u16, bool); 145] = [
    ("core/netdev_max_backlog", "1000", 0o644, true),
    ("core/optmem_max", "131072", 0o644, false),
    ("core/rmem_default", "212992", 0o644, true),
    ("core/rmem_max", "212992", 0o644, true),
    ("core/txrehash", "1", 0o644, false),
    ("core/wmem_default", "212992", 0o644, true),
    ("core/wmem_max", "212992", 0o644, true),
    ("core/xfrm_acq_expires", "30", 0o644, false),
    ("core/xfrm_aevent_etime", "10", 0o644, false),
    ("core/xfrm_aevent_rseqth", "2", 0o644, false),
    ("core/xfrm_larval_drop", "1", 0o644, false),
    ("ipv4/fib_multipath_hash_fields", "7", 0o644, false),
    ("ipv4/fib_multipath_hash_policy", "0", 0o644, false),
    ("ipv4/fib_multipath_use_neigh", "0", 0o644, false),
    ("ipv4/fib_notify_on_flag_change", "0", 0o644, false),
    ("ipv4/fwmark_reflect", "0", 0o644, false),
    ("ipv4/icmp_echo_enable_probe", "0", 0o644, false),
    ("ipv4/icmp_echo_ignore_all", "0", 0o644, false),
    ("ipv4/icmp_echo_ignore_broadcasts", "1", 0o644, false),
    ("ipv4/icmp_errors_use_inbound_ifaddr", "0", 0o644, false),
    ("ipv4/icmp_ignore_bogus_error_responses", "1", 0o644, false),
    ("ipv4/icmp_ratelimit", "1000", 0o644, false),
    ("ipv4/icmp_ratemask", "6168", 0o644, false),
    ("ipv4/igmp_link_local_mcast_reports", "1", 0o644, false),
    ("ipv4/igmp_max_memberships", "20", 0o644, false),
    ("ipv4/igmp_max_msf", "10", 0o644, false),
    ("ipv4/igmp_qrv", "2", 0o644, false),
    ("ipv4/ip_autobind_reuse", "0", 0o644, false),
    ("ipv4/ip_default_ttl", "64", 0o644, false),
    ("ipv4/ip_dynaddr", "0", 0o644, false),
    ("ipv4/ip_early_demux", "1", 0o644, false),
    ("ipv4/ip_forward_update_priority", "1", 0o644, false),
    ("ipv4/ip_forward_use_pmtu", "0", 0o644, false),
    ("ipv4/ip_local_reserved_ports", "", 0o644, false),
    ("ipv4/ip_no_pmtu_disc", "0", 0o644, false),
    ("ipv4/ip_nonlocal_bind", "0", 0o644, false),
    ("ipv4/ipfrag_high_thresh", "4194304", 0o644, false),
    ("ipv4/ipfrag_low_thresh", "3145728", 0o644, false),
    ("ipv4/ipfrag_max_dist", "64", 0o644, false),
    ("ipv4/ipfrag_time", "30", 0o644, false),
    ("ipv4/nexthop_compat_mode", "1", 0o644, false),
    ("ipv4/raw_l3mdev_accept", "1", 0o644, false),
    ("ipv4/tcp_abort_on_overflow", "0", 0o644, false),
    ("ipv4/tcp_adv_win_scale", "1", 0o644, false),
    (
        "ipv4/tcp_allowed_congestion_control",
        "reno cubic",
        0o444,
        false,
    ),
    ("ipv4/tcp_app_win", "31", 0o644, false),
    ("ipv4/tcp_autocorking", "1", 0o644, false),
    (
        "ipv4/tcp_available_congestion_control",
        "reno cubic",
        0o444,
        false,
    ),
    ("ipv4/tcp_base_mss", "1024", 0o644, false),
    ("ipv4/tcp_challenge_ack_limit", "2147483647", 0o644, false),
    ("ipv4/tcp_child_ehash_entries", "0", 0o644, false),
    ("ipv4/tcp_comp_sack_delay_ns", "1000000", 0o644, false),
    ("ipv4/tcp_comp_sack_nr", "44", 0o644, false),
    ("ipv4/tcp_comp_sack_slack_ns", "100000", 0o644, false),
    ("ipv4/tcp_congestion_control", "cubic", 0o644, false),
    ("ipv4/tcp_dsack", "1", 0o644, false),
    ("ipv4/tcp_early_demux", "1", 0o644, false),
    ("ipv4/tcp_early_retrans", "3", 0o644, false),
    ("ipv4/tcp_ecn", "2", 0o644, false),
    ("ipv4/tcp_ecn_fallback", "1", 0o644, false),
    ("ipv4/tcp_ehash_entries", "-32768", 0o444, false),
    ("ipv4/tcp_fack", "0", 0o644, false),
    ("ipv4/tcp_fastopen", "1", 0o644, false),
    ("ipv4/tcp_fastopen_blackhole_timeout_sec", "0", 0o644, false),
    (
        "ipv4/tcp_fastopen_key",
        "00000000-00000000-00000000-00000000",
        0o600,
        false,
    ),
    ("ipv4/tcp_fin_timeout", "60", 0o644, false),
    ("ipv4/tcp_frto", "2", 0o644, false),
    ("ipv4/tcp_fwmark_accept", "0", 0o644, false),
    ("ipv4/tcp_invalid_ratelimit", "500", 0o644, false),
    ("ipv4/tcp_keepalive_intvl", "75", 0o644, false),
    ("ipv4/tcp_keepalive_probes", "9", 0o644, false),
    ("ipv4/tcp_keepalive_time", "7200", 0o644, false),
    ("ipv4/tcp_l3mdev_accept", "0", 0o644, false),
    ("ipv4/tcp_limit_output_bytes", "1048576", 0o644, false),
    ("ipv4/tcp_max_reordering", "300", 0o644, false),
    ("ipv4/tcp_max_syn_backlog", "256", 0o644, false),
    ("ipv4/tcp_max_tw_buckets", "16384", 0o644, false),
    ("ipv4/tcp_migrate_req", "0", 0o644, false),
    ("ipv4/tcp_min_rtt_wlen", "300", 0o644, false),
    ("ipv4/tcp_min_snd_mss", "48", 0o644, false),
    ("ipv4/tcp_min_tso_segs", "2", 0o644, false),
    ("ipv4/tcp_moderate_rcvbuf", "1", 0o644, false),
    ("ipv4/tcp_mtu_probe_floor", "48", 0o644, false),
    ("ipv4/tcp_mtu_probing", "0", 0o644, false),
    ("ipv4/tcp_no_metrics_save", "0", 0o644, false),
    ("ipv4/tcp_no_ssthresh_metrics_save", "1", 0o644, false),
    ("ipv4/tcp_notsent_lowat", "4294967295", 0o644, false),
    ("ipv4/tcp_orphan_retries", "0", 0o644, false),
    ("ipv4/tcp_pacing_ca_ratio", "120", 0o644, false),
    ("ipv4/tcp_pacing_ss_ratio", "200", 0o644, false),
    ("ipv4/tcp_pingpong_thresh", "1", 0o644, false),
    ("ipv4/tcp_probe_interval", "600", 0o644, false),
    ("ipv4/tcp_probe_threshold", "8", 0o644, false),
    ("ipv4/tcp_recovery", "1", 0o644, false),
    ("ipv4/tcp_reflect_tos", "0", 0o644, false),
    ("ipv4/tcp_reordering", "3", 0o644, false),
    ("ipv4/tcp_retrans_collapse", "1", 0o644, false),
    ("ipv4/tcp_retries1", "3", 0o644, false),
    ("ipv4/tcp_retries2", "15", 0o644, false),
    ("ipv4/tcp_rfc1337", "0", 0o644, false),
    ("ipv4/tcp_rmem", "4096\t131072\t6291456", 0o644, false),
    ("ipv4/tcp_sack", "1", 0o644, false),
    ("ipv4/tcp_slow_start_after_idle", "1", 0o644, false),
    ("ipv4/tcp_stdurg", "0", 0o644, false),
    ("ipv4/tcp_syn_retries", "6", 0o644, false),
    ("ipv4/tcp_synack_retries", "5", 0o644, false),
    ("ipv4/tcp_syncookies", "1", 0o644, false),
    ("ipv4/tcp_thin_linear_timeouts", "0", 0o644, false),
    ("ipv4/tcp_timestamps", "1", 0o644, false),
    ("ipv4/tcp_tso_rtt_log", "9", 0o644, false),
    ("ipv4/tcp_tso_win_divisor", "3", 0o644, false),
    ("ipv4/tcp_tw_reuse", "2", 0o644, false),
    ("ipv4/tcp_window_scaling", "1", 0o644, false),
    ("ipv4/tcp_wmem", "4096\t16384\t4194304", 0o644, false),
    ("ipv4/tcp_workaround_signed_windows", "0", 0o644, false),
    ("ipv4/udp_early_demux", "1", 0o644, false),
    ("ipv4/udp_l3mdev_accept", "0", 0o644, false),
    ("ipv4/udp_rmem_min", "4096", 0o644, false),
    ("ipv4/udp_wmem_min", "4096", 0o644, false),
    ("ipv4/xfrm4_gc_thresh", "32768", 0o644, false),
    ("ipv6/anycast_src_echo_reply", "0", 0o644, false),
    ("ipv6/auto_flowlabels", "1", 0o644, false),
    ("ipv6/bindv6only", "0", 0o644, false),
    ("ipv6/fib_multipath_hash_fields", "7", 0o644, false),
    ("ipv6/fib_multipath_hash_policy", "0", 0o644, false),
    ("ipv6/fib_notify_on_flag_change", "0", 0o644, false),
    ("ipv6/flowlabel_consistency", "1", 0o644, false),
    ("ipv6/flowlabel_reflect", "0", 0o644, false),
    ("ipv6/flowlabel_state_ranges", "0", 0o644, false),
    ("ipv6/fwmark_reflect", "0", 0o644, false),
    ("ipv6/idgen_delay", "1", 0o644, false),
    ("ipv6/idgen_retries", "3", 0o644, false),
    ("ipv6/ioam6_id", "16777215", 0o644, false),
    ("ipv6/ioam6_id_wide", "72057594037927935", 0o644, false),
    ("ipv6/ip6frag_high_thresh", "4194304", 0o644, false),
    ("ipv6/ip6frag_low_thresh", "3145728", 0o644, false),
    ("ipv6/ip6frag_time", "60", 0o644, false),
    ("ipv6/ip_nonlocal_bind", "0", 0o644, false),
    ("ipv6/max_dst_opts_length", "2147483647", 0o644, false),
    ("ipv6/max_dst_opts_number", "8", 0o644, false),
    ("ipv6/max_hbh_length", "2147483647", 0o644, false),
    ("ipv6/max_hbh_opts_number", "8", 0o644, false),
    ("ipv6/seg6_flowlabel", "0", 0o644, false),
    ("ipv6/xfrm6_gc_thresh", "32768", 0o644, false),
    ("unix/max_dgram_qlen", "10", 0o644, false),
];

/// `/proc/sys/net/` 下的路径 `path`（如 `ipv4/tcp_keepalive_time`）在 [`SYSCTLS`] 里的那一项；
/// `init` 是开机时的命名空间（只在那里有的也算）。
pub(crate) fn sysctl(
    path: &[u8],
    init: bool,
) -> Option<&'static (&'static str, &'static str, u16, bool)> {
    SYSCTLS
        .iter()
        .find(|(p, _, _, only)| p.as_bytes() == path && (init || !only))
}

/// 读 [`SYSCTLS`] 里的一项：写过的值或者默认值。
pub(crate) fn sysctl_read(conf: &NetConf, path: &'static str) -> String {
    let dflt = sysctl(path.as_bytes(), true).map_or("", |e| e.1);
    let v = conf.sysctls.get(path).map_or(dflt, String::as_str);
    format!("{v}\n")
}

/// 写 [`SYSCTLS`] 里的一项：整数的按默认值的个数，写几个改前几个、多出来的不看
/// （`proc_dointvec`）；
/// 拥塞控制算法要是有的（不然 ENOENT）；保留的端口、TCP Fast Open 的密钥按格式。
pub(crate) fn sysctl_write(
    conf: &mut NetConf,
    path: &'static str,
    data: &[u8],
) -> Result<(), Errno> {
    let text = std::str::from_utf8(data).map_err(|_| Errno::EINVAL)?;
    let text = text.trim_end_matches(['\n', '\0']).trim();
    let value = match path {
        "ipv4/tcp_congestion_control" => {
            if !matches!(text, "reno" | "cubic") {
                return Err(Errno::ENOENT);
            }
            text.to_owned()
        }
        "ipv4/ip_local_reserved_ports" => {
            let ok = text
                .split(',')
                .all(|r| r.is_empty() || r.split('-').all(|p| p.trim().parse::<u16>().is_ok()));
            if !ok {
                return Err(Errno::EINVAL);
            }
            text.to_owned()
        }
        "ipv4/tcp_fastopen_key" => {
            let ok = text.split(',').all(|k| {
                let parts: Vec<&str> = k.split('-').collect();
                parts.len() == 4
                    && parts
                        .iter()
                        .all(|p| p.len() <= 8 && u32::from_str_radix(p, 16).is_ok())
            });
            if !ok {
                return Err(Errno::EINVAL);
            }
            text.to_owned()
        }
        _ => {
            let old = sysctl_read(conf, path);
            let mut vals: Vec<String> = old.trim_end().split('\t').map(str::to_owned).collect();
            let new: Vec<&str> = text.split_whitespace().collect();
            // 多出来的不看（`__do_proc_dointvec` 按表里的个数读）
            let new = &new[..new.len().min(vals.len())];
            if new.is_empty() || new.iter().any(|v| v.parse::<i64>().is_err()) {
                return Err(Errno::EINVAL);
            }
            for (slot, v) in vals.iter_mut().zip(new) {
                *slot = v.parse::<i64>().unwrap().to_string();
            }
            vals.join("\t")
        }
    };
    conf.sysctls.insert(path, value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 别的 sysctl：默认值，整数写几个改前几个、多出来的不看，不是数是 EINVAL，拥塞控制算法
    /// 要是有的，只在开机时的命名空间里有的。
    #[test]
    fn plain_sysctls() {
        let mut c = NetConf::default();
        assert_eq!(sysctl_read(&c, "ipv4/tcp_keepalive_time"), "7200\n");
        assert_eq!(sysctl_read(&c, "ipv4/tcp_rmem"), "4096\t131072\t6291456\n");
        sysctl_write(&mut c, "ipv4/tcp_rmem", b"8192\n").unwrap();
        assert_eq!(sysctl_read(&c, "ipv4/tcp_rmem"), "8192\t131072\t6291456\n");
        sysctl_write(&mut c, "ipv4/tcp_rmem", b"1 2 3 4").unwrap();
        assert_eq!(sysctl_read(&c, "ipv4/tcp_rmem"), "1\t2\t3\n");
        assert_eq!(
            sysctl_write(&mut c, "ipv4/tcp_fin_timeout", b"abc"),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            sysctl_write(&mut c, "ipv4/tcp_fin_timeout", b""),
            Err(Errno::EINVAL)
        );
        sysctl_write(&mut c, "ipv4/tcp_congestion_control", b"reno\n").unwrap();
        assert_eq!(
            sysctl_write(&mut c, "ipv4/tcp_congestion_control", b"bbr"),
            Err(Errno::ENOENT)
        );
        assert_eq!(sysctl_read(&c, "ipv4/tcp_congestion_control"), "reno\n");
        sysctl_write(&mut c, "ipv4/ip_local_reserved_ports", b"8080,9000-9010").unwrap();
        assert_eq!(
            sysctl_write(&mut c, "ipv4/ip_local_reserved_ports", b"x"),
            Err(Errno::EINVAL)
        );
        // 新的命名空间里没有只在开机时的命名空间里有的
        assert!(sysctl(b"core/rmem_max", true).is_some());
        assert!(sysctl(b"core/rmem_max", false).is_none());
        assert!(sysctl(b"ipv4/tcp_keepalive_time", false).is_some());
        assert!(sysctl(b"ipv4/no_such", true).is_none());
        // 每个命名空间各有一份
        let other = NetConf::for_new_ns(&c);
        assert_eq!(
            sysctl_read(&other, "ipv4/tcp_congestion_control"),
            "cubic\n"
        );
    }

    #[test]
    fn tables_are_sorted_and_parse_like_proc_dointvec() {
        for t in [&V4[..], &V6[..]] {
            assert!(t.windows(2).all(|w| w[0].0 < w[1].0));
        }
        assert_eq!(parse_int(b"1\n"), Ok(1));
        assert_eq!(parse_int(b"  -3 \n"), Ok(-3));
        assert_eq!(parse_int(b"+7"), Ok(7));
        assert_eq!(parse_int(b"x"), Err(Errno::EINVAL));
        assert_eq!(parse_int(b"1 2"), Err(Errno::EINVAL));
        assert_eq!(parse_int(b""), Err(Errno::EINVAL));
        assert_eq!(check(true, "addr_gen_mode", 4), Err(Errno::EINVAL));
        assert_eq!(check(false, "forwarding", 2), Ok(()));
        let c = Conf::new(false, false);
        assert_eq!(c.get(false, "accept_source_route"), 1);
        assert_eq!(Conf::new(false, true).get(false, "accept_source_route"), 0);
        assert_eq!(parse_port_range(b"1024\t65535\n", 1024), Ok((1024, 65535)));
        assert_eq!(parse_port_range(b"80 65534", 1024), Err(Errno::EINVAL));
        assert_eq!(parse_port_range(b"80 65534", 0), Ok((80, 65534)));
        assert_eq!(parse_port_range(b"2000 1001", 0), Err(Errno::EINVAL));
        assert_eq!(parse_prot_sock(b"0", 32768), Ok(0));
        assert_eq!(parse_prot_sock(b"40000", 32768), Err(Errno::EINVAL));
        assert_eq!(parse_ping_group(b"0 2147483647"), Ok((0, 2_147_483_647)));
        assert_eq!(parse_ping_group(b"5 2"), Ok((1, 0)));
        assert_eq!(show_secret(None), Err(Errno::EIO));
    }
}
