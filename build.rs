use std::process::Command;

// CI passes the release's image tag as DOCKET_VERSION; anything else is a
// local build, named for the day and the jj change it came from.
fn main() {
    println!("cargo:rerun-if-env-changed=DOCKET_VERSION");
    let version = std::env::var("DOCKET_VERSION")
        .ok()
        .filter(|v| !v.is_empty());
    let version = version.unwrap_or_else(|| {
        // Every jj operation replaces the op head, so a new change renames
        // the build.
        println!("cargo:rerun-if-changed=.jj/repo/op_heads/heads");
        let date = cmd("date", &["-u", "+%Y-%m-%d"]);
        let change = cmd(
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", "change_id.short()"],
        );
        format!("{date}+{change}-dev")
    });
    println!("cargo:rustc-env=DOCKET_VERSION={version}");
}

fn cmd(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}
