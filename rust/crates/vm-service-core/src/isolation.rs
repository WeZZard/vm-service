//! What an `internet` guest needs beyond Softnet (DC-14,
//! `docs/acquisition-contract.md`, "Guest network"):
//!
//! - the guest firewall, a pf anchor that drops the guest's connections to
//!   this host (`guest_firewall`), installed once by root;
//! - public DNS resolvers in the guest, because the gateway's resolver is
//!   this host and the firewall drops it;
//! - an isolation check before the lease is ready: the guest tries to reach
//!   a listener on this host through its default gateway, and must fail.
//!
//! The functions here build the guest commands and decide from their output;
//! `ops.rs` runs them.

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

use guest_firewall::Subnet;

use crate::ssh::shell_quote;

/// The resolvers an `internet` guest gets when `VM_SERVICE_GUEST_DNS` is unset.
pub const DEFAULT_GUEST_DNS: &str = "1.1.1.1,8.8.8.8";

/// The environment variable that overrides [`DEFAULT_GUEST_DNS`].
pub const GUEST_DNS_VARIABLE: &str = "VM_SERVICE_GUEST_DNS";

/// The environment variable that overrides the anchor file the service reads;
/// for test fixtures and hosts that keep it elsewhere.
pub const ANCHOR_FILE_VARIABLE: &str = "VM_SERVICE_GUEST_FIREWALL_ANCHOR";

/// A marker in the probe command, so a fake guest can recognise it.
pub const PROBE_MARKER: &str = "vm-service isolation probe";

/// How long the guest waits for the gateway to answer, in seconds.
pub const PROBE_TIMEOUT_S: u64 = 3;

/// Parse the guest resolvers: `value` is `VM_SERVICE_GUEST_DNS`, a
/// comma-separated list of IPv4 addresses that are reachable through Softnet.
///
/// Softnet drops a guest's IPv6 and every non-global IPv4 destination, so a
/// private, shared, benchmark (`198.18.0.0/15`), loopback, or IPv6 resolver
/// would never answer.
pub fn parse_guest_dns(value: Option<&str>) -> Result<Vec<Ipv4Addr>, String> {
    let text = match value.map(str::trim) {
        None | Some("") => DEFAULT_GUEST_DNS,
        Some(text) => text,
    };
    let mut servers = Vec::new();
    for item in text.split(',').map(str::trim) {
        let address: IpAddr = item.parse().map_err(|_| {
            format!(
                "{GUEST_DNS_VARIABLE} must list IPv4 addresses separated by commas; got {item:?}"
            )
        })?;
        let IpAddr::V4(address) = address else {
            return Err(format!(
                "{GUEST_DNS_VARIABLE}: {address} is IPv6, which Softnet drops; use IPv4 resolvers"
            ));
        };
        if !globally_routable(address) {
            return Err(format!(
                "{GUEST_DNS_VARIABLE}: {address} is not a public address, so Softnet drops it"
            ));
        }
        if !servers.contains(&address) {
            servers.push(address);
        }
    }
    Ok(servers)
}

fn globally_routable(address: Ipv4Addr) -> bool {
    let [a, b, _, _] = address.octets();
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_unspecified()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_multicast()
        || a == 0
        || a >= 240
        || (a == 100 && (64..128).contains(&b))
        || (a == 198 && (18..20).contains(&b))
        || (a == 192 && b == 0))
}

/// The anchor file the service reads: `VM_SERVICE_GUEST_FIREWALL_ANCHOR`, or
/// the installed default.
pub fn anchor_file(value: Option<&str>) -> PathBuf {
    match value {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => PathBuf::from(guest_firewall::ANCHOR_FILE),
    }
}

/// Read the installed guest firewall's subnet from `path`, or the reason,
/// with the install command, that an `internet` lease cannot be offered.
///
/// Whether pf has the anchor loaded needs root to see; the isolation check
/// catches an anchor that is installed but not in force.
pub fn guest_firewall_subnet(path: &Path) -> Result<Subnet, String> {
    let installer = installer_path();
    let default_subnet: Subnet = guest_firewall::DEFAULT_SUBNET
        .parse()
        .expect("the default subnet is valid");
    let text = std::fs::read_to_string(path).map_err(|_| {
        format!(
            "guest firewall not installed: {} is missing; network internet needs the pf anchor {} \
(DC-14); install it once with: {} (no NAT fallback)",
            path.display(),
            guest_firewall::ANCHOR,
            guest_firewall::install_command(&installer, &default_subnet)
        )
    })?;
    guest_firewall::parse_anchor_subnet(&text).ok_or_else(|| {
        format!(
            "guest firewall unreadable: {} names no subnet; reinstall it with: {} (no NAT fallback)",
            path.display(),
            guest_firewall::install_command(&installer, &default_subnet)
        )
    })
}

/// The installer executable, staged beside the running daemon.
pub fn installer_path() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.parent()
                .map(|directory| directory.join(guest_firewall::INSTALLER))
        })
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| guest_firewall::INSTALLER.to_string())
}

/// The root script that points a guest of `kind` at `servers`.
///
/// - Linux with systemd-resolved: a `resolved.conf.d` drop-in makes the
///   servers global with the routing domain `~.`, and the default interface
///   gets them too and stops being the default route for lookups. A DHCP
///   renewal can restore the gateway on the interface; resolved then still
///   answers from the global servers, because the firewall drops the gateway.
/// - Linux without systemd-resolved: `/etc/resolv.conf` lists the servers.
/// - macOS: `networksetup -setdnsservers` on the service of the default
///   route's interface.
pub fn resolver_script(kind: &str, servers: &[Ipv4Addr]) -> String {
    let list = servers
        .iter()
        .map(Ipv4Addr::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    if kind == "macos" {
        return format!(
            "set -e\n\
dev=$(route -n get default 2>/dev/null | awk '/interface:/ {{print $2; exit}}')\n\
svc=$(networksetup -listnetworkserviceorder | awk -v dev=\"$dev\" '/^\\([0-9*]+\\) / {{name=$0; sub(/^\\([0-9*]+\\) /, \"\", name)}} index($0, \"Device: \" dev \")\") {{print name; exit}}')\n\
[ -n \"$svc\" ] || {{ echo \"no network service for interface ${{dev:-?}}\" >&2; exit 1; }}\n\
networksetup -setdnsservers \"$svc\" {list}\n\
dscacheutil -flushcache\n\
killall -HUP mDNSResponder 2>/dev/null || true\n\
networksetup -getdnsservers \"$svc\"\n"
        );
    }
    let resolv_conf = servers
        .iter()
        .map(|server| format!("nameserver {server}\\n"))
        .collect::<String>();
    format!(
        "set -e\n\
dev=$(ip route show default | awk '{{for (i = 1; i < NF; i++) if ($i == \"dev\") {{print $(i + 1); exit}}}}')\n\
if command -v resolvectl >/dev/null 2>&1 && systemctl is-active --quiet systemd-resolved; then\n\
  mkdir -p /etc/systemd/resolved.conf.d\n\
  printf '[Resolve]\\nDNS={list}\\nDomains=~.\\n' > /etc/systemd/resolved.conf.d/90-vm-service-guest-dns.conf\n\
  systemctl restart systemd-resolved\n\
  if [ -n \"$dev\" ]; then\n\
    resolvectl dns \"$dev\" {list}\n\
    resolvectl default-route \"$dev\" false 2>/dev/null || true\n\
  fi\n\
  resolvectl flush-caches 2>/dev/null || true\n\
  resolvectl dns\n\
else\n\
  rm -f /etc/resolv.conf\n\
  printf '{resolv_conf}' > /etc/resolv.conf\n\
  cat /etc/resolv.conf\n\
fi\n"
    )
}

/// Wrap a root `script` for the guest account: `sudo -S` reads `password`
/// from standard input, and `sudo -n` runs without one.
///
/// Returns the remote command and its standard input.
pub fn as_root(script: &str, password: &str) -> (String, Option<Vec<u8>>) {
    if password.is_empty() {
        (format!("sudo -n sh -c {}", shell_quote(script)), None)
    } else {
        (
            format!("sudo -S -p '' sh -c {}", shell_quote(script)),
            Some(format!("{password}\n").into_bytes()),
        )
    }
}

/// The guest command that tries a TCP connection to its default gateway on
/// `port` and prints `gateway=<address> result=<open|refused|timeout|error:...>`.
pub fn probe_command(kind: &str, port: u16) -> String {
    let timeout = PROBE_TIMEOUT_S;
    if kind == "macos" {
        return format!(
            "# {PROBE_MARKER}\n\
gw=$(route -n get default 2>/dev/null | awk '/gateway:/ {{print $2; exit}}')\n\
if [ -z \"$gw\" ]; then echo 'gateway= result=error:no-default-route'; exit 0; fi\n\
out=$(nc -v -z -G {timeout} -w {timeout} \"$gw\" {port} 2>&1); rc=$?\n\
if [ $rc -eq 0 ]; then r=open\n\
elif printf '%s' \"$out\" | grep -qi refused; then r=refused\n\
elif printf '%s' \"$out\" | grep -qi 'timed out'; then r=timeout\n\
else r=error:$rc; fi\n\
echo \"gateway=$gw result=$r\"\n"
        );
    }
    format!(
        "# {PROBE_MARKER}\n\
gw=$(ip route show default | awk '{{print $3; exit}}')\n\
if [ -z \"$gw\" ]; then echo 'gateway= result=error:no-default-route'; exit 0; fi\n\
out=$(timeout {timeout} bash -c \"exec 3<>/dev/tcp/$gw/{port}\" 2>&1); rc=$?\n\
if [ $rc -eq 0 ]; then r=open\n\
elif [ $rc -eq 124 ]; then r=timeout\n\
elif printf '%s' \"$out\" | grep -qi refused; then r=refused\n\
else r=error:$rc; fi\n\
echo \"gateway=$gw result=$r\"\n"
    )
}

/// What the guest saw when it tried the gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    /// The connection completed.
    Open,
    /// The host answered with a reset.
    Refused,
    /// Nothing answered within the probe timeout.
    Timeout,
    /// The probe could not run; the text says why.
    Error(String),
}

/// The probe's output: the guest's default gateway and the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOutcome {
    /// The guest's default gateway, when it has one.
    pub gateway: Option<Ipv4Addr>,
    /// What the connection attempt did.
    pub result: ProbeResult,
}

/// Parse the probe's output. Anything unexpected is an error result.
pub fn parse_probe(output: &str) -> ProbeOutcome {
    let line = output
        .lines()
        .rev()
        .find(|line| line.starts_with("gateway="))
        .unwrap_or_default();
    let mut gateway = None;
    let mut result = ProbeResult::Error(format!("unexpected probe output: {:?}", output.trim()));
    for field in line.split_whitespace() {
        if let Some(value) = field.strip_prefix("gateway=") {
            gateway = value.parse().ok();
        } else if let Some(value) = field.strip_prefix("result=") {
            result = match value {
                "open" => ProbeResult::Open,
                "refused" => ProbeResult::Refused,
                "timeout" => ProbeResult::Timeout,
                other => ProbeResult::Error(other.to_string()),
            };
        }
    }
    ProbeOutcome { gateway, result }
}

/// Decide whether the guest is isolated from this host.
///
/// Only a timeout, with no connection seen by the host's listener, passes.
/// A completed connection or a reset means the host answered; an error means
/// isolation was not shown. Every failure names the anchor and the command
/// that installs it for the subnet the guest is on.
pub fn decide(
    outcome: &ProbeOutcome,
    accepted: bool,
    port: u16,
    anchor_subnet: &Subnet,
) -> Result<(), String> {
    if !accepted && outcome.result == ProbeResult::Timeout && outcome.gateway.is_some() {
        return Ok(());
    }
    let installer = installer_path();
    let subnet = outcome
        .gateway
        .map(Subnet::around)
        .unwrap_or(*anchor_subnet);
    let install = guest_firewall::install_command(&installer, &subnet);
    let gateway = outcome
        .gateway
        .map(|gateway| gateway.to_string())
        .unwrap_or_else(|| "its default gateway".to_string());
    let what = if accepted {
        "connected to".to_string()
    } else {
        match &outcome.result {
            ProbeResult::Open => "connected to".to_string(),
            ProbeResult::Refused => "was answered by".to_string(),
            ProbeResult::Timeout => "could not find".to_string(),
            ProbeResult::Error(reason) => format!("could not probe ({reason})"),
        }
    };
    let coverage = match outcome.gateway {
        Some(gateway) if !anchor_subnet.contains(gateway) => {
            format!("the guest firewall covers {anchor_subnet}, not the guest's subnet {subnet}")
        }
        _ => format!(
            "the pf anchor {} for {anchor_subnet} is not in force",
            guest_firewall::ANCHOR
        ),
    };
    Err(format!(
        "guest isolation check failed: the guest {what} this host at {gateway}:{port}; {coverage}; \
install it with: {install} (no NAT fallback)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subnet() -> Subnet {
        guest_firewall::DEFAULT_SUBNET.parse().unwrap()
    }

    fn gateway() -> Option<Ipv4Addr> {
        Some("192.168.2.1".parse().unwrap())
    }

    #[test]
    fn guest_dns_defaults_to_two_public_resolvers() {
        let expected: Vec<Ipv4Addr> = vec!["1.1.1.1".parse().unwrap(), "8.8.8.8".parse().unwrap()];
        assert_eq!(parse_guest_dns(None).unwrap(), expected);
        assert_eq!(parse_guest_dns(Some("")).unwrap(), expected);
        assert_eq!(
            parse_guest_dns(Some(" 9.9.9.9 , 9.9.9.9,149.112.112.112")).unwrap(),
            vec![
                "9.9.9.9".parse::<Ipv4Addr>().unwrap(),
                "149.112.112.112".parse().unwrap()
            ]
        );
    }

    #[test]
    fn guest_dns_refuses_what_softnet_drops() {
        for value in [
            "192.168.2.1",
            "10.0.0.53",
            "172.16.0.1",
            "127.0.0.53",
            "198.18.0.2",
            "100.100.100.100",
            "169.254.0.1",
            "0.0.0.0",
            "2606:4700:4700::1111",
            "1.1.1.1,dns.google",
            "1.1.1.1,",
        ] {
            let error = parse_guest_dns(Some(value)).unwrap_err();
            assert!(error.starts_with(GUEST_DNS_VARIABLE), "{value}: {error}");
        }
    }

    #[test]
    fn the_linux_resolver_uses_systemd_resolved_or_resolv_conf() {
        let servers = parse_guest_dns(None).unwrap();
        let script = resolver_script("linux", &servers);
        assert!(script.contains(
            "printf '[Resolve]\\nDNS=1.1.1.1 8.8.8.8\\nDomains=~.\\n' > /etc/systemd/resolved.conf.d/90-vm-service-guest-dns.conf"
        ));
        assert!(script.contains("systemctl restart systemd-resolved"));
        assert!(script.contains("resolvectl dns \"$dev\" 1.1.1.1 8.8.8.8"));
        assert!(script.contains("resolvectl default-route \"$dev\" false"));
        assert!(script
            .contains("printf 'nameserver 1.1.1.1\\nnameserver 8.8.8.8\\n' > /etc/resolv.conf"));
        assert!(!script.contains("networksetup"));
    }

    #[test]
    fn the_macos_resolver_uses_networksetup_on_the_default_service() {
        let servers = parse_guest_dns(Some("9.9.9.9")).unwrap();
        let script = resolver_script("macos", &servers);
        assert!(script.contains("route -n get default"));
        assert!(script.contains("networksetup -setdnsservers \"$svc\" 9.9.9.9\n"));
        assert!(script.contains("killall -HUP mDNSResponder"));
        assert!(!script.contains("resolvectl"));
    }

    /// The generated scripts are valid `sh`.
    #[test]
    fn the_scripts_parse_as_shell() {
        let servers = parse_guest_dns(None).unwrap();
        for script in [
            resolver_script("linux", &servers),
            resolver_script("macos", &servers),
            probe_command("linux", 40000),
            probe_command("macos", 40000),
        ] {
            let status = std::process::Command::new("/bin/sh")
                .args(["-n", "-c", &script])
                .status()
                .unwrap();
            assert!(status.success(), "sh -n rejected:\n{script}");
        }
    }

    #[test]
    fn root_scripts_use_sudo_with_or_without_a_password() {
        let (command, stdin) = as_root("echo hi", "admin");
        assert_eq!(command, "sudo -S -p '' sh -c 'echo hi'");
        assert_eq!(stdin, Some(b"admin\n".to_vec()));
        let (command, stdin) = as_root("echo hi", "");
        assert_eq!(command, "sudo -n sh -c 'echo hi'");
        assert!(stdin.is_none());
    }

    #[test]
    fn probe_commands_target_the_default_gateway() {
        let linux = probe_command("linux", 40001);
        assert!(linux.contains(PROBE_MARKER));
        assert!(linux.contains("ip route show default"));
        assert!(linux.contains("timeout 3 bash -c \"exec 3<>/dev/tcp/$gw/40001\""));
        let macos = probe_command("macos", 40001);
        assert!(macos.contains("route -n get default"));
        assert!(macos.contains("nc -v -z -G 3 -w 3 \"$gw\" 40001"));
    }

    #[test]
    fn probe_output_is_parsed() {
        assert_eq!(
            parse_probe("noise\ngateway=192.168.2.1 result=timeout\n"),
            ProbeOutcome {
                gateway: gateway(),
                result: ProbeResult::Timeout
            }
        );
        assert_eq!(
            parse_probe("gateway=192.168.2.1 result=open").result,
            ProbeResult::Open
        );
        assert_eq!(
            parse_probe("gateway=192.168.2.1 result=refused").result,
            ProbeResult::Refused
        );
        assert_eq!(
            parse_probe("gateway= result=error:no-default-route"),
            ProbeOutcome {
                gateway: None,
                result: ProbeResult::Error("error:no-default-route".to_string())
            }
        );
        assert!(matches!(
            parse_probe("ran: something").result,
            ProbeResult::Error(_)
        ));
    }

    #[test]
    fn only_a_silent_gateway_passes() {
        let timeout = ProbeOutcome {
            gateway: gateway(),
            result: ProbeResult::Timeout,
        };
        assert!(decide(&timeout, false, 40000, &subnet()).is_ok());

        let error = decide(&timeout, true, 40000, &subnet()).unwrap_err();
        assert!(
            error.contains("connected to this host at 192.168.2.1:40000"),
            "{error}"
        );

        for result in [
            ProbeResult::Open,
            ProbeResult::Refused,
            ProbeResult::Error("x".into()),
        ] {
            let outcome = ProbeOutcome {
                gateway: gateway(),
                result,
            };
            let error = decide(&outcome, false, 40000, &subnet()).unwrap_err();
            assert!(error.starts_with("guest isolation check failed"), "{error}");
            assert!(error.contains("com.apple/250.vm-service"), "{error}");
            assert!(
                error.contains("vm-service-guest-firewall install"),
                "{error}"
            );
            assert!(error.ends_with("(no NAT fallback)"), "{error}");
        }

        let no_gateway = ProbeOutcome {
            gateway: None,
            result: ProbeResult::Timeout,
        };
        assert!(decide(&no_gateway, false, 40000, &subnet()).is_err());
    }

    #[test]
    fn a_guest_outside_the_anchor_subnet_is_told_the_subnet_to_install() {
        let outcome = ProbeOutcome {
            gateway: Some("192.168.3.1".parse().unwrap()),
            result: ProbeResult::Open,
        };
        let error = decide(&outcome, false, 40000, &subnet()).unwrap_err();
        assert!(
            error.contains("covers 192.168.2.0/24, not the guest's subnet 192.168.3.0/24"),
            "{error}"
        );
        assert!(error.contains("install --subnet 192.168.3.0/24"), "{error}");
    }

    #[test]
    fn a_missing_anchor_names_the_install_command() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("absent");
        let error = guest_firewall_subnet(&missing).unwrap_err();
        assert!(error.starts_with("guest firewall not installed"), "{error}");
        assert!(
            error.contains("vm-service-guest-firewall install (no NAT fallback)"),
            "{error}"
        );

        let present = directory.path().join("anchor");
        std::fs::write(&present, guest_firewall::anchor_rules(&subnet())).unwrap();
        assert_eq!(guest_firewall_subnet(&present).unwrap(), subnet());

        std::fs::write(&present, "block drop all\n").unwrap();
        assert!(guest_firewall_subnet(&present)
            .unwrap_err()
            .starts_with("guest firewall unreadable"));
    }

    #[test]
    fn the_anchor_path_can_be_overridden() {
        assert_eq!(
            anchor_file(None),
            PathBuf::from("/etc/pf.anchors/com.wezzard.vm-service")
        );
        assert_eq!(
            anchor_file(Some("")),
            PathBuf::from("/etc/pf.anchors/com.wezzard.vm-service")
        );
        assert_eq!(anchor_file(Some("/tmp/a")), PathBuf::from("/tmp/a"));
    }
}
