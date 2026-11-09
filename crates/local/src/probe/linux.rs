//! Linux 采集：全部数据来自 /proc 文件解析，无外部命令、无特权 API。
//!
//! 端口归属是两步匹配：/proc/net/tcp{,6} 里 LISTEN 行的 socket inode ↔ 树内进程
//! /proc/[pid]/fd 的 `socket:[inode]` 链接。fd 只读树内后代：应用是它们的祖先，
//! Yama ptrace_scope=1 下也放行，天然满足权限面。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use anyhow::{Context, Result};

use super::{ListeningSocket, ProbeProcess};

/// 全量进程表（装配在 mod.rs）。
pub(super) fn processes() -> Result<Vec<ProbeProcess>> {
    Ok(read_processes())
}

/// 结束进程：SIGTERM 经 mod.rs 的公共 `sigterm`。发信号前按采样期的 exe
/// 链接复核身份——现行值不一致即 PID 已被复用，拒杀；采样期链接缺失（None，
/// ptrace 权限所致）时无从比对，交给 kill 自身的存在性检查。
pub(super) fn kill_process(pid: u32, expect_exe: Option<&str>) -> Result<()> {
    if let Some(expected) = expect_exe
        && exe_link(pid).as_deref() != Some(expected)
    {
        anyhow::bail!("进程 {pid} 的可执行文件已变化（疑似 PID 复用），拒绝结束");
    }
    super::sigterm(pid)
}

/// LISTEN socket 的两步匹配：内核表按 inode 索引，树内进程的 fd 链接对号入座。
/// fd 只读树内后代——应用是它们的祖先，Yama ptrace_scope=1 下也放行。
pub(super) fn listening_sockets(
    tree_pids: &HashSet<u32>,
) -> Result<(Vec<ListeningSocket>, Option<String>)> {
    let mut listen_by_inode: HashMap<u32, (IpAddr, u16)> = HashMap::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let text = fs::read_to_string(table).with_context(|| format!("读取 {table} 失败"))?;
        for line in text.lines().skip(1) {
            if let Some((addr, port, inode)) = parse_net_table_line(line) {
                listen_by_inode.insert(inode, (addr, port));
            }
        }
    }

    let mut sockets = Vec::new();
    let mut seen = HashSet::new();
    for pid in tree_pids {
        let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(link) = fs::read_link(entry.path()) else {
                continue;
            };
            let Some(inode) = parse_socket_inode(&link.to_string_lossy()) else {
                continue;
            };
            if !seen.insert((*pid, inode)) {
                continue;
            }
            if let Some(&(addr, port)) = listen_by_inode.get(&inode) {
                sockets.push(ListeningSocket {
                    addr,
                    port,
                    pid: *pid,
                });
            }
        }
    }
    Ok((sockets, None))
}

/// 全量进程表（跳过僵尸与读不到的竞态进程）。目录枚举本身失败才报错。
fn read_processes() -> Vec<ProbeProcess> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut processes = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        if let Some(process) = read_process(pid) {
            processes.push(process);
        }
    }
    processes
}

fn read_process(pid: u32) -> Option<ProbeProcess> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (state, ppid, comm) = parse_stat_line(&stat)?;
    // 僵尸无内存可展示，也不占用端口，直接跳过（ADR-028 口径）
    if state == 'Z' {
        return None;
    }
    let rss_bytes = fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|status| parse_vm_rss_kb(&status))
        .map(|kb| kb.saturating_mul(1024));
    let command = fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .and_then(|bytes| parse_cmdline(&bytes));
    let name = super::display_name(&comm, command.as_deref().unwrap_or_default());
    let exe_path = exe_link(pid);
    Some(ProbeProcess {
        pid,
        ppid,
        name,
        command: command.map(|parts| parts.join(" ")),
        exe_path,
        rss_bytes,
    })
}

/// /proc/[pid]/exe 链接目标。readlink 需要同 UID（ptrace 读权限），跨 UID
/// 进程取不到就留空。
fn exe_link(pid: u32) -> Option<String> {
    fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
}

/// /proc/[pid]/stat：comm 可含空格与括号，按**最后一个** `)` 切分；
/// 其后字段序是 state、ppid。解析失败返回 None（竞态下进程可能已消失）。
fn parse_stat_line(stat: &str) -> Option<(char, u32, String)> {
    let close = stat.rfind(')')?;
    let rest = stat[close + 1..].trim_start();
    let mut fields = rest.split(' ');
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse().ok()?;
    let comm = stat[stat.find('(')? + 1..close].to_owned();
    Some((state, ppid, comm))
}

/// /proc/[pid]/status 的 `VmRSS:` 行，返回 KiB。内核线程没有该行。
fn parse_vm_rss_kb(status: &str) -> Option<u64> {
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// cmdline 以 NUL 分隔成 argv；内核线程为空。展示时以空格连接（read_process）。
fn parse_cmdline(bytes: &[u8]) -> Option<Vec<String>> {
    let parts = String::from_utf8_lossy(bytes)
        .split('\0')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    (!parts.is_empty()).then_some(parts)
}

/// /proc/net/tcp{,6} 数据行 → (本地地址, 本地端口, inode)。非 LISTEN 状态行
/// 返回 None：侧边栏只关心谁在监听（ADR-028 口径）。
fn parse_net_table_line(line: &str) -> Option<(IpAddr, u16, u32)> {
    let mut fields = line.split_whitespace();
    let _sl = fields.next()?;
    let local = fields.next()?;
    let _remote = fields.next()?;
    let state = fields.next()?;
    if state != "0A" {
        return None;
    }
    // 字段序：sl local rem st tx:rx tr:tm retrnsmt uid timeout inode
    let inode = fields.nth(5)?.parse().ok()?;
    let (addr_text, port_text) = local.split_once(':')?;
    let port = u16::from_str_radix(port_text, 16).ok()?;
    let addr = if addr_text.len() == 8 {
        IpAddr::V4(parse_ipv4_hex(addr_text)?)
    } else {
        IpAddr::V6(parse_ipv6_hex(addr_text)?)
    };
    Some((addr, port, inode))
}

/// /proc/net/tcp 的 IPv4 地址：8 个 hex 字符，按小端字节序存放。
fn parse_ipv4_hex(text: &str) -> Option<Ipv4Addr> {
    let raw = u32::from_str_radix(text, 16).ok()?;
    Some(Ipv4Addr::from(raw.swap_bytes()))
}

/// /proc/net/tcp6 的 IPv6 地址：32 个 hex 字符、4 组每组 8 个（每组是一个
/// 小端 u32），组间正序。
fn parse_ipv6_hex(text: &str) -> Option<Ipv6Addr> {
    if text.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (group, chunk) in text.as_bytes().chunks(8).enumerate() {
        let raw = u32::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
        bytes[group * 4..group * 4 + 4].copy_from_slice(&raw.swap_bytes().to_be_bytes());
    }
    Some(Ipv6Addr::from(bytes))
}

/// fd 链接目标 `socket:[<inode>]`。
fn parse_socket_inode(link: &str) -> Option<u32> {
    link.strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::super::collect_sample;
    use super::{
        parse_cmdline, parse_ipv4_hex, parse_ipv6_hex, parse_net_table_line, parse_socket_inode,
        parse_stat_line, parse_vm_rss_kb,
    };

    /// comm 含空格与括号：必须按最后一个 `)` 切，state 与 ppid 不串位。
    #[test]
    fn stat_line_splits_on_last_paren() {
        let (state, ppid, comm) =
            parse_stat_line("42 (sh -c (a)b)) S 40 42 42 0 -1 4194624").expect("合法 stat 行");
        assert_eq!(state, 'S');
        assert_eq!(ppid, 40);
        assert_eq!(comm, "sh -c (a)b)");
    }

    #[test]
    fn vm_rss_reads_kib_value() {
        assert_eq!(
            parse_vm_rss_kb("VmRSS:\t  1234 kB\nThreads:\t1\n"),
            Some(1234)
        );
        assert_eq!(parse_vm_rss_kb("Threads:\t1\n"), None);
    }

    #[test]
    fn cmdline_joins_nul_parts() {
        assert_eq!(
            parse_cmdline(b"make\0-j8\0\0"),
            Some(vec!["make".to_owned(), "-j8".to_owned()])
        );
        assert_eq!(parse_cmdline(b"\0\0"), None, "内核线程空 cmdline");
    }

    /// 127.0.0.1:8080 的 LISTEN 行（真实格式，端口 hex）。
    #[test]
    fn net_table_line_parses_listen_ipv4() {
        let line = "  1: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 99345 1 0000000000000000 100 0 0 10 0";
        let (addr, port, inode) = parse_net_table_line(line).expect("LISTEN 行应解析");
        assert_eq!(addr, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(port, 8080);
        assert_eq!(inode, 99345);
        // 非 LISTEN 状态被过滤
        let established = line.replace(" 0A ", " 01 ");
        assert!(parse_net_table_line(&established).is_none());
    }

    /// ::1 的 LISTEN 行（tcp6 每 4 字节一组小端）。
    #[test]
    fn net_table_line_parses_listen_ipv6() {
        let line = "  2: 00000000000000000000000001000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 99400 1 0000000000000000 100 0 0 10 0";
        let (addr, port, _) = parse_net_table_line(line).expect("LISTEN 行应解析");
        assert_eq!(addr, IpAddr::V6(Ipv6Addr::LOCALHOST));
        assert_eq!(port, 8080);
    }

    #[test]
    fn hex_addresses_swap_byte_order() {
        assert_eq!(parse_ipv4_hex("0100007F"), Some(Ipv4Addr::LOCALHOST));
        assert_eq!(parse_ipv4_hex("00000000"), Some(Ipv4Addr::UNSPECIFIED));
        assert_eq!(parse_ipv4_hex("short"), None);
        assert_eq!(
            parse_ipv6_hex("00000000000000000000000001000000"),
            Some(Ipv6Addr::LOCALHOST)
        );
        assert_eq!(parse_ipv6_hex("0100007F"), None, "长度不符按无效处理");
    }

    #[test]
    fn socket_inode_parses_from_fd_link() {
        assert_eq!(parse_socket_inode("socket:[99345]"), Some(99345));
        assert_eq!(parse_socket_inode("pipe:[12]"), None);
    }

    /// 真实 /proc 端到端：树根、名称与驻留内存都要到位。`sh -c` 单命令会被
    /// exec 优化成 sleep 本身，因此只断言根存在与根即采样锚点。
    #[test]
    fn collect_reports_real_process_tree() {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .spawn()
            .expect("sh 应能启动");
        let pid = child.id();
        // 等 sh exec 成 sleep
        std::thread::sleep(std::time::Duration::from_millis(300));

        let sample = collect_sample(pid).expect("采集应成功");
        assert_eq!(sample.root.process.pid, pid, "树根即采样锚点");
        assert!(
            sample.root.process.name.contains("sleep"),
            "sh 已 exec 成 sleep：{}",
            sample.root.process.name
        );
        assert!(
            sample.root.process.rss_bytes.is_some_and(|rss| rss > 0),
            "根进程应有驻留内存"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// 真实端口与两级树端到端：python3（不 exec 优化）→ sleep 的树，以及
    /// 树内进程 LISTEN 的端口。环境没有 python3 时打印原因并跳过。
    #[test]
    fn collect_reports_listening_port_of_tree_process() {
        let mut server = match std::process::Command::new("python3")
            .args([
                "-c",
                // 起 http 监听（随机端口）再挂一个子进程：一条命令同时验证两级树与端口
                "import http.server, subprocess, threading\n\
                 httpd = http.server.HTTPServer(('127.0.0.1', 0), http.server.SimpleHTTPRequestHandler)\n\
                 threading.Thread(target=httpd.serve_forever, daemon=True).start()\n\
                 subprocess.run(['sleep', '30'])",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(server) => server,
            Err(error) => {
                eprintln!("跳过：无法启动 python3（{error}）");
                return;
            }
        };
        let pid = server.id();
        let mut found_port = false;
        let mut found_child = false;
        for _ in 0..25 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            if let Ok(sample) = collect_sample(pid) {
                let names: Vec<&str> = sample
                    .root
                    .walk()
                    .iter()
                    .map(|(node, _)| node.process.name.as_str())
                    .collect();
                found_child = names.iter().any(|name| name.contains("sleep"));
                found_port = sample.ports.iter().any(|port| {
                    port.pid == pid && port.addrs.iter().any(|addr| addr.is_loopback())
                });
                if found_port && found_child {
                    break;
                }
            }
        }
        let _ = server.kill();
        let _ = server.wait();
        assert!(found_child, "5 秒内应看到 python3 → sleep 两级树");
        assert!(found_port, "5 秒内应看到 python3 的 LISTEN 端口");
    }
}
