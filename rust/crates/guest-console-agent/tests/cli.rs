//! CLI framing tests: argument rejection and the probe record emitted for the
//! wrong platform (which avoids touching any real guest service).

use std::process::Command;

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_guest-console-agent"))
}

#[test]
fn invalid_arguments_exit_two_with_a_fixed_message() {
    for args in [
        vec![],
        vec!["bogus"],
        vec!["bogus", "probe"],
        vec!["linux", "bogus"],
        vec!["linux", "probe", "extra"],
    ] {
        let output = binary().args(&args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "args {args:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "guest-console: arguments_invalid\n",
            "args {args:?}"
        );
        assert!(output.stdout.is_empty(), "args {args:?}");
    }
}

#[test]
fn probe_for_the_other_platform_reports_a_mismatch() {
    let other = if cfg!(target_os = "macos") {
        "linux"
    } else {
        "macos"
    };
    let output = binary().args([other, "probe"]).output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());

    let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(record["version"], serde_json::json!(1));
    assert_eq!(record["kind"], serde_json::json!(other));
    assert_eq!(record["ready"], serde_json::json!(false));
    assert_eq!(
        record["error"],
        serde_json::json!("guest_platform_mismatch")
    );
    assert_eq!(record["session"], serde_json::Value::Null);
}
