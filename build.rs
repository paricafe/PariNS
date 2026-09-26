use std::{env, process::Command};

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn main() {
    for name in [
        "PARINS_OFFICIAL_RELEASE",
        "GITHUB_ACTIONS",
        "GITHUB_REPOSITORY",
        "GITHUB_REF",
        "GITHUB_SHA",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
    println!("cargo:rerun-if-changed=.git/index");
    let commit = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let official = env::var("PARINS_OFFICIAL_RELEASE").as_deref() == Ok("1");
    if official {
        let version = env::var("CARGO_PKG_VERSION").expect("package version");
        let tag = format!("refs/tags/v{version}");
        assert_eq!(
            env::var("GITHUB_ACTIONS").as_deref(),
            Ok("true"),
            "official builds require GitHub Actions"
        );
        assert_eq!(
            env::var("GITHUB_REPOSITORY").as_deref(),
            Ok("paricafe/PariNS"),
            "official repository"
        );
        assert_eq!(
            env::var("GITHUB_REF").as_deref(),
            Ok(tag.as_str()),
            "official tag"
        );
        assert_eq!(
            env::var("GITHUB_SHA").as_deref(),
            Ok(commit.as_str()),
            "source SHA mismatch"
        );
        assert_eq!(
            git(&["rev-parse", &format!("{tag}^{{commit}}")]).as_deref(),
            Some(commit.as_str()),
            "tag must resolve to source SHA"
        );
        assert_eq!(
            git(&["status", "--porcelain", "--untracked-files=normal"]).as_deref(),
            Some(""),
            "official source must be clean"
        );
    }
    println!(
        "cargo:rustc-env=PARINS_BUILD_TARGET={}",
        env::var("TARGET").expect("target")
    );
    println!("cargo:rustc-env=PARINS_SOURCE_COMMIT={commit}");
    println!("cargo:rustc-env=PARINS_OFFICIAL={official}");
}
