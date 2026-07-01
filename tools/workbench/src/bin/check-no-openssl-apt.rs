//! Guard: no OpenSSL system package may be installed by any Dockerfile.
//!
//! The Rust graph is kept OpenSSL-free by `deny.toml` (the openssl/boring* crate
//! bans), but that can't see `apt` installs. This scans every tracked
//! Dockerfile/Containerfile and fails if a line installs a `libssl*`/`openssl*`
//! package. TLS is rustls throughout; nothing here needs system OpenSSL.

use workbench::{git, read, repo_root, run};

fn main() {
    run("check-no-openssl-apt", check, |n| {
        println!("check-no-openssl-apt: ok — no OpenSSL apt packages in {n} Dockerfile(s)");
    })
}

fn check() -> Result<usize, Vec<String>> {
    let root = repo_root()?;
    let root_str = root
        .to_str()
        .ok_or_else(|| vec!["repo root path is not utf-8".to_string()])?;

    let files: Vec<String> = git(&["-C", root_str, "ls-files"])?
        .lines()
        .filter(|p| {
            let name = p.rsplit('/').next().unwrap_or(p);
            name.contains("Dockerfile") || name.contains("Containerfile")
        })
        .map(str::to_string)
        .collect();

    let mut offenders = Vec::new();
    for rel in &files {
        let contents = read(&root.join(rel))?;
        for (i, line) in contents.lines().enumerate() {
            if is_openssl_package_line(line) {
                offenders.push(format!("{rel}:{}: {}", i + 1, line.trim()));
            }
        }
    }

    if !offenders.is_empty() {
        let mut msg = vec![
            "OpenSSL system package(s) found in a Dockerfile — TLS is rustls, none is needed:"
                .to_string(),
        ];
        msg.extend(offenders);
        msg.push("remove the package, or justify it and update this guard.".to_string());
        return Err(msg);
    }
    Ok(files.len())
}

/// A non-comment line that installs a `libssl*` / `openssl*` apt package.
fn is_openssl_package_line(line: &str) -> bool {
    !line.trim_start().starts_with('#') && (line.contains("libssl") || line.contains("openssl"))
}
