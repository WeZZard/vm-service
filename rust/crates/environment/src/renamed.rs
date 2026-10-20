//! The service's variables after the rename to machine-service (MG-1).
//!
//! Every `VM_SERVICE_*` variable became `MACHINE_SERVICE_*`. No reader accepts
//! the old names: the daemon refuses to start while one is set, and the
//! installer rewrites the ones an installed vm-service agent carries, so a
//! missed caller fails at once with its fix in the error instead of being
//! silently ignored (DC-37, MG-3).

/// The prefix the service's variables carried before the rename.
pub const LEGACY_PREFIX: &str = "VM_SERVICE_";

/// The prefix of the service's variables.
pub const PREFIX: &str = "MACHINE_SERVICE_";

/// The new name of a `VM_SERVICE_*` variable, or `None` for any other name.
pub fn replacement(name: &str) -> Option<String> {
    name.strip_prefix(LEGACY_PREFIX)
        .map(|rest| format!("{PREFIX}{rest}"))
}

/// Refuse when any of `names` is a `VM_SERVICE_*` variable, naming each one
/// and its replacement.
pub fn refuse_legacy_variables<I, S>(names: I) -> Result<(), String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut found: Vec<String> = names
        .into_iter()
        .filter_map(|name| {
            let name = name.as_ref();
            replacement(name).map(|new| format!("{name} is now {new}"))
        })
        .collect();
    if found.is_empty() {
        return Ok(());
    }
    found.sort();
    Err(format!(
        "VM_SERVICE_* variables are no longer read; rename them: {}",
        found.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renames_only_the_service_prefix() {
        assert_eq!(
            replacement("VM_SERVICE_PORT").as_deref(),
            Some("MACHINE_SERVICE_PORT")
        );
        assert_eq!(replacement("VM_ENVIRONMENT_FILE"), None);
        assert_eq!(replacement("MACHINE_SERVICE_PORT"), None);
    }

    /// AC-36: an old variable name is refused with its replacement named.
    #[test]
    fn ac36_refusal_names_every_old_variable_and_its_replacement() {
        assert_eq!(refuse_legacy_variables(["PATH", "VM_RELAY_URL"]), Ok(()));
        let error =
            refuse_legacy_variables(["VM_SERVICE_PORT", "PATH", "VM_SERVICE_HOST"]).unwrap_err();
        assert_eq!(
            error,
            "VM_SERVICE_* variables are no longer read; rename them: \
             VM_SERVICE_HOST is now MACHINE_SERVICE_HOST, \
             VM_SERVICE_PORT is now MACHINE_SERVICE_PORT"
        );
    }
}
