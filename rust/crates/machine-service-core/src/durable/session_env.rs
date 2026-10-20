//! The session environment of a durable lease (`docs/arch/durable-machine.md`,
//! DM-4; `docs/arch/agent-keys.md`, AK-3 to AK-5).
//!
//! It is built at acquisition from the account's login shell, stripped of
//! every credential the owner's own shell may carry, and extended with the
//! lease's variables and the loaded agents' variables. Only the variable
//! names are ever recorded.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use super::agents::{AgentKeys, Secret};
use super::process::LEASE_VARIABLE;

/// The owner's credential variables, removed from every session (DM-4,
/// AK-3).
pub const CREDENTIAL_VARIABLES: [&str; 8] = [
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "LITELLM_CODEX_KEY",
    "CODEX_HOME",
];
/// The variable naming the lease's workspace (DM-4).
pub const WORKSPACE_VARIABLE: &str = "MACHINE_SERVICE_WORKSPACE";
/// Separates the login shell's own chatter from the environment dump.
const MARKER: &str = "MACHINE_SERVICE_LOGIN_ENVIRONMENT";

/// A built session environment and the secrets to redact from its output.
#[derive(Debug, Clone, Default)]
pub struct SessionEnv {
    /// The variables every command of the lease receives.
    pub vars: BTreeMap<String, String>,
    /// The loaded agent keys' values, for redaction only (AK-5).
    pub secrets: Vec<Secret>,
}

impl SessionEnv {
    /// The variable names, which is all a record keeps (DM-4).
    pub fn names(&self) -> Vec<String> {
        self.vars.keys().cloned().collect()
    }

    /// The shell that runs an `argv` command: the session's `SHELL`, or zsh.
    pub fn shell(&self) -> String {
        self.vars
            .get("SHELL")
            .filter(|shell| shell.starts_with('/'))
            .cloned()
            .unwrap_or_else(|| "/bin/zsh".to_string())
    }
}

/// Whether a variable is the service's own configuration or a credential,
/// neither of which belongs in a session.
fn excluded(name: &str) -> bool {
    CREDENTIAL_VARIABLES.contains(&name)
        || name.starts_with(environment::renamed::PREFIX)
        || name.starts_with(environment::renamed::LEGACY_PREFIX)
}

/// Run the account's login shell and return the environment it builds
/// (DM-4 step 1). The shell starts from `base` minus credentials and the
/// service's own variables, in `home`.
pub fn capture_login_environment(
    shell: &Path,
    base: &[(String, String)],
    home: &Path,
    timeout: Duration,
) -> Result<Vec<(String, String)>, String> {
    let mut command = Command::new(shell);
    command
        .args(["-l", "-c", &format!("printf '\\0{MARKER}\\0'; env -0")])
        .env_clear()
        .envs(base.iter().filter(|(name, _)| !excluded(name)).cloned())
        .current_dir(home);
    let output = crate::proc::run_capture(&mut command, None, timeout)
        .map_err(|error| format!("login shell {}: {error}", shell.display()))?;
    if !output.status.success() {
        return Err(format!(
            "login shell {} exited with {}",
            shell.display(),
            output.status.code().unwrap_or(-1)
        ));
    }
    parse_env_dump(&output.stdout)
        .ok_or_else(|| format!("login shell {} printed no environment", shell.display()))
}

/// Parse the `\0MARKER\0` + `env -0` output.
pub fn parse_env_dump(bytes: &[u8]) -> Option<Vec<(String, String)>> {
    let marker = format!("\0{MARKER}\0");
    let start = bytes
        .windows(marker.len())
        .rposition(|window| window == marker.as_bytes())?
        + marker.len();
    let mut out = Vec::new();
    for entry in bytes[start..].split(|byte| *byte == 0) {
        if entry.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(entry);
        if let Some((name, value)) = text.split_once('=') {
            if !name.is_empty() {
                out.push((name.to_string(), value.to_string()));
            }
        }
    }
    Some(out)
}

/// The lease's workspace folders (DM-1).
#[derive(Debug, Clone)]
pub struct WorkspacePaths<'a> {
    /// The workspace root.
    pub root: &'a Path,
    /// `work/`, the default working directory.
    pub work: &'a Path,
    /// `tmp/`, the session's `TMPDIR`.
    pub tmp: &'a Path,
    /// `codex/`, the session's `CODEX_HOME`.
    pub codex: &'a Path,
}

/// Build the session environment (DM-4 steps 2 and 3).
pub fn build(
    login: Vec<(String, String)>,
    lease_id: &str,
    workspace: &WorkspacePaths<'_>,
    home: &Path,
    agents: Option<&AgentKeys>,
) -> SessionEnv {
    let mut vars: BTreeMap<String, String> = login
        .into_iter()
        .filter(|(name, _)| !excluded(name))
        .collect();
    // HOME stays the account's home (DM-1).
    vars.insert("HOME".to_string(), home.to_string_lossy().to_string());
    vars.insert(LEASE_VARIABLE.to_string(), lease_id.to_string());
    vars.insert(
        WORKSPACE_VARIABLE.to_string(),
        workspace.root.to_string_lossy().to_string(),
    );
    vars.insert(
        "TMPDIR".to_string(),
        workspace.tmp.to_string_lossy().to_string(),
    );
    vars.insert(
        "CODEX_HOME".to_string(),
        workspace.codex.to_string_lossy().to_string(),
    );
    let mut secrets = Vec::new();
    if let Some(agents) = agents {
        for (name, value) in agents.environment() {
            vars.insert(name, value);
        }
        secrets = agents.secrets();
    }
    SessionEnv { vars, secrets }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-23 (TL-1): the owner's credentials and the service's own settings
    /// never reach a session; the lease's variables do.
    #[test]
    fn ac23_credentials_are_removed_and_lease_variables_added() {
        let login = vec![
            ("PATH".to_string(), "/opt/homebrew/bin:/usr/bin".to_string()),
            (
                "ANTHROPIC_AUTH_TOKEN".to_string(),
                "owner-token".to_string(),
            ),
            ("OPENAI_API_KEY".to_string(), "owner-openai".to_string()),
            (
                "CLAUDE_CODE_OAUTH_TOKEN".to_string(),
                "owner-oauth".to_string(),
            ),
            ("CODEX_HOME".to_string(), "/Users/u/.codex".to_string()),
            ("MACHINE_SERVICE_PORT".to_string(), "6240".to_string()),
            ("HOME".to_string(), "/elsewhere".to_string()),
        ];
        let root = Path::new("/ws/L");
        let paths = WorkspacePaths {
            root,
            work: &root.join("work"),
            tmp: &root.join("tmp"),
            codex: &root.join("codex"),
        };
        let env = build(login, "L", &paths, Path::new("/Users/u"), None);
        for name in CREDENTIAL_VARIABLES
            .iter()
            .filter(|name| **name != "CODEX_HOME")
        {
            assert!(!env.vars.contains_key(*name), "{name}");
        }
        assert!(!env.vars.contains_key("MACHINE_SERVICE_PORT"));
        assert_eq!(env.vars["PATH"], "/opt/homebrew/bin:/usr/bin");
        assert_eq!(env.vars["HOME"], "/Users/u");
        assert_eq!(env.vars["MACHINE_SERVICE_LEASE_ID"], "L");
        assert_eq!(env.vars["MACHINE_SERVICE_WORKSPACE"], "/ws/L");
        assert_eq!(env.vars["TMPDIR"], "/ws/L/tmp");
        assert_eq!(env.vars["CODEX_HOME"], "/ws/L/codex");
        assert!(env.secrets.is_empty());
    }

    #[test]
    fn env_dump_skips_shell_chatter() {
        let mut bytes = b"welcome\n\0".to_vec();
        bytes.extend_from_slice(format!("\0{MARKER}\0A=1\0B=x=y\0").as_bytes());
        let parsed = parse_env_dump(&bytes).unwrap();
        assert_eq!(
            parsed,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "x=y".to_string())
            ]
        );
        assert!(parse_env_dump(b"A=1\0").is_none());
    }

    /// DM-4 (TL-2): a real login shell in a fixture home builds the
    /// environment its profile sets, and credentials from the base are
    /// stripped before the shell starts.
    #[test]
    fn dm4_real_login_shell_capture() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".zprofile"),
            "export FIXTURE_PROFILE=loaded\nexport ANTHROPIC_AUTH_TOKEN=from-profile\n",
        )
        .unwrap();
        let base = vec![
            (
                "HOME".to_string(),
                home.path().to_string_lossy().to_string(),
            ),
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("OPENAI_API_KEY".to_string(), "from-base".to_string()),
        ];
        let captured = capture_login_environment(
            Path::new("/bin/zsh"),
            &base,
            home.path(),
            Duration::from_secs(30),
        )
        .unwrap();
        let map: BTreeMap<_, _> = captured.into_iter().collect();
        assert_eq!(map["FIXTURE_PROFILE"], "loaded");
        assert!(!map.contains_key("OPENAI_API_KEY"));
        // The profile's own credential is captured here and removed by
        // `build`.
        assert_eq!(map["ANTHROPIC_AUTH_TOKEN"], "from-profile");
    }
}
