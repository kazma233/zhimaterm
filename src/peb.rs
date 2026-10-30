//! 跨进程读 PEB 参数块（RTL_USER_PROCESS_PARAMETERS）：Windows 上获取**其他**
//! 进程当前目录与完整命令行的唯一非侵入途径——公开 API（GetCurrentDirectory /
//! GetCommandLine）只答自己，跨进程没有文档化接口。消费方：cwd.rs（恢复上次
//! 工作目录，ADR-039）、probe（完整 argv，ADR-028）。
//!
//! 布局契约：参数块前半被 windows-sys 与系统头文件（winternl.h）一样整体标成
//! Reserved，CurrentDirectory 藏在里面，只能按保留区结构定偏移；CommandLine
//! 是显式字段，用 `offset_of!` 定位。偏移是运行期契约（ADR-028 的 libproc
//! 教训：类型检查抓不到布局错误），由下方对照系统权威答案的自身进程测试实证。

use std::path::PathBuf;

use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Threading::{
    OpenProcess, PEB, PROCESS_BASIC_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_VM_READ, RTL_USER_PROCESS_PARAMETERS,
};

/// 参数块保留区里 CurrentDirectory（CURDIR）的位置：保留区从 ConsoleHandle
/// 起（[0]）、ConsoleFlags（[1]）、三个标准句柄（[2..5]），之后才是路径的
/// UNICODE_STRING——即公开布局的 0x38（x64）/ 0x24（x86）。指针宽度随
/// target 自适应，两种架构同一式。
const CURRENT_DIRECTORY_OFFSET: usize =
    core::mem::offset_of!(RTL_USER_PROCESS_PARAMETERS, Reserved2)
        + 5 * core::mem::size_of::<*const core::ffi::c_void>();

/// UNICODE_STRING 里 Buffer 指针的字节偏移（Length u16 + MaximumLength u16 + 对齐垫）。
const UNICODE_STRING_BUFFER_OFFSET: usize = 8;

/// 进程的当前工作目录。进程已退出、受保护（提权/系统进程）或读取失败时 None。
pub(crate) fn current_directory(pid: u32) -> Option<PathBuf> {
    let reader = ProcessReader::open(pid)?;
    let block = reader.parameters_block()?;
    let (bytes, buffer) = unicode_string_at(&block, CURRENT_DIRECTORY_OFFSET)?;
    let path = reader.read_utf16(buffer, bytes)?;
    (!path.is_empty()).then_some(PathBuf::from(path))
}

/// 进程的完整命令行（原始形态，含引号）。切成 argv 由消费方决定口径。
pub(crate) fn command_line(pid: u32) -> Option<String> {
    let reader = ProcessReader::open(pid)?;
    let block = reader.parameters_block()?;
    let (bytes, buffer) = unicode_string_at(
        &block,
        core::mem::offset_of!(RTL_USER_PROCESS_PARAMETERS, CommandLine),
    )?;
    reader.read_utf16(buffer, bytes)
}

/// 目标进程地址只能当远程数值传递，绝不能解引用成引用——统一经
/// ReadProcessMemory 拷回本地再解析。
struct ProcessReader {
    process: windows_sys::Win32::Foundation::HANDLE,
}

impl ProcessReader {
    fn open(pid: u32) -> Option<Self> {
        // SAFETY: 只传标量参数；失败返回 null 句柄，由下方判空短路。
        let process =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid) };
        (!process.is_null()).then_some(Self { process })
    }

    fn read(&self, address: usize, buffer: &mut [u8]) -> Option<()> {
        let mut read = 0usize;
        // SAFETY: 远程地址只作数值传递（不解引用）；本地缓冲长度与调用一致，
        // 实际读取字节数返回后逐一核对。
        let ok = unsafe {
            ReadProcessMemory(
                self.process,
                address as *const core::ffi::c_void,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut read,
            )
        };
        (ok != 0 && read == buffer.len()).then_some(())
    }

    /// 参数块整块读回：PBI → PEB.ProcessParameters 指针 → 参数块字节。
    fn parameters_block(&self) -> Option<Vec<u8>> {
        // SAFETY: 全零是 PROCESS_BASIC_INFORMATION 的有效初值，仅作输出缓冲。
        let mut basic: PROCESS_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: 输出缓冲的类型与长度和 ProcessBasicInformation 类别匹配；
        // NTSTATUS 为负时下方短路。
        let status = unsafe {
            NtQueryInformationProcess(
                self.process,
                ProcessBasicInformation,
                (&mut basic as *mut PROCESS_BASIC_INFORMATION).cast(),
                core::mem::size_of::<PROCESS_BASIC_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        // NTSTATUS 成功是 >= 0（含 WARNING）
        if status < 0 {
            return None;
        }
        let mut pointer = [0u8; core::mem::size_of::<usize>()];
        self.read(
            basic.PebBaseAddress as usize + core::mem::offset_of!(PEB, ProcessParameters),
            &mut pointer,
        )?;
        let parameters = usize::from_le_bytes(pointer.try_into().ok()?);
        let mut block = vec![0u8; core::mem::size_of::<RTL_USER_PROCESS_PARAMETERS>()];
        self.read(parameters, &mut block)?;
        Some(block)
    }

    /// 读远程 UTF-16 串并截掉结尾 NUL。长度上限只防野值：路径与命令行的
    /// 真实上限远小于此。
    fn read_utf16(&self, address: usize, byte_len: usize) -> Option<String> {
        const MAX_BYTES: usize = 1 << 20;
        if address == 0 || byte_len == 0 || byte_len % 2 != 0 || byte_len > MAX_BYTES {
            return None;
        }
        let mut bytes = vec![0u8; byte_len];
        self.read(address, &mut bytes)?;
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let end = units
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(units.len());
        Some(String::from_utf16_lossy(&units[..end]))
    }
}

impl Drop for ProcessReader {
    fn drop(&mut self) {
        // SAFETY: 句柄由 open 独占取得，Drop 是唯一释放点。
        unsafe { CloseHandle(self.process) };
    }
}

/// 参数块字节里取一个 UNICODE_STRING：返回（字节数, 远程缓冲地址）。
fn unicode_string_at(block: &[u8], offset: usize) -> Option<(usize, usize)> {
    let bytes = usize::from(u16::from_le_bytes(
        block.get(offset..offset + 2)?.try_into().ok()?,
    ));
    let buffer = usize::from_le_bytes(
        block
            .get(offset + UNICODE_STRING_BUFFER_OFFSET..)?
            .get(..core::mem::size_of::<usize>())?
            .try_into()
            .ok()?,
    );
    Some((bytes, buffer))
}

#[cfg(test)]
mod tests {
    use super::{command_line, current_directory};

    /// 布局契约的实机实证：读自己必须与系统给自身的权威答案一致——偏移
    /// 错了这里立即爆（ADR-028 教训的对应护栏）。
    #[test]
    fn reads_own_process_parameters() {
        let cwd = current_directory(std::process::id()).expect("应能读到自身 cwd");
        assert_eq!(
            std::fs::canonicalize(&cwd).expect("读到的目录应存在"),
            std::fs::canonicalize(std::env::current_dir().expect("当前目录应可读"))
                .expect("当前目录应存在")
        );

        let command = command_line(std::process::id()).expect("应能读到自身命令行");
        // SAFETY: GetCommandLineW 返回的指针在进程生命周期内有效且以 NUL 终止，
        // 遍历止于 NUL 后立刻拷出切片。
        let own = unsafe {
            let wide = windows_sys::Win32::System::Environment::GetCommandLineW();
            let mut len = 0usize;
            while *wide.add(len) != 0 {
                len += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(wide, len))
        };
        assert_eq!(command, own, "PEB 命令行应与 GetCommandLineW 一致");
    }

    /// 不存在的进程：句柄拿不到，安静返回 None 而不是报错。
    #[test]
    fn missing_process_yields_none() {
        // 0xFFFFFFF0 是 PID 空间的远端，正常系统上不会命中真实进程
        assert!(super::current_directory(0xFFFFFFF0).is_none());
        assert!(super::command_line(0xFFFFFFF0).is_none());
    }
}
