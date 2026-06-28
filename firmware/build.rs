use std::process::Command;

fn main() {
    embuild::espidf::sysenv::output();

    // Bake the build id (evc04#101) so the running image can publish which commit
    // it is over MQTT, instead of inferring the build from the telemetry schema.
    // Re-run when HEAD or the reflog moves (commit, checkout) so it never stales.
    let version = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=FW_VERSION={version}");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/logs/HEAD");
}
