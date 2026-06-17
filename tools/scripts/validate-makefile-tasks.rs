// Validate all Makefile.toml tasks work correctly with minimal execution.
//
// Run by the `validate-makefile-tasks` task via cargo-make's `@rust` script
// runner (script = { file = "tools/scripts/validate-makefile-tasks.rs" } in
// Makefile.toml). IMAGE_NAME and the version vars come from Makefile.toml
// [env].

use std::process::{Command, Stdio};
use std::env;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🔍 Starting validation of all Makefile tasks...");

    // 1. Check version matching
    println!("\nStep 1: Checking version consistency...");
    run_makers_task("check-matching-zebras")?;

    // 2. Compute the image tag
    println!("\nStep 2: Computing image tag...");
    run_makers_task("compute-image-tag")?;

    // 3. Ensure image exists (will build if necessary)
    println!("\nStep 3: Ensuring container image exists...");
    run_makers_task("ensure-image-exists")?;

    // 4. Get the computed tag
    let tag = get_image_tag()?;
    let image_name = env::var("IMAGE_NAME")
        .unwrap_or_else(|_| "zingodevops/zaino-ci".to_string());
    let working_dir = env::current_dir()?.to_string_lossy().to_string();

    // 5. Run a single fast test to validate the full pipeline
    println!("\nStep 4: Running minimal test to validate setup...");
    println!("Using image: {}:{}", image_name, tag);

    let status = Command::new("podman")
        .args(&[
            "run", "--rm",
            "--init",
            "--name", "zaino-validation-test",
            "-v", &format!("{}:/home/container_user/zaino", working_dir),
            "-v", "zaino-container-target:/home/container_user/zaino/target:U",
            "-v", "zaino-cargo-git:/usr/local/cargo/git:U",
            "-v", "zaino-cargo-registry:/usr/local/cargo/registry:U",
            "-e", "TEST_BINARIES_DIR=/home/container_user/zaino/\
                   integration-tests/test_binaries/bins",
            "-w", "/home/container_user/zaino",
            "-u", "container_user",
            &format!("{}:{}", image_name, tag),
            "cargo", "test",
            "--package", "zaino-testutils",
            "--lib", "launch_testmanager::zcashd::basic",
            "--", "--nocapture"
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    if !status.success() {
        eprintln!("❌ Validation failed!");
        std::process::exit(1);
    }

    println!("\n✅ All tasks validated successfully!");
    Ok(())
}

fn run_makers_task(task: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!("DEBUG: About to run makers {}", task);
    let status = Command::new("makers")
        .arg(task)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    println!("DEBUG: makers {} completed with status: {:?}", task, status);
    if !status.success() {
        return Err(format!("Task '{}' failed", task).into());
    }
    Ok(())
}

fn get_image_tag() -> Result<String, Box<dyn std::error::Error>> {
    println!("DEBUG: Getting image tag...");
    // First try to get from environment
    if let Ok(tag) = env::var("CARGO_MAKE_IMAGE_TAG") {
        if !tag.is_empty() {
            println!("DEBUG: Found tag in env: {}", tag);
            return Ok(tag);
        }
    }

    println!("DEBUG: Computing tag with script...");
    // Otherwise compute it
    let output = Command::new("./tools/scripts/get-ci-image-tag.sh")
        .output()?;

    if !output.status.success() {
        return Err("Failed to compute image tag".into());
    }

    let tag = String::from_utf8(output.stdout)?.trim().to_string();
    println!("DEBUG: Computed tag: {}", tag);
    Ok(tag)
}
