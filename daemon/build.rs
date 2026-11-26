use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=TIWI_VERSION");

    let git_dir = Path::new("../.git");
    if git_dir.exists() {
        let head = git_dir.join("HEAD");
        println!("cargo:rerun-if-changed={}", head.display());
        if let Ok(contents) = std::fs::read_to_string(&head)
            && let Some(ref_path) = contents.trim().strip_prefix("ref: ")
        {
            println!(
                "cargo:rerun-if-changed={}",
                git_dir.join(ref_path).display()
            );
        }
        let packed = git_dir.join("packed-refs");
        if packed.exists() {
            println!("cargo:rerun-if-changed={}", packed.display());
        }
        let tags = git_dir.join("refs/tags");
        if tags.exists() {
            println!("cargo:rerun-if-changed={}", tags.display());
        }
    }

    let version = std::env::var("TIWI_VERSION")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .or_else(|| git(&["describe", "--tags", "--abbrev=0"]))
        .unwrap_or_else(|| format!("v{}", env!("CARGO_PKG_VERSION")));

    let version = if version.starts_with('v') {
        version
    } else {
        format!("v{version}")
    };

    let commit = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=TIWI_FULL_VERSION={version} ({commit})");
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
