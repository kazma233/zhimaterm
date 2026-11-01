//! Windows 采集：进程表走 ToolHelp 快照（依赖树已有，perf 验收同款），
//! 内存走 `GetProcessMemoryInfo`（WorkingSet，与 RSS 同口径），端口走
//! `GetExtendedTcpTable` 的 OWNER_PID LISTENER 表——这张表只含 LISTEN 行，
//! 与侧边栏「仅监听端口」的口径天然一致（ADR-028）。
//!
//! PID 重用是这里的主要坑：进程退出后 PPID 不清零，重用后的 PID 可能让 ppid 链
//! 成环或串到别人的进程上。防环在公共的树构建里（visited 集合）；本层不再防。

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use anyhow::{Context, Result, bail};

use super::{ListeningSocket, ProbeProcess};
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, STILL_ACTIVE};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, TerminateProcess,
};
use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

/// AF_INET / AF_INET6（iphlpapi 按地址族分表查询）。windows-sys 的常量在
/// WinSock feature 下，为这一个值开整个 feature 不值，字面量与 SDK 一致。
const AF_INET: u32 = 2;
const AF_INET6: u32 = 23;

/// 全量进程表（装配在 mod.rs）。
pub(super) fn processes() -> Result<Vec<ProbeProcess>> {
    list_processes().context("枚举进程失败")
}

/// 全系统 LISTEN socket，v4/v6 两张表各查一次；树内过滤由公共装配完成。
pub(super) fn listening_sockets(
    _tree_pids: &HashSet<u32>,
) -> Result<(Vec<ListeningSocket>, Option<String>)> {
    let mut sockets = Vec::new();
    for (family, ipv6) in [(AF_INET, false), (AF_INET6, true)] {
        sockets.extend(extended_tcp_table(family, ipv6).context("枚举监听端口失败")?);
    }
    Ok((sockets, None))
}

/// 结束进程：`TerminateProcess` 是硬杀——Windows 没有 SIGTERM 等价物，与
/// Unix「先优雅后确认」的口径差异显式接受（ADR-028）。发信号前按采样期的
/// 完整路径复核身份（QueryFullProcessImageNameW 现行值比对），不一致即 PID
/// 已被复用，拒杀；采样期路径缺失（None）时无从比对，交给 OpenProcess 的
/// 存在性检查。「已退出」有两种形态：对象已销毁时 OpenProcess 报 87；对象
/// 仍被某人持有（调用方未回收、句柄存活）时 OpenProcess 成功，须按退出码
/// 判定，对已终止的对象 TerminateProcess 只会得到拒绝访问。
pub(super) fn kill_process(pid: u32, expect_exe: Option<&str>) -> Result<()> {
    if let Some(expected) = expect_exe
        && full_image_path(pid).as_deref() != Some(expected)
    {
        anyhow::bail!("进程 {pid} 的可执行文件已变化（疑似 PID 复用），拒绝结束");
    }
    // SAFETY: 只传标量参数；句柄判空后使用并在块尾关闭。
    unsafe {
        let process = OpenProcess(
            PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        );
        if process.is_null() {
            let error = std::io::Error::last_os_error();
            // 87 = ERROR_INVALID_PARAMETER：OpenProcess 对不存在进程的报错形态
            // （5 = ERROR_ACCESS_DENIED 是权限，照原样透出）
            if error.raw_os_error() == Some(87) {
                anyhow::bail!("进程 {pid} 已退出");
            }
            return Err(anyhow::Error::new(error).context(format!("无法结束进程 {pid}")));
        }
        // STILL_ACTIVE（259）是约定的「仍在运行」哨兵值，与 pty::Child::try_wait
        // 同一口径；真退出码恰为 259 的进程会被误判为仍在，只能硬杀报错
        let mut code: u32 = 0;
        if GetExitCodeProcess(process, &mut code) != 0 && code != STILL_ACTIVE as u32 {
            CloseHandle(process);
            anyhow::bail!("进程 {pid} 已退出");
        }
        let ok = TerminateProcess(process, 1);
        let error = std::io::Error::last_os_error();
        CloseHandle(process);
        if ok == 0 {
            anyhow::bail!("结束进程 {pid} 失败：{error}");
        }
    }
    Ok(())
}

/// 全系统进程表。命令行与解释器脚本名经 PEB 恢复（见 `peb` 模块）。
fn list_processes() -> Result<Vec<ProbeProcess>> {
    // SAFETY: 只传标量参数；失败返回 INVALID_HANDLE_VALUE 由下方判断。
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        bail!(
            "CreateToolhelp32Snapshot 失败：{}",
            std::io::Error::last_os_error()
        );
    }

    // SAFETY: 全零是 PROCESSENTRY32W 的有效初值（仅标量与定长数组），
    // dwSize 由下一行按结构体大小设置。
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut processes = Vec::new();
    // SAFETY: 快照句柄有效且 dwSize 已设置；entry 作为输出缓冲循环复用，
    // CloseHandle 后句柄不再使用。
    unsafe {
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                processes.push(process_from_entry(&entry));
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    Ok(processes)
}

fn process_from_entry(entry: &PROCESSENTRY32W) -> ProbeProcess {
    let name = String::from_utf16_lossy(
        &entry.szExeFile[..entry
            .szExeFile
            .iter()
            .position(|&ch| ch == 0)
            .unwrap_or(entry.szExeFile.len())],
    );
    let rss_bytes = resident_size(entry.th32ProcessID);
    let argv = command_argv(entry.th32ProcessID);
    let name = match &argv {
        Some(argv) => super::display_name(&name, argv),
        None => name,
    };
    ProbeProcess {
        pid: entry.th32ProcessID,
        ppid: entry.th32ParentProcessID,
        name,
        command: argv.map(|argv| argv.join(" ")),
        exe_path: full_image_path(entry.th32ProcessID),
        rss_bytes,
    }
}

/// 完整命令行（PEB）按系统规则切成 argv：引号语义由 CommandLineToArgvW
/// 单点维护，不自写解析。取不到（进程退出、受保护）返回 None。
fn command_argv(pid: u32) -> Option<Vec<String>> {
    let command = crate::peb::command_line(pid)?;
    let wide: Vec<u16> = command.encode_utf16().chain([0]).collect();
    let mut count = 0i32;
    // SAFETY: wide 以 NUL 终止；失败返回 null 指针由下方判断。
    let argv = unsafe { CommandLineToArgvW(wide.as_ptr(), &mut count) };
    if argv.is_null() {
        return None;
    }
    // SAFETY: argv 指向 count 个 NUL 终止的 UTF-16 串（该 API 的返回契约）；
    // 逐串在 NUL 前拷出后，用 API 要求的 LocalFree 释放整块。
    let parts = unsafe {
        let parts = (0..count.max(0) as usize)
            .map(|index| {
                let part = *argv.add(index);
                let mut len = 0usize;
                while *part.add(len) != 0 {
                    len += 1;
                }
                String::from_utf16_lossy(std::slice::from_raw_parts(part, len))
            })
            .collect::<Vec<_>>();
        windows_sys::Win32::Foundation::LocalFree(argv.cast());
        parts
    };
    (!parts.is_empty()).then_some(parts)
}

/// 完整可执行路径：不走 PEB 的独立公开接口，详情弹层「可执行路径」字段来源。
fn full_image_path(pid: u32) -> Option<String> {
    // SAFETY: 句柄判空后使用，size 先传入缓冲容量；CloseHandle 后不再使用句柄。
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let mut buffer = [0u16; 1024];
        let mut size = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut size);
        CloseHandle(process);
        (ok != 0).then(|| String::from_utf16_lossy(&buffer[..size as usize]))
    }
}

/// 单进程 WorkingSet。OpenProcess 拿不到句柄（权限或竞态回收）时为 None，
/// 进程行仍然展示——内存只是缺一格。
fn resident_size(pid: u32) -> Option<u64> {
    // SAFETY: cb 按结构体大小设置；句柄判空后使用并在块尾关闭。
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return None;
        }
        let mut counters = PROCESS_MEMORY_COUNTERS {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            PageFaultCount: 0,
            PeakWorkingSetSize: 0,
            WorkingSetSize: 0,
            QuotaPeakPagedPoolUsage: 0,
            QuotaPagedPoolUsage: 0,
            QuotaPeakNonPagedPoolUsage: 0,
            QuotaNonPagedPoolUsage: 0,
            PagefileUsage: 0,
            PeakPagefileUsage: 0,
        };
        let ok = GetProcessMemoryInfo(process, &mut counters, counters.cb);
        CloseHandle(process);
        (ok != 0).then_some(counters.WorkingSetSize as u64)
    }
}

/// GetExtendedTcpTable 两段调用：先探大小再取表。参数序是
/// (buffer, size, Order=TRUE, Family, TableClass, Reserved)——Order 与 Family
/// 相邻且类型不同，传反了不会立刻报错，只会查不出任何行。
/// 行按 crate 的结构体布局读（v4 是 6 个 DWORD、v6 含两个 16 字节地址段，
/// 手写字节偏移很容易串位）；dwLocalPort 只有低 16 位有效且为网络字节序，
/// dwLocalAddr 亦是网络字节序。
fn extended_tcp_table(family: u32, ipv6: bool) -> Result<Vec<ListeningSocket>> {
    let mut size: u32 = 0;
    // SAFETY: 探大小与实取两阶段共用：size 指向有效变量；探询阶段 buffer
    // 允许为 null，实取阶段指向按返回大小分配的对齐缓冲。
    let query = |buffer: *mut core::ffi::c_void, size: &mut u32| unsafe {
        GetExtendedTcpTable(
            buffer,
            size,
            // Order=TRUE：按本地地址排序返回
            1,
            family,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        )
    };
    query(std::ptr::null_mut(), &mut size);
    if size == 0 {
        return Ok(Vec::new());
    }
    // 表头是 u32 条目数，行结构全 4 字节对齐字段：按 u32 分配天然满足对齐
    let mut buffer = vec![0u32; size.div_ceil(4) as usize];
    let status = query(buffer.as_mut_ptr().cast(), &mut size);
    if status != 0 {
        bail!("GetExtendedTcpTable 失败：错误码 {status}");
    }

    let entries = buffer[0] as usize;
    // SAFETY: size > 0 保证缓冲至少含表头一个 u32，表头之后紧跟行数组。
    let first_row = unsafe { buffer.as_ptr().add(1) };
    let mut sockets = Vec::with_capacity(entries);
    if ipv6 {
        // SAFETY: 成功返回的缓冲含 dwNumEntries 个 v6 行（LISTENER 表契约）；
        // 按 u32 分配满足行内字段的 4 字节对齐。
        let rows = unsafe {
            std::slice::from_raw_parts(first_row as *const MIB_TCP6ROW_OWNER_PID, entries)
        };
        for row in rows {
            sockets.push(ListeningSocket {
                addr: IpAddr::V6(Ipv6Addr::from(row.ucLocalAddr)),
                port: u16::from_be(row.dwLocalPort as u16),
                pid: row.dwOwningPid,
            });
        }
    } else {
        // SAFETY: 同上，v4 行结构（6 个 DWORD）与表类别匹配。
        let rows = unsafe {
            std::slice::from_raw_parts(first_row as *const MIB_TCPROW_OWNER_PID, entries)
        };
        for row in rows {
            sockets.push(ListeningSocket {
                addr: IpAddr::V4(Ipv4Addr::from(u32::from_be(row.dwLocalAddr))),
                port: u16::from_be(row.dwLocalPort as u16),
                pid: row.dwOwningPid,
            });
        }
    }
    Ok(sockets)
}
