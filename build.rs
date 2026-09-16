// Embeds the app icon (assets/minipdf.ico) into the .exe via rc.exe.
// No build-dependencies: only std is used. If rc.exe cannot be found,
// the build continues without an embedded icon (with a warning).

use std::path::PathBuf;
use std::process::Command;

fn probe(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn find_rc() -> Option<String> {
    // 1. rc.exe on PATH (e.g. VS developer prompt)
    if probe("rc.exe", &["/?"]) {
        return Some("rc.exe".to_owned());
    }
    // 2. WindowsSdkDir env (VS build environment)
    if let Ok(dir) = std::env::var("WindowsSdkDir") {
        let mut p = PathBuf::from(dir);
        p.push("bin");
        if let Ok(ver) = std::env::var("WindowsSDKVersion") {
            p.push(ver);
        } else {
            p.push("x64");
            let c = p.join("rc.exe");
            if c.exists() {
                return Some(c.to_string_lossy().to_string());
            }
            p.pop();
        }
        for arch in ["x64", "x86", "arm64"] {
            let c = p.join(arch).join("rc.exe");
            if c.exists() {
                return Some(c.to_string_lossy().to_string());
            }
        }
    }
    // 3. Well-known Windows Kits location, newest version wins
    if let Ok(kits) = std::fs::read_dir(r"C:\Program Files (x86)\Windows Kits\10\bin") {
        let mut vers: Vec<PathBuf> = kits
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.join("x64").join("rc.exe").exists())
            .collect();
        vers.sort();
        if let Some(v) = vers.pop() {
            return Some(v.join("x64").join("rc.exe").to_string_lossy().to_string());
        }
    }
    None
}

fn main() {
    println!("cargo:rerun-if-changed=assets/minipdf.rc");
    println!("cargo:rerun-if-changed=assets/minipdf.ico");
    println!("cargo:rerun-if-changed=build.rs");

    let rc = match find_rc() {
        Some(r) => r,
        None => {
            println!("cargo:warning=minipdf: rc.exe not found, skipping embedded icon");
            return;
        }
    };
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let res = PathBuf::from(&out_dir).join("minipdf.res");
    let st = Command::new(&rc)
        .args([
            "/nologo",
            "/fo",
            &res.to_string_lossy(),
            "assets/minipdf.rc",
        ])
        .status();
    match st {
        Ok(s) if s.success() && res.exists() => {
            // NOTE: no quotes here — rustc passes the value verbatim to link.exe,
            // and embedded quotes become part of the file name (LNK1104).
            println!("cargo:rustc-link-arg={}", res.to_string_lossy());
        }
        _ => {
            println!("cargo:warning=minipdf: rc.exe failed, skipping embedded icon");
        }
    }
}
