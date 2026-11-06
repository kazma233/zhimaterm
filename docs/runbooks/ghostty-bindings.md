# ghostty 绑定：再生成与更新 pin

libghostty-vt 的 FFI 绑定在 `ffi/ghostty-vt-sys/src/bindings.rs`，由
`src/bin/gen_bindings.rs` 对官方头文件 `include/ghostty/vt.h` **预生成提交**
（ADR-051）。allowlist 是精确清单（约 30 个函数 + 所用类型），生成物约
1600 行；日常构建只依赖 Zig，不需要 bindgen/libclang。C API 的机制口径
（tagged get/set、sized 结构、回调协议等）见
[libghostty-vt-api-notes](../context/libghostty-vt-api-notes.md)。

## 1. 头文件从哪来

生成工具按以下顺序找头文件（取先命中者）：

1. `GHOSTTY_INCLUDE_DIR`：直接指向含 `ghostty/vt.h` 的 include 目录；
2. `GHOSTTY_SOURCE_DIR/include`：本地 Ghostty 源码树；
3. workspace 构建缓存 `target/*/build/ghostty-vt-sys-*/out/src/include`：
   build.rs 下载解包的 pin 源码树——**最省事**，先随便构建一次
   （`cargo build -p ghostty-vt-sys`）就有了。

三者都找不到时工具会报错并指向本 runbook。

## 2. 再生成绑定

改动 allowlist（新增/移除函数或类型）或头文件口径变化后：

```bash
cargo run -p ghostty-vt-sys --features bindgen-tool --bin gen-bindings
```

要点：

- bindgen/heck 是 `bindgen-tool` feature 的可选依赖，只有跑这个工具才编
 译，工具本身需要 libclang（macOS 装 Xcode Command Line Tools 即有）。
- 生成后跑 `cargo test contract`（26 场景 × 3 后端）确认行为不回归；
  新增绑定的消费代码在 `src/terminal/backend/ghostty/`。
- **这个 bin 不被默认收尾检查覆盖**（`--all-targets` 不开 feature）：改过
  它要单独跑
  `cargo clippy -p ghostty-vt-sys --all-targets --features bindgen-tool`。

## 3. 更新 pin（升级 libghostty-vt）

1. 改 `ffi/ghostty-vt-sys/GHOSTTY.lock` 五个字段：`commit`（完整 hash）、
   `version`（`<上游版本>-main+<短hash>`）、`url`（tip.files.ghostty.org 按
   commit 取源码包）、`sha256`、`root`（tarball 顶层目录名）。
   校验和不对会在下载时 panic，并附上从 Ghostty checkout 用 `git archive`
   复现该 tarball 的完整命令（排除清单在内）。
2. 重新生成绑定（见上；libghostty-vt 未到 1.0，C API 会变，allowlist 与
   `vt.rs` 的封装按编译错误与 vt.h 文档逐个对齐）。
3. `cargo test contract` + 真实使用冒烟（表格对齐、TUI、同步输出）。

## 4. 离线构建

```bash
GHOSTTY_SOURCE_DIR=<本地 Ghostty 源码树> cargo build
```

源码树须含 `build.zig`（checkout 或解包的源码包均可）；build.rs 会把它
拷进 `$OUT_DIR/src` 再跑 Zig，不污染原目录。Zig 依赖包的离线缓存另设
`GHOSTTY_ZIG_SYSTEM_DIR`（指向 `zig build --system` 缓存目录）。
