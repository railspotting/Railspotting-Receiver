//! The Linux deployment files in packaging/linux and the command line they rely on.

use std::path::PathBuf;
use std::process::Command;

fn deploy() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packaging/linux")
}

#[test]
fn install_script_syntax() {
    let st = Command::new("bash").arg("-n").arg(deploy().join("install.sh")).status().unwrap();
    assert!(st.success());
}

#[test]
fn the_unit_command_line_parses() {
    use clap::Parser;
    let unit = std::fs::read_to_string(deploy().join("railspotting-receiver.service")).unwrap();
    let exec = unit.lines().find_map(|l| l.strip_prefix("ExecStart=")).unwrap();
    let a = railspotting_engine::cli::Args::try_parse_from(exec.split_whitespace()).unwrap();
    assert!(matches!(a.cmd, Some(railspotting_engine::cli::Cmd::Run { simulate: false })));
    let a = railspotting_engine::cli::Args::try_parse_from(["railspotting-receiver", "run", "--simulate"]).unwrap();
    assert!(matches!(a.cmd, Some(railspotting_engine::cli::Cmd::Run { simulate: true })));
    let a = railspotting_engine::cli::Args::try_parse_from(
        ["railspotting-receiver", "detector", "add", "ns-475.8", "--text", "x", "--split", "5.2", "--opening-end", "2.6", "a", "b", "c"]).unwrap();
    assert!(matches!(a.cmd, Some(railspotting_engine::cli::Cmd::Detector { .. })));
    assert!(railspotting_engine::cli::Args::try_parse_from(["railspotting-receiver", "detector", "add", "x", "a"]).is_err());
}
