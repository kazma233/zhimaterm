//! 用 Zig 从 Ghostty 官方源码编译 libghostty-vt 并静态链接（ADR-051）。
//!
//! 源码解析顺序：
//!
//! 1. `GHOSTTY_SOURCE_DIR`：Ghostty checkout 或解包后的源码 tarball。
//! 2. 下载 `GHOSTTY_SOURCE_URL`（默认取 `GHOSTTY.lock` 里的 url），
//!    并按 `GHOSTTY.lock` 的 sha256 校验。
//!
//! 源码树先拷进 `$OUT_DIR/src` 再跑 Zig：Zig 的构建产物（zig-out/zig-cache）
//! 会写进源码目录，不能污染使用者的 checkout。
//!
//! 改编自 libghostty-vt-sys 0.2.2（MIT OR Apache-2.0，经 ghostty-vt-sys
//! 0.8.0+ghostty.1.4.0-main.83fb956）；裁掉了子模块、pkg-config 与发布版本
//! 校验分支，本仓只有「pin 源码包自建」一条路径。

use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

/// 按 `GHOSTTY.lock` pin 的 Ghostty 源码。
struct Lock {
  commit: String,
  version: String,
  url: String,
  sha256: String,
  root: String,
}

impl Lock {
  fn read(path: &Path) -> Self {
    let text = fs::read_to_string(path).unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    let field = |name: &str| -> String {
      text
        .lines()
        .filter_map(|line| line.split_once('='))
        .find(|(key, _)| key.trim() == name)
        .map(|(_, value)| value.trim().to_owned())
        .unwrap_or_else(|| panic!("{} has no `{name}` field", path.display()))
    };
    Self {
      commit: field("commit"),
      version: field("version"),
      url: field("url"),
      sha256: field("sha256"),
      root: field("root"),
    }
  }
}

fn main() {
  println!("cargo:rerun-if-changed=build.rs");
  println!("cargo:rerun-if-changed=GHOSTTY.lock");
  for knob in [
    "GHOSTTY_SOURCE_DIR",
    "GHOSTTY_SOURCE_URL",
    "GHOSTTY_ZIG_SYSTEM_DIR",
    "GHOSTTY_VT_CPU",
    "ZIG",
  ] {
    println!("cargo:rerun-if-env-changed={knob}");
  }

  let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
  let lock = Lock::read(&manifest_dir.join("GHOSTTY.lock"));
  let target = env::var("TARGET").expect("TARGET");

  if let Some(dir) = env::var_os("GHOSTTY_SOURCE_DIR") {
    let dir = PathBuf::from(dir);
    assert!(
      dir.join("build.zig").exists(),
      "GHOSTTY_SOURCE_DIR does not contain build.zig: {}",
      dir.display()
    );
    build_from(&dir, &lock, &target);
    return;
  }

  let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
  let src = out_dir.join("src");
  download_and_extract(&lock, &src);
  build_tree(&src, &lock, &target);
}

/// 把源码树拷进 `$OUT_DIR/src` 再构建（原因见模块文档）。
fn build_from(source: &Path, lock: &Lock, target: &str) {
  let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
  let src = out_dir.join("src");
  if src.exists() {
    fs::remove_dir_all(&src).unwrap_or_else(|error| panic!("failed to remove {}: {error}", src.display()));
  }
  copy_tree(source, &src);
  build_tree(&src, lock, target);
}

/// 拷贝 Ghostty 源码树，跳过 git 元数据与 Zig 产物目录。
fn copy_tree(from: &Path, to: &Path) {
  fs::create_dir_all(to).unwrap_or_else(|error| panic!("failed to create {}: {error}", to.display()));
  let entries = fs::read_dir(from).unwrap_or_else(|error| panic!("failed to read {}: {error}", from.display()));
  for entry in entries {
    let entry = entry.unwrap_or_else(|error| panic!("failed to read {}: {error}", from.display()));
    let name = entry.file_name();
    if matches!(
      name.to_str(),
      Some(".git" | ".zig-cache" | "zig-cache" | "zig-out" | "zig-pkg")
    ) {
      continue;
    }
    let path = entry.path();
    let dest = to.join(&name);
    let kind = entry
      .file_type()
      .unwrap_or_else(|error| panic!("failed to stat {}: {error}", path.display()));
    if kind.is_dir() {
      copy_tree(&path, &dest);
    } else if kind.is_file() {
      fs::copy(&path, &dest)
        .unwrap_or_else(|error| panic!("failed to copy {} to {}: {error}", path.display(), dest.display()));
    }
  }
}

/// 下载 pin 的 tarball，校验 sha256 后解包到 `src`。
fn download_and_extract(lock: &Lock, src: &Path) {
  let url = env::var("GHOSTTY_SOURCE_URL").unwrap_or_else(|_| lock.url.clone());
  let stamp = src.join(".ghostty-vt-sha256");
  if fs::read_to_string(&stamp).is_ok_and(|s| s.trim() == lock.sha256) && src.join("build.zig").exists() {
    return;
  }
  if src.exists() {
    fs::remove_dir_all(src).unwrap_or_else(|error| panic!("failed to remove {}: {error}", src.display()));
  }

  let bytes = match fetch(&url) {
    Ok(bytes) => bytes,
    Err(error) => panic!("{}", download_failure(lock, &url, &error)),
  };
  let actual = format!("{:x}", Sha256::digest(&bytes));
  if actual != lock.sha256 {
    panic!(
      "{}",
      download_failure(lock, &url, &format!("sha256 mismatch: got {actual}"))
    );
  }

  let decoder = flate2::read::GzDecoder::new(bytes.as_slice());
  let mut archive = tar::Archive::new(decoder);
  let entries = archive
    .entries()
    .unwrap_or_else(|error| panic!("failed to read tarball: {error}"));
  fs::create_dir_all(src).unwrap_or_else(|error| panic!("failed to create {}: {error}", src.display()));
  let root = Path::new(&lock.root);
  for entry in entries {
    let mut entry = entry.unwrap_or_else(|error| panic!("failed to read tarball entry: {error}"));
    // git archive 会写一条携带 commit id 的 pax 全局头，不是文件条目。
    let kind = entry.header().entry_type();
    if matches!(kind, tar::EntryType::XGlobalHeader | tar::EntryType::XHeader) {
      continue;
    }
    let path = entry
      .path()
      .unwrap_or_else(|error| panic!("failed to read tarball entry path: {error}"))
      .into_owned();
    let Ok(rel) = path.strip_prefix(root) else {
      panic!(
        "tarball entry {} is outside the expected root {}",
        path.display(),
        root.display()
      );
    };
    if rel.as_os_str().is_empty() {
      continue;
    }
    let dest = src.join(rel);
    entry
      .unpack(&dest)
      .unwrap_or_else(|error| panic!("failed to extract {}: {error}", dest.display()));
  }
  fs::write(&stamp, &lock.sha256).unwrap_or_else(|error| panic!("failed to write {}: {error}", stamp.display()));
}

fn fetch(url: &str) -> Result<Vec<u8>, String> {
  use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
  // Cargo.toml 按构建主机选了 TLS 后端（macOS/Windows 系统栈、其余 rustls），
  // ureq 侧要显式指定成同一个；信任锚也用平台系统根：ureq 默认的内置
  // Mozilla 根集不含 pin 站点链路的锚（Cloudflare 走 GlobalSign ECC
  // Root CA - R4 交叉签发，该根已从 Mozilla 列表移除），信任系统证书库
  // 才能下载，与 cargo/git 等构建工具的口径一致。
  let provider = if cfg!(any(target_os = "macos", windows)) {
    TlsProvider::NativeTls
  } else {
    TlsProvider::Rustls
  };
  let agent: ureq::Agent = ureq::Agent::config_builder()
    .tls_config(
      TlsConfig::builder()
        .provider(provider)
        .root_certs(RootCerts::PlatformVerifier)
        .build(),
    )
    .build()
    .into();
  let response = agent.get(url).call().map_err(|error| error.to_string())?;
  let mut bytes = Vec::new();
  response
    .into_body()
    .into_reader()
    .read_to_end(&mut bytes)
    .map_err(|error| error.to_string())?;
  Ok(bytes)
}

fn download_failure(lock: &Lock, url: &str, error: &str) -> String {
  format!(
    "\n\
         ghostty-vt-sys could not obtain the Ghostty source tarball.\n\
         \n\
         error:    {error}\n\
         url:      {url}\n\
         sha256:   {sha}\n\
         commit:   {commit}\n\
         \n\
         Without network access, point GHOSTTY_SOURCE_DIR at a Ghostty checkout or\n\
         an extracted libghostty-vt source tarball at commit {commit}, or set\n\
         GHOSTTY_SOURCE_URL to a mirror of the tarball. The tarball is reproducible\n\
         from a Ghostty checkout with:\n\
         \n\
         \x20 printf '{version}' > LIB_VERSION\n\
         \x20 git archive --format=tgz --prefix={root}/ --add-file=LIB_VERSION \\\n\
         \x20   --prefix={root}/ -o libghostty-vt-source.tar.gz {commit} \\\n\
         \x20   $(for d in images macos dist/doxygen dist/linux dist/macos dist/windows \\\n\
         \x20     flatpak snap po example test src/font/res src/crash/testdata \\\n\
         \x20     pkg/wuffs/src/too_big.jpg pkg/wuffs/src/too_big.png pkg/breakpad/vendor \\\n\
         \x20     vendor; do printf ':(exclude)%s ' $d; done)\n",
    sha = lock.sha256,
    commit = lock.commit,
    version = lock.version,
    root = lock.root,
  )
}

/// 在 `src` 里跑 `zig build` 并输出链接指令。
fn build_tree(src: &Path, lock: &Lock, target: &str) {
  let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
  let zig = resolve_zig();

  // 源码包带 LIB_VERSION（checkout 没有，git 历史外才有版本），缺省回落
  // 到 lock.version。
  let version = fs::read_to_string(src.join("LIB_VERSION"))
    .map(|v| v.trim().to_owned())
    .unwrap_or_else(|_| lock.version.clone());
  let prefix = out_dir.join("zig-out");
  let cache = out_dir.join("zig-cache");
  let cpu = env::var("GHOSTTY_VT_CPU").unwrap_or_else(|_| "baseline".to_owned());

  let mut build = Command::new(&zig);
  build
    .arg("build")
    .arg("-Demit-lib-vt")
    .arg("-Doptimize=ReleaseFast")
    .arg("-Dsimd=true")
    .arg(format!("-Dtarget={}", zig_target(target)))
    .arg(format!("-Dcpu={cpu}"))
    .arg(format!("-Dversion-string={version}"))
    .arg("-Demit-xcframework=false")
    .arg("-Demit-themes=false")
    .arg("--prefix")
    .arg(&prefix)
    .arg("--cache-dir")
    .arg(&cache)
    .current_dir(src);

  if let Some(dir) = env::var_os("GHOSTTY_ZIG_SYSTEM_DIR").map(PathBuf::from) {
    assert!(
      dir.is_dir(),
      "GHOSTTY_ZIG_SYSTEM_DIR is not a directory: {}",
      dir.display()
    );
    build.arg("--system").arg(&dir);
  }

  run(build, "zig build");

  let lib_dir = prefix.join("lib");
  let lib_name = if target.ends_with("windows-msvc") {
    "ghostty-vt-static"
  } else {
    "ghostty-vt"
  };
  let archive = if target.contains("windows") {
    lib_dir.join(format!("{lib_name}.lib"))
  } else {
    lib_dir.join(format!("lib{lib_name}.a"))
  };
  assert!(archive.exists(), "zig build did not produce {}", archive.display());
  let include_dir = prefix.join("include");
  assert!(
    include_dir.join("ghostty").join("vt.h").exists(),
    "zig build did not install headers in {}",
    include_dir.display()
  );

  println!("cargo:rustc-link-search=native={}", lib_dir.display());
  println!("cargo:rustc-link-lib=static={lib_name}");
  println!("cargo:include={}", include_dir.display());
}

fn resolve_zig() -> PathBuf {
  if let Some(zig) = env::var_os("ZIG").map(PathBuf::from) {
    check_zig_version(&zig);
    return zig;
  }

  let path_zig = PathBuf::from("zig");
  let path_result = zig_version(&path_zig);
  if matches!(&path_result, Ok(version) if version.starts_with("0.17.")) {
    return path_zig;
  }

  match path_result {
    Ok(version) => panic!(
      "ghostty-vt-sys needs Zig 0.17.x, found `{version}` at `zig`.\n\
             Install Zig 0.17.x and put it on PATH, or set ZIG to its binary."
    ),
    Err(error) => panic!(
      "ghostty-vt-sys could not find Zig 0.17.x: {error}\n\
             Install Zig 0.17.x and put it on PATH, or set ZIG to its binary."
    ),
  }
}

fn check_zig_version(zig: &Path) {
  let version = zig_version(zig).unwrap_or_else(|error| panic!("{error}"));
  assert!(
    version.starts_with("0.17."),
    "ghostty-vt-sys needs Zig 0.17.x, found `{version}` at `{}`.\n\
         Install Zig 0.17.x and put it on PATH, or set ZIG to its binary.",
    zig.display()
  );
}

fn zig_version(zig: &Path) -> Result<String, String> {
  let output = Command::new(zig)
    .arg("version")
    .output()
    .map_err(|error| format!("failed to run `{} version`: {error}", zig.display()))?;
  let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
  if !output.status.success() {
    return Err(format!(
      "`{} version` failed with status {}",
      zig.display(),
      output.status
    ));
  }
  Ok(version)
}

fn run(mut command: Command, context: &str) {
  let status = command
    .status()
    .unwrap_or_else(|error| panic!("failed to execute {context}: {error}"));
  assert!(status.success(), "{context} failed with status {status}");
}

fn zig_target(target: &str) -> &'static str {
  match target {
    "x86_64-unknown-linux-gnu" => "x86_64-linux-gnu",
    "x86_64-unknown-linux-musl" => "x86_64-linux-musl",
    "aarch64-unknown-linux-gnu" => "aarch64-linux-gnu",
    "aarch64-unknown-linux-musl" => "aarch64-linux-musl",
    "x86_64-apple-darwin" => "x86_64-macos",
    "aarch64-apple-darwin" => "aarch64-macos",
    "x86_64-pc-windows-msvc" => "x86_64-windows-msvc",
    "aarch64-pc-windows-msvc" => "aarch64-windows-msvc",
    "aarch64-apple-ios" => "aarch64-ios",
    "aarch64-apple-ios-sim" => "aarch64-ios-simulator",
    other => panic!(
      "ghostty-vt-sys has no Zig target for `{other}`; supported targets are \
             x86_64 and aarch64 linux-gnu, linux-musl, macos and windows-msvc, plus \
             aarch64-apple-ios and aarch64-apple-ios-sim"
    ),
  }
}
