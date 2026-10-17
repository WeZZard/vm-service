//! The host packet filter that keeps `internet` guests off this host (DC-14).
//!
//! Softnet passes a guest's traffic to the vmnet bridge's gateway address,
//! which is this host. The guest firewall is a pf anchor, loaded under the
//! default `/etc/pf.conf`'s `anchor "com.apple/*"`, that drops every
//! connection a guest on the Softnet subnet opens to one of this host's own
//! addresses. pf keeps state for the host's own connections to those guests,
//! so the replies to the service's SSH still pass.
//!
//! This crate generates the anchor rules and the LaunchDaemon that loads
//! them, and reads an installed anchor back. Installing needs root once; see
//! the `vm-service-guest-firewall` binary.

use std::fmt;
use std::net::Ipv4Addr;
use std::str::FromStr;

/// The pf anchor the rules are loaded into.
pub const ANCHOR: &str = "com.apple/250.vm-service";
/// The installed anchor file.
pub const ANCHOR_FILE: &str = "/etc/pf.anchors/com.wezzard.vm-service";
/// The LaunchDaemon label that loads the anchor at boot.
pub const LABEL: &str = "com.wezzard.vm-service.guest-firewall";
/// The installed LaunchDaemon property list.
pub const DAEMON_PLIST: &str = "/Library/LaunchDaemons/com.wezzard.vm-service.guest-firewall.plist";
/// The LaunchDaemon's log.
pub const DAEMON_LOG: &str = "/var/log/com.wezzard.vm-service.guest-firewall.log";
/// The installer executable's name; it is staged beside `vm-service`.
pub const INSTALLER: &str = "vm-service-guest-firewall";
/// How often, in seconds, the LaunchDaemon reloads the anchor, which also
/// restores it after the system reloads `/etc/pf.conf` at boot.
pub const RELOAD_INTERVAL_S: u32 = 300;

/// The subnet vmnet gave Softnet's guests on the host where DC-14 was
/// designed (macOS 27, Tart 2.32.1, Softnet 0.19.0).
///
/// Softnet passes no address to vmnet, so vmnet chooses the subnet; Softnet
/// does not fix it. Concurrent Softnet guests share it. A host whose Softnet
/// guests land elsewhere fails each `internet` acquisition's isolation check
/// with the subnet it saw, and the anchor is reinstalled with `--subnet`.
pub const DEFAULT_SUBNET: &str = "192.168.2.0/24";

/// The marker line that carries the subnet in an anchor file.
const SUBNET_MACRO: &str = "softnet_guests";

/// An IPv4 private subnet, the network address and its prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subnet {
    network: Ipv4Addr,
    prefix: u8,
}

/// A subnet that cannot be the Softnet subnet.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SubnetError(String);

impl Subnet {
    /// The network address.
    pub fn network(&self) -> Ipv4Addr {
        self.network
    }

    /// The prefix length.
    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    /// Whether `address` lies in this subnet.
    pub fn contains(&self, address: Ipv4Addr) -> bool {
        u32::from(address) & self.mask() == u32::from(self.network)
    }

    /// The `/24` that holds `address`, which is how vmnet sizes its subnets.
    pub fn around(address: Ipv4Addr) -> Subnet {
        let octets = address.octets();
        Subnet {
            network: Ipv4Addr::new(octets[0], octets[1], octets[2], 0),
            prefix: 24,
        }
    }

    fn mask(&self) -> u32 {
        if self.prefix == 0 {
            0
        } else {
            u32::MAX << (32 - u32::from(self.prefix))
        }
    }
}

impl FromStr for Subnet {
    type Err = SubnetError;

    /// Parse `a.b.c.d/n`. The subnet must be private (RFC 1918), as vmnet's
    /// are, between `/16` and `/30`, and written with its network address.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = || SubnetError(format!("invalid subnet {text:?}: expected a.b.c.d/n"));
        let (address, prefix) = text.split_once('/').ok_or_else(invalid)?;
        let network: Ipv4Addr = address.parse().map_err(|_| invalid())?;
        let prefix: u8 = prefix.parse().map_err(|_| invalid())?;
        if !(16..=30).contains(&prefix) {
            return Err(SubnetError(format!(
                "invalid subnet {text:?}: the prefix must be between /16 and /30"
            )));
        }
        if !network.is_private() {
            return Err(SubnetError(format!(
                "invalid subnet {text:?}: vmnet subnets are private (RFC 1918)"
            )));
        }
        let subnet = Subnet { network, prefix };
        if u32::from(network) & subnet.mask() != u32::from(network) {
            return Err(SubnetError(format!(
                "invalid subnet {text:?}: write it with its network address"
            )));
        }
        Ok(subnet)
    }
}

impl fmt::Display for Subnet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.network, self.prefix)
    }
}

/// The pf rules of the anchor for guests on `subnet`.
///
/// - The host's own connections to the guests create state, so their
///   replies, such as the guest's side of the service's SSH, pass.
/// - DHCP renewals from the guests reach bootpd(8) on the host.
/// - Every other TCP or UDP packet from the guests to one of the host's own
///   addresses is dropped. `(self)` follows the host's addresses as the vmnet
///   bridge comes and goes.
///
/// Softnet drops a guest's IPv6, so the rules are IPv4 only.
pub fn anchor_rules(subnet: &Subnet) -> String {
    format!(
        "# vm-service guest firewall (DC-14), loaded into the pf anchor {ANCHOR}.\n\
# Generated by {INSTALLER}; reinstall it rather than editing this file.\n\
{SUBNET_MACRO} = \"{subnet}\"\n\
# The host's own connections to the guests keep state, so their replies pass.\n\
pass out quick inet from (self) to ${SUBNET_MACRO} keep state\n\
# DHCP renewals reach bootpd(8) on the host.\n\
pass in quick inet proto udp from ${SUBNET_MACRO} port 68 to (self) port 67\n\
# Any other connection a guest opens to this host is dropped.\n\
block drop in quick inet proto {{ tcp udp }} from ${SUBNET_MACRO} to (self)\n"
    )
}

/// Read the subnet from an installed anchor file's text.
pub fn parse_anchor_subnet(text: &str) -> Option<Subnet> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once('=')?;
        if name.trim() != SUBNET_MACRO {
            return None;
        }
        value.trim().trim_matches('"').parse().ok()
    })
}

/// The shell command the LaunchDaemon runs: enable pf if it is disabled, then
/// load the anchor. Both steps are idempotent, so the daemon repeats them.
pub fn loader_command() -> String {
    format!(
        "/sbin/pfctl -s info 2>/dev/null | /usr/bin/grep -q '^Status: Enabled' || /sbin/pfctl -E; \
/sbin/pfctl -a {ANCHOR} -f {ANCHOR_FILE}"
    )
}

/// The LaunchDaemon property list that loads the anchor at boot and every
/// [`RELOAD_INTERVAL_S`] seconds.
pub fn daemon_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>/bin/sh</string>
		<string>-c</string>
		<string>{command}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>StartInterval</key>
	<integer>{interval}</integer>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#,
        label = xml_escape(LABEL),
        command = xml_escape(&loader_command()),
        interval = RELOAD_INTERVAL_S,
        log = xml_escape(DAEMON_LOG),
    )
}

/// The command an operator runs once, as root, to install the firewall for
/// `subnet`. `installer` is the path of the installer executable.
pub fn install_command(installer: &str, subnet: &Subnet) -> String {
    if subnet.to_string() == DEFAULT_SUBNET {
        format!("sudo {installer} install")
    } else {
        format!("sudo {installer} install --subnet {subnet}")
    }
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    fn default_subnet() -> Subnet {
        DEFAULT_SUBNET.parse().unwrap()
    }

    #[test]
    fn subnets_parse_and_display() {
        let subnet = default_subnet();
        assert_eq!(subnet.to_string(), "192.168.2.0/24");
        assert!(subnet.contains("192.168.2.1".parse().unwrap()));
        assert!(subnet.contains("192.168.2.254".parse().unwrap()));
        assert!(!subnet.contains("192.168.64.1".parse().unwrap()));
        assert_eq!(
            Subnet::around("192.168.64.7".parse().unwrap()).to_string(),
            "192.168.64.0/24"
        );
        assert!("10.20.0.0/16".parse::<Subnet>().is_ok());
    }

    #[test]
    fn bad_subnets_are_refused() {
        for text in [
            "192.168.2.1/24",
            "192.168.2.0",
            "192.168.2.0/8",
            "192.168.2.0/31",
            "8.8.8.0/24",
            "0.0.0.0/0",
            "fd00::/64",
            "",
        ] {
            assert!(text.parse::<Subnet>().is_err(), "{text}");
        }
    }

    #[test]
    fn rules_keep_host_state_pass_dhcp_and_drop_the_rest() {
        let rules = anchor_rules(&default_subnet());
        let lines: Vec<&str> = rules
            .lines()
            .filter(|line| !line.starts_with('#'))
            .collect();
        assert_eq!(
            lines,
            vec![
                "softnet_guests = \"192.168.2.0/24\"",
                "pass out quick inet from (self) to $softnet_guests keep state",
                "pass in quick inet proto udp from $softnet_guests port 68 to (self) port 67",
                "block drop in quick inet proto { tcp udp } from $softnet_guests to (self)",
            ]
        );
    }

    #[test]
    fn the_subnet_is_read_back_from_an_anchor() {
        let subnet: Subnet = "10.9.8.0/24".parse().unwrap();
        assert_eq!(parse_anchor_subnet(&anchor_rules(&subnet)), Some(subnet));
        assert_eq!(parse_anchor_subnet("block drop all\n"), None);
        assert_eq!(parse_anchor_subnet("softnet_guests = \"nonsense\"\n"), None);
    }

    /// `pfctl -n` parses without root and without touching the loaded rules.
    #[test]
    fn pfctl_accepts_the_rules() {
        if !Path::new("/sbin/pfctl").exists() {
            return;
        }
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), anchor_rules(&default_subnet())).unwrap();
        let output = Command::new("/sbin/pfctl")
            .arg("-nf")
            .arg(file.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "pfctl -nf failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn the_daemon_loads_the_anchor_at_boot_and_repeats() {
        let plist = daemon_plist();
        assert!(plist.contains("<string>com.wezzard.vm-service.guest-firewall</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(plist.contains("<integer>300</integer>"));
        assert!(plist.contains(
            "/sbin/pfctl -a com.apple/250.vm-service -f /etc/pf.anchors/com.wezzard.vm-service"
        ));
        // The shell operators are escaped for XML.
        assert!(plist.contains("|| /sbin/pfctl -E;"));
        assert!(plist.contains("&apos;^Status: Enabled&apos;"));
        assert!(!plist.contains("'^Status"));
    }

    #[test]
    fn plutil_accepts_the_daemon() {
        if !Path::new("/usr/bin/plutil").exists() {
            return;
        }
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), daemon_plist()).unwrap();
        let lint = Command::new("/usr/bin/plutil")
            .arg("-lint")
            .arg(file.path())
            .output()
            .unwrap();
        assert!(
            lint.status.success(),
            "{}",
            String::from_utf8_lossy(&lint.stdout)
        );
        let json = Command::new("/usr/bin/plutil")
            .args(["-extract", "ProgramArguments.2", "raw", "-o", "-"])
            .arg(file.path())
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&json.stdout).trim_end(),
            loader_command()
        );
    }

    #[test]
    fn the_install_command_names_a_non_default_subnet() {
        assert_eq!(
            install_command("/x/vm-service-guest-firewall", &default_subnet()),
            "sudo /x/vm-service-guest-firewall install"
        );
        assert_eq!(
            install_command(
                "/x/vm-service-guest-firewall",
                &"192.168.3.0/24".parse().unwrap()
            ),
            "sudo /x/vm-service-guest-firewall install --subnet 192.168.3.0/24"
        );
    }
}
