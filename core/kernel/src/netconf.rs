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

#[cfg(test)]
mod tests {
    use super::*;

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
