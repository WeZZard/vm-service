//! What `machctl acquire` asks for (`docs/ux/cli/list.md` CL-2, DC-69).
//!
//! Three forms, one at a time: `--image` leases a virtual machine, as
//! `vmctl acquire` did, from the local instance or the one `--instance`
//! names; `--machine` leases a named durable machine from the instance that
//! owns it; `--require` lists every instance, keeps the machines and offers
//! with every named capability, and leases the best one. Without any of the
//! three the image is `macos26`, as before.

/// The image `--image` defaults to when no other form is named.
pub const DEFAULT_IMAGE: &str = "macos26";

/// What one acquisition asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A virtual machine of `image`, from the local instance or `instance`.
    Image {
        image: String,
        instance: Option<String>,
    },
    /// The durable machine `name`, possibly written `<name>@<instance>`.
    Machine(String),
    /// Any machine with every capability, from the instances `instance`
    /// narrows to, or all of them.
    Require {
        names: Vec<String>,
        instance: Option<String>,
    },
}

/// The flags of `machctl acquire` that apply only to a virtual machine,
/// each with whether it was given a value other than its default.
pub struct VirtualFlags<'a> {
    pub network: &'a str,
    pub profile: bool,
    pub cpu: bool,
    pub memory_mb: bool,
    pub disk_gb: bool,
    pub no_wait: bool,
    pub vnc: bool,
    pub source: &'a str,
    pub expected_source_fingerprint: bool,
}

impl VirtualFlags<'_> {
    /// The first virtual-machine-only flag that was given, by its spelling.
    fn first_given(&self) -> Option<&'static str> {
        [
            (self.network != "nat", "--network"),
            (self.profile, "--profile"),
            (self.cpu, "--cpu"),
            (self.memory_mb, "--memory-mb"),
            (self.disk_gb, "--disk-gb"),
            (self.no_wait, "--no-wait"),
            (self.vnc, "--vnc"),
            (self.source != "base", "--source"),
            (
                self.expected_source_fingerprint,
                "--expected-source-fingerprint",
            ),
        ]
        .into_iter()
        .find(|(given, _)| *given)
        .map(|(_, flag)| flag)
    }
}

/// Decide the form from the flags, or the usage error that refuses them.
pub fn target(
    image: Option<String>,
    machine: Option<String>,
    require: Vec<String>,
    instance: Option<String>,
    flags: &VirtualFlags<'_>,
) -> Result<Target, String> {
    if !require.is_empty() {
        crate::capability::validate(&require)?;
        if image.is_some() || machine.is_some() {
            return Err(
                "--require chooses the machine; it cannot be combined with --image or --machine"
                    .to_string(),
            );
        }
        if let Some(flag) = flags.first_given() {
            return Err(format!(
                "{flag} applies only to --image; --require may choose a durable machine"
            ));
        }
        return Ok(Target::Require {
            names: require,
            instance,
        });
    }
    if let Some(machine) = machine {
        if image.is_some() {
            return Err("--machine and --image cannot be combined".to_string());
        }
        if instance.is_some() {
            return Err(
                "--instance chooses the instance for an image; name a durable machine by itself"
                    .to_string(),
            );
        }
        if let Some(flag) = flags.first_given() {
            return Err(format!(
                "{flag} applies only to a virtual machine; --machine leases a durable machine"
            ));
        }
        return Ok(Target::Machine(machine));
    }
    Ok(Target::Image {
        image: image.unwrap_or_else(|| DEFAULT_IMAGE.to_string()),
        instance,
    })
}

/// Whether a refused acquisition was refused for capacity, so the next
/// candidate may be tried (CL-2): a durable machine already leased or
/// without its GUI session (MM-5), or an instance at its macOS limit.
pub fn capacity_refusal(status: u16, error: &str) -> bool {
    status == 409
        && (error.starts_with("durable machine leased")
            || error.starts_with("durable machine unavailable")
            || error.contains("VM limit reached"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> VirtualFlags<'static> {
        VirtualFlags {
            network: "nat",
            profile: false,
            cpu: false,
            memory_mb: false,
            disk_gb: false,
            no_wait: false,
            vnc: false,
            source: "base",
            expected_source_fingerprint: false,
        }
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    /// CL-2: without a form the image is macos26, as `vmctl acquire` had it.
    #[test]
    fn cl2_default_is_the_macos26_image() {
        assert_eq!(
            target(None, None, vec![], None, &plain()),
            Ok(Target::Image {
                image: "macos26".to_string(),
                instance: None
            })
        );
        assert_eq!(
            target(
                Some("ubuntu2404".to_string()),
                None,
                vec![],
                Some("mac-a".to_string()),
                &plain()
            ),
            Ok(Target::Image {
                image: "ubuntu2404".to_string(),
                instance: Some("mac-a".to_string())
            })
        );
    }

    /// CL-2: the three forms exclude each other, and a durable form refuses
    /// the flags that only shape a virtual machine.
    #[test]
    fn cl2_forms_exclude_each_other() {
        let error = target(
            Some("macos26".to_string()),
            None,
            strings(&["metal3"]),
            None,
            &plain(),
        )
        .unwrap_err();
        assert!(error.contains("--require"), "{error}");
        assert!(target(
            Some("macos26".to_string()),
            Some("mac-b".to_string()),
            vec![],
            None,
            &plain()
        )
        .is_err());
        assert!(target(
            None,
            Some("mac-b".to_string()),
            vec![],
            Some("mac-a".to_string()),
            &plain()
        )
        .is_err());
        let mut flags = plain();
        flags.cpu = true;
        let error = target(None, Some("mac-b".to_string()), vec![], None, &flags).unwrap_err();
        assert!(error.starts_with("--cpu applies only"), "{error}");
        let mut flags = plain();
        flags.network = "internet";
        let error = target(None, None, strings(&["metal3"]), None, &flags).unwrap_err();
        assert!(error.starts_with("--network applies only"), "{error}");
        assert_eq!(
            target(None, Some("mac-b".to_string()), vec![], None, &plain()),
            Ok(Target::Machine("mac-b".to_string()))
        );
        let error = target(None, None, strings(&["metal"]), None, &plain()).unwrap_err();
        assert!(error.contains("unknown capability"), "{error}");
    }

    /// CL-2: only a capacity refusal moves on to the next candidate.
    #[test]
    fn cl2_capacity_refusals() {
        assert!(capacity_refusal(
            409,
            "durable machine leased: mac-b (purpose x, until 2026-10-21 10:00:00)"
        ));
        assert!(capacity_refusal(
            409,
            "durable machine unavailable: mac-b (no logged-in GUI session for wezzard (the console belongs to root))"
        ));
        assert!(capacity_refusal(
            409,
            "macOS VM limit reached (2 active); release one first"
        ));
        assert!(!capacity_refusal(400, "durable machine leased: mac-b"));
        assert!(!capacity_refusal(409, "durable acquisition failed: disk"));
    }
}
