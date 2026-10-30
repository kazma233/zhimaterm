fn main() {
    // 把 .ico 嵌进 PE 资源段。macOS 读 .app bundle 里的 icns,Linux 读
    // .desktop 条目,只有 Windows 需要编进二进制本身,否则 Explorer/
    // 任务栏/快捷方式显示默认图标。
    #[cfg(target_os = "windows")]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("icons/icon.ico");
        if let Err(e) = res.compile() {
            eprintln!("cargo:warning=winresource failed to embed icon: {e}");
        }
    }
}
