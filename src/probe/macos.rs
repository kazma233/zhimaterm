//! macOS 采集：进程与内存走 libproc、完整 argv 走 sysctl `KERN_PROCARGS2`（`ps`
//! 同款系统接口，无需特权）、端口归属调系统自带的 `lsof -F`（ADR-028 决策）。
//!
//! 两个天然约束都恰好放行：libproc 与 sysctl/lsof 在非 root 下只能看到自己 UID
//! 的进程，而侧边栏关心的正是本会话 shell 的后代——全在同 UID 里。lsof 缺失或
//! 失败时进程区照常展示，只有端口区降级（`ports_unavailable`）。

use std::collections::HashSet;
use std::ffi::CStr;
use std::net::IpAddr;
use std::process::{Command, Stdio};

use anyhow::{Result, bail};

use super::{ListeningSocket, ProbeProcess};

/// proc_listallpids 的 pid 缓冲上限：按十分宽松的全系统进程数给。
const PID_BUFFER_CAPACITY: usize = 8192;

/// 全量进程表（装配在 mod.rs）。
pub(super) fn processes() -> Result<Vec<ProbeProcess>> {
    list_processes()
}

/// 结束进程：SIGTERM 经 mod.rs 的公共 `sigterm`。发信号前按采样期的可执行
/// 路径复核身份——proc_pidpath 现行值不一致即 PID 已被复用，拒杀；采样期
/// 路径缺失（None）时无从比对，交给 kill 自身的存在性检查。
pub(super) fn kill_process(pid: u32, expect_exe: Option<&str>) -> Result<()> {
    if let Some(expected) = expect_exe
        && process_path(pid as i32).as_deref() != Some(expected)
    {
        anyhow::bail!("进程 {pid} 的可执行文件已变化（疑似 PID 复用），拒绝结束");
    }
    super::sigterm(pid)
}

/// lsof 是全系统输出，树内过滤由公共装配完成，不吃树参数。
pub(super) fn listening_sockets(
    _tree_pids: &HashSet<u32>,
) -> Result<(Vec<ListeningSocket>, Option<String>)> {
    Ok(lsof_listening())
}

/// 全系统进程表。单个 pid 的信息取不到（竞态回收）就跳过该进程。
fn list_processes() -> Result<Vec<ProbeProcess>> {
    let mut pid_buffer = vec![0i32; PID_BUFFER_CAPACITY];
    // SAFETY: 缓冲容量与传给内核的字节数一致；返回负数即失败由下方判断，
    // 非负数是实际写入的 pid 个数，不超过容量。
    let count = unsafe {
        libc::proc_listallpids(
            pid_buffer.as_mut_ptr().cast(),
            (PID_BUFFER_CAPACITY * std::mem::size_of::<i32>()) as i32,
        )
    };
    if count < 0 {
        bail!("proc_listallpids 失败：返回 {count}");
    }
    Ok(pid_buffer[..count as usize]
        .iter()
        .filter_map(|&pid| process_info(pid))
        .collect())
}

/// 全量 argv（sysctl `KERN_PROCARGS2`，ps 同款）。proc_pidpath 只有可执行文件，
/// 解释器跑的脚本与参数都在这里。取不到（权限 / 竞态 / setproctitle 改写）返回
/// None，调用方回退可执行路径。
fn proc_argv(pid: i32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: usize = 0;
    // 首次空 buffer 查询所需大小，按实际大小取整段（含 env，argv 在其前面）
    // SAFETY: 探询阶段 oldp 允许为 null，oldlenp 指向有效变量。
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    let mut buffer = vec![0u8; size];
    // SAFETY: 缓冲按探询到的大小分配，size 同时是入参容量与出参实际长度。
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    buffer.truncate(size);
    parse_procargs(&buffer)
}

/// KERN_PROCARGS2 缓冲区布局：`[argc: i32][可执行路径][NUL 对齐填充][argv 串…]`
/// （其后是 env，与 argv 无边界标记、不解析）。串全部 NUL 结尾；argc 与实际串数
/// 不符（setproctitle 改写中的竞态）按取不到处理。
fn parse_procargs(buffer: &[u8]) -> Option<Vec<String>> {
    if buffer.len() < 5 {
        return None;
    }
    let argc = u32::from_ne_bytes(buffer[..4].try_into().ok()?) as usize;
    if argc == 0 {
        return None;
    }
    let mut cursor = 4;
    // 跳过可执行路径与其后的对齐 NUL，落到 argv[0] 首字节
    while cursor < buffer.len() && buffer[cursor] != 0 {
        cursor += 1;
    }
    while cursor < buffer.len() && buffer[cursor] == 0 {
        cursor += 1;
    }
    let mut argv = Vec::with_capacity(argc.min(128));
    for _ in 0..argc {
        let end = cursor + buffer[cursor..].iter().position(|&byte| byte == 0)?;
        argv.push(String::from_utf8_lossy(&buffer[cursor..end]).into_owned());
        cursor = end + 1;
    }
    (!argv.is_empty()).then_some(argv)
}

fn process_info(pid: i32) -> Option<ProbeProcess> {
    // SAFETY: 全零是 proc_bsdinfo 的有效初值，仅作 proc_pidinfo 的输出缓冲。
    let mut bsd: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    // SAFETY: 结构体布局与函数声明都由 libc 维护、与 sys/proc_info.h 的 C 契约
    // 一致；缓冲长度按结构体大小传入，返回值（写入字节数）核对后才使用——
    // 不等于结构体大小说明进程已竞态回收，跳过。
    let size = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::addr_of_mut!(bsd).cast(),
            std::mem::size_of::<libc::proc_bsdinfo>() as i32,
        )
    };
    if size != std::mem::size_of::<libc::proc_bsdinfo>() as i32 {
        return None;
    }
    let comm = CStr::from_bytes_until_nul(&bsd.pbi_comm.map(|c| c as u8))
        .ok()?
        .to_string_lossy()
        .into_owned();

    // SAFETY: 同上——全零初值仅作输出缓冲，返回字节数核对后使用。
    let mut task: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    // SAFETY: 同上（PROC_PIDTASKINFO 类别与 proc_taskinfo 布局匹配）。
    let task_size = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            std::ptr::addr_of_mut!(task).cast(),
            std::mem::size_of::<libc::proc_taskinfo>() as i32,
        )
    };
    // `pti_resident_size` 是驻留内存，与 Linux VmRSS 同口径
    let rss_bytes = (task_size == std::mem::size_of::<libc::proc_taskinfo>() as i32)
        .then_some(task.pti_resident_size);

    // 完整路径兜底：argv 取不到时名字取末段、命令行退化为路径；取不到路径时退 comm
    let path = process_path(pid);
    let (exec_name, path) = match &path {
        Some(path) => (
            path.rsplit('/').next().unwrap_or(path).to_owned(),
            Some(path.clone()),
        ),
        None => (comm.clone(), None),
    };
    // argv 是最完整的「启动命令」；解释器跑脚本时内核名是解释器（跑 pi 显示
    // node），名字与命令行都从 argv 恢复。可执行路径独立保留给详情视图
    let (name, command) = match proc_argv(pid).filter(|argv| !argv.is_empty()) {
        Some(argv) => (super::display_name(&exec_name, &argv), Some(argv.join(" "))),
        None => (exec_name, path.clone()),
    };

    Some(ProbeProcess {
        pid: bsd.pbi_pid,
        ppid: bsd.pbi_ppid,
        name,
        command,
        exe_path: path,
        rss_bytes,
    })
}

/// 可执行文件完整路径（proc_pidpath）。取不到（权限 / 竞态回收）返回 None。
fn process_path(pid: i32) -> Option<String> {
    let mut path_buffer = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: 缓冲按 API 约定的 MAXSIZE 分配，长度参数与容量一致；返回正数
    // 是不含 NUL 的路径字节数，据此切片。
    let path_len = unsafe {
        libc::proc_pidpath(
            pid,
            path_buffer.as_mut_ptr().cast(),
            path_buffer.len() as u32,
        )
    };
    (path_len > 0).then(|| String::from_utf8_lossy(&path_buffer[..path_len as usize]).into_owned())
}

/// lsof 的 LISTEN 采集：返回（socket 列表, 端口区不可用原因）。
fn lsof_listening() -> (Vec<ListeningSocket>, Option<String>) {
    let output = Command::new("lsof")
        .args(["-nP", "-F", "pcn", "-iTCP", "-sTCP:LISTEN"])
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            (parse_lsof(&text), None)
        }
        Ok(output) => (
            Vec::new(),
            Some(format!(
                "lsof 退出异常：{}",
                output.status.code().unwrap_or(-1)
            )),
        ),
        Err(error) => (Vec::new(), Some(format!("无法启动 lsof：{error}"))),
    }
}

/// `lsof -F pcn` 输出解析：`p<pid>` 开进程组，`n<addr:port>` 归属最近的 pid，
/// 其余字段（c 等）忽略。同一进程多个 fd 各出一行 n，都被收下后由合并层去重。
fn parse_lsof(output: &str) -> Vec<ListeningSocket> {
    let mut sockets = Vec::new();
    let mut pid: Option<u32> = None;
    for line in output.lines() {
        let Some(tag) = line.chars().next() else {
            continue;
        };
        let value = &line[1..];
        match tag {
            'p' => pid = value.parse().ok(),
            'n' => {
                if let (Some(pid), Some((addr, port))) = (pid, parse_lsof_address(value)) {
                    sockets.push(ListeningSocket { addr, port, pid });
                }
            }
            _ => {}
        }
    }
    sockets
}

/// `n` 字段的地址：`127.0.0.1:8080`、`[::1]:8080`、`*:8080`（通配按 v4 通配计）。
fn parse_lsof_address(value: &str) -> Option<(IpAddr, u16)> {
    let (addr, port) = value.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    let addr = addr
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(addr);
    let addr: IpAddr = if addr == "*" {
        IpAddr::from(std::net::Ipv4Addr::UNSPECIFIED)
    } else {
        addr.parse().ok()?
    };
    Some((addr, port))
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::os::unix::fs::PermissionsExt;

    use super::{ListeningSocket, parse_lsof, parse_lsof_address, parse_procargs, proc_argv};

    /// KERN_PROCARGS2 缓冲区五段：argc、可执行路径、对齐填充、argv、env；
    /// argv 恰好取全，env 不混入。
    #[test]
    fn procargs_parses_argv_and_skips_env() {
        let mut buffer = 3i32.to_ne_bytes().to_vec();
        buffer.extend_from_slice(b"/usr/local/bin/node\0");
        buffer.extend_from_slice(b"\0\0\0");
        buffer.extend_from_slice(b"node\0/usr/local/bin/pi\0--serve\0");
        buffer.extend_from_slice(b"PATH=/usr/bin\0HOME=/tmp\0");
        assert_eq!(
            parse_procargs(&buffer),
            Some(vec![
                "node".to_owned(),
                "/usr/local/bin/pi".to_owned(),
                "--serve".to_owned()
            ])
        );
    }

    /// argc 与实际串数不符（setproctitle 改写中的竞态）：按取不到处理，调用方
    /// 回退可执行路径。
    #[test]
    fn procargs_rejects_inconsistent_and_empty() {
        assert_eq!(parse_procargs(&[]), None);
        assert_eq!(parse_procargs(&0i32.to_ne_bytes()), None, "argc=0 无 argv");
        let mut buffer = 2i32.to_ne_bytes().to_vec();
        buffer.extend_from_slice(b"/bin/sh\0\0only-one\0");
        assert_eq!(parse_procargs(&buffer), None, "串数不足 argc");
    }

    /// 实机端到端：sysctl 读自身 argv（同 UID 无需特权），argv[0] 与 current_exe
    /// 对得上。
    #[test]
    fn proc_argv_reads_own_process() {
        let argv = proc_argv(std::process::id() as i32).expect("自身 argv 应可读");
        let expected = std::env::current_exe()
            .expect("current_exe")
            .file_name()
            .expect("测试二进制文件名")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            argv[0].rsplit('/').next(),
            Some(expected.as_str()),
            "argv[0] 应指向测试二进制：{argv:?}"
        );
    }

    /// 实机端到端：shebang 脚本（用户跑 pi 的真实形态）名字应显示脚本而非解释器。
    #[test]
    fn script_process_shows_script_name() {
        let dir = std::env::temp_dir().join(format!("zhimaterm-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let script = dir.join("pi-fixture.sh");
        std::fs::write(&script, "#!/bin/sh\nsleep 30 &\nwait\n").expect("写脚本");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("加可执行位");

        let mut child = std::process::Command::new(&script)
            .spawn()
            .expect("脚本应能启动");
        let sample = super::super::collect_sample(child.id()).expect("采集应成功");

        assert_eq!(
            sample.root.process.name, "pi-fixture.sh",
            "名字应是脚本而非解释器 sh"
        );
        assert!(
            sample
                .root
                .process
                .command
                .as_deref()
                .unwrap_or_default()
                .contains("pi-fixture.sh"),
            "命令行应含脚本路径：{:?}",
            sample.root.process.command
        );

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真实输出样例（ Fedora 上 `lsof -nP -F pcn -iTCP -sTCP:LISTEN` 对
    /// `python -m http.server` 的采集，macOS 同构）。
    #[test]
    fn lsof_output_groups_addresses_by_pid() {
        let output = "p203884\ncpython3\nn127.0.0.1:18123\np99\nccaddy\nn[::1]:8080\nn*:8080\n";
        let sockets = parse_lsof(output);
        assert_eq!(
            sockets,
            vec![
                ListeningSocket {
                    addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    port: 18123,
                    pid: 203884,
                },
                ListeningSocket {
                    addr: IpAddr::V6(Ipv6Addr::LOCALHOST),
                    port: 8080,
                    pid: 99,
                },
                // 通配绑定按 v4 通配计，端口仍要展示
                ListeningSocket {
                    addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                    port: 8080,
                    pid: 99,
                },
            ]
        );
    }

    /// n 行出现在任何 p 行之前、或地址解析失败：丢弃该行而不是解析 panic。
    #[test]
    fn lsof_tolerates_stray_and_invalid_lines() {
        assert!(parse_lsof("n1.2.3.4:80\n").is_empty());
        assert!(parse_lsof("p1\nnnot-an-address\n").is_empty());
        assert_eq!(parse_lsof("p7\n").len(), 0);
        assert_eq!(parse_lsof_address("[::1]:8080").unwrap().1, 8080);
        assert!(parse_lsof_address("no-port").is_none());
    }
}
