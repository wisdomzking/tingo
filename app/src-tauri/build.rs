use std::path::Path;
use std::process::Command;

fn main() {
    // 录音组件按平台编译，作为资源打进 App：
    //   macOS: swiftc 编译 recorder.swift（ScreenCaptureKit / AVFoundation / WebKit）
    //   Windows: cargo 编译 recorder crate（ffmpeg dshow / Edge / PowerShell）
    let out = Path::new("bin/tingo-recorder");
    std::fs::create_dir_all("bin").unwrap();

    if cfg!(target_os = "windows") {
        println!("cargo:rerun-if-changed=../recorder/src/main.rs");
        println!("cargo:rerun-if-changed=../recorder/Cargo.toml");
        // cargo 产物在 target/recorder/release/tingo-recorder.exe；
        // 复制到 bin/tingo-recorder（无扩展名，与 engine 的 resources.join 契约一致；
        // Windows 下 std::process::Command 会自动补 .exe）
        let built = Path::new("../../target/recorder/release/tingo-recorder.exe");
        let src = Path::new("../recorder/src/main.rs");
        // 产物已存在且不比源码旧时跳过子 cargo，避免重复编译/联网更新索引导致卡住
        let fresh = built.exists()
            && out.exists()
            && out.metadata().and_then(|m| m.modified()).ok()
                >= src.metadata().and_then(|m| m.modified()).ok();
        if !fresh {
            // --offline：recorder 零依赖，无需联网；避免子 cargo 更新索引卡死
            let status = Command::new("cargo")
                .args([
                    "build", "--release", "--offline",
                    "--manifest-path", "../recorder/Cargo.toml",
                    "--target-dir", "../../target/recorder",
                ])
                .status()
                .expect("需要 Rust/cargo 来编译 Windows 录音组件");
            assert!(status.success(), "Windows 录音组件编译失败");
        }
        assert!(built.exists(), "找不到编译产物 {}", built.display());
        std::fs::copy(built, out).expect("复制 Windows 录音组件失败");
    } else {
        let src = Path::new("../../src/tingo/recorder.swift");
        println!("cargo:rerun-if-changed={}", src.display());
        let fresh = out.exists()
            && out.metadata().and_then(|m| m.modified()).ok()
                >= src.metadata().and_then(|m| m.modified()).ok();
        if !fresh {
            let status = Command::new("swiftc")
                .args(["-O", "-target", "arm64-apple-macos13.0", src.to_str().unwrap(), "-o", out.to_str().unwrap()])
                .status()
                .expect("需要 Xcode Command Line Tools（swiftc）来编译录音组件");
            assert!(status.success(), "录音组件编译失败");
        }
    }

    // 公证检查 Resources 里的可执行文件，封装前先签名（仅 macOS 正式签名时）
    println!("cargo:rerun-if-env-changed=APPLE_SIGNING_IDENTITY");
    println!("cargo:rerun-if-changed=Entitlements.plist");
    if cfg!(target_os = "macos") {
        if let Ok(identity) = std::env::var("APPLE_SIGNING_IDENTITY") {
            if !identity.is_empty() && identity != "-" {
                let status = Command::new("codesign")
                    .args(["--force", "--sign", &identity, "--timestamp", "--options", "runtime",
                        "--identifier", "io.github.tancheng33.tingo.recorder",
                        "--entitlements", "Entitlements.plist", out.to_str().unwrap()])
                    .status()
                    .expect("无法启动 codesign");
                assert!(status.success(), "录音组件正式签名失败");
            }
        }
    }
    tauri_build::build()
}
