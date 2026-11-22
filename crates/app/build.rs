//! Windows exe 资源：把 `assets/app-icon.ico` 嵌进可执行文件。
//!
//! 只在 **Windows 目标**上执行；Linux/macOS 构建不受影响，也不需要资源编译器。
//! `embed-resource` 会优先用 `rc.exe`（自动搜 Windows SDK），GNU 工具链回退
//! `windres`；找不到时构建直接失败，避免静默生成无图标的 exe。

fn main() {
    println!("cargo:rerun-if-changed=assets/app-icon.ico");
    println!("cargo:rerun-if-changed=app-icon.rc");

    #[cfg(windows)]
    {
        use std::env;

        if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
            return;
        }
        embed_resource::compile("app-icon.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("嵌入 Windows 应用图标失败（检查 rc.exe / Windows SDK）");
    }
}
