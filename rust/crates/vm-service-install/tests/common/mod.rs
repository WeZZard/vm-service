//! Shared fixture for the installer integration tests.
//!
//! The port of `tests/unit/test_console_install.py` runs the built
//! `vm-service-install` binary against an isolated `HOME`, a fake Tart, and a
//! `launchctl` stub, exactly as the Python test runs the shell script.

#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// A `launchctl` stand-in that records calls and keeps the documented sentinel
/// files. It mirrors the Python stub in `test_console_install.py`.
pub const LAUNCHCTL_STUB: &str = r#"#!/bin/sh
home="$HOME"
printf '%s\n' "$*" >> "$home/launchctl.log"
job="$home/loaded-job"
op="$1"
case "$op" in
  print)
    if [ -e "$home/print-fails" ]; then
      echo 'permission denied' >&2
      exit 1
    fi
    if [ -e "$home/user-domain" ]; then expected="user/"; else expected="gui/"; fi
    target="${2:-}"
    case "$target" in
      "$expected"*) ;;
      *) echo 'Could not find service "com.wezzard.vm-service" in domain' >&2; exit 113 ;;
    esac
    if [ ! -e "$job" ]; then
      echo 'Could not find service "com.wezzard.vm-service" in domain' >&2
      exit 113
    fi
    echo "path = $home/Library/LaunchAgents/com.wezzard.vm-service.plist"
    content="$(cat "$job")"
    if [ -n "$content" ]; then echo "pid = $content"; fi
    ;;
  unload)
    if [ -e "$home/unload-fails" ]; then exit 1; fi
    if [ ! -e "$home/linger-job" ]; then rm -f "$job"; fi
    ;;
  load)
    : > "$job"
    ;;
  *)
    exit 99
    ;;
esac
exit 0
"#;

pub fn write_exec(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

pub struct Harness {
    pub tmp: TempDir,
    pub home: PathBuf,
    pub bin: PathBuf,
    pub log: PathBuf,
    pub tartlog: PathBuf,
    pub tart: PathBuf,
    pub plist: PathBuf,
    pub config: PathBuf,
    pub viewer: PathBuf,
    pub installer: PathBuf,
    pub extra_env: Vec<(String, String)>,
}

impl Harness {
    pub fn new() -> Self {
        let parent = std::env::var("HOME").expect("HOME is set for the test runner");
        let tmp = tempfile::Builder::new()
            .prefix(".vm-service-install-test-")
            .tempdir_in(parent)
            .unwrap();
        let home = tmp.path().to_path_buf();
        let bin = home.join("bin");
        fs::create_dir_all(&bin).unwrap();

        let log = home.join("launchctl.log");
        let tartlog = home.join("tart.log");
        let tart = bin.join("tart");
        write_exec(
            &tart,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/tart.log\"\n[ \"$1\" = --version ] || exit 99\nprintf '2.32.1\\n'\n",
        );
        write_exec(&bin.join("launchctl"), LAUNCHCTL_STUB);

        let installer = bin.join("vm-service-install");
        fs::copy(env!("CARGO_BIN_EXE_vm-service-install"), &installer).unwrap();
        fs::set_permissions(&installer, fs::Permissions::from_mode(0o755)).unwrap();

        for name in [
            "vm-service",
            "console-worker",
            "guest-console-agent",
            "guest-console-agent-linux",
            "vmctl",
        ] {
            write_exec(&bin.join(name), "#!/bin/sh\nexit 0\n");
        }
        let viewer = bin.join("vncviewer");
        write_exec(&viewer, "#!/bin/sh\nexit 99\n");

        let config = home.join("console & <trusted>.json");
        let body = format!(
            "{{\"schemaVersion\":1,\"enabled\":true,\"linux_viewer\":\"{}\"}}",
            viewer.display()
        );
        fs::write(&config, body).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();

        let plist = home.join("Library/LaunchAgents/com.wezzard.vm-service.plist");

        Self {
            tmp,
            home,
            bin,
            log,
            tartlog,
            tart,
            plist,
            config,
            viewer,
            installer,
            extra_env: Vec::new(),
        }
    }

    pub fn set_env(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        self.extra_env.retain(|(existing, _)| existing != key);
        self.extra_env.push((key.to_string(), value));
    }

    pub fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(&self.installer);
        command.args(args);
        command.env_clear();
        command.env("HOME", &self.home);
        command.env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()));
        command.env("TART", &self.tart);
        command.env("PYTHONDONTWRITEBYTECODE", "1");
        for (key, value) in &self.extra_env {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    pub fn assert_refused(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            !output.status.success(),
            "expected a refusal for {args:?}, got success: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    pub fn saved(&self) -> serde_json::Value {
        let data = fs::read(&self.plist).unwrap();
        vm_service_install::plist::parse(&data).unwrap()
    }

    pub fn save_old(&self, environment: &[(&str, &str)]) {
        let mut env = serde_json::Map::new();
        for (key, value) in environment {
            env.insert(
                (*key).to_string(),
                serde_json::Value::String((*value).to_string()),
            );
        }
        let value = serde_json::json!({
            "EnvironmentVariables": serde_json::Value::Object(env),
            "ProgramArguments": ["/usr/bin/python3", "/old/path/vm-service"],
        });
        fs::create_dir_all(self.plist.parent().unwrap()).unwrap();
        fs::write(&self.plist, vm_service_install::plist::dumps(&value)).unwrap();
        fs::write(self.home.join("loaded-job"), "").unwrap();
    }

    pub fn config_path(&self) -> String {
        self.config.to_string_lossy().into_owned()
    }

    pub fn home_str(&self) -> &str {
        self.home.to_str().unwrap()
    }
}
