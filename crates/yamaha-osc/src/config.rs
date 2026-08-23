//! Configuration: one TOML file, validated before it is allowed to take effect.
//!
//! A show cannot be stopped to debug a config file, so validation is deliberately strict and
//! happens up front — unknown keys are rejected rather than silently ignored, because a typo in a
//! key name that is quietly dropped is exactly the failure that shows up mid-performance.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// The whole configuration file.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub console: ConsoleConfig,
    #[serde(default)]
    pub osc: OscConfig,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub health: HealthConfig,
    /// Friendly names for strip indices, so a cue can read `/ch/lead_vox/fader -6`.
    ///
    /// Names are not scoped to a strip type: `band = "1-8"` can be used as `/ch/band/on 0` or
    /// `/dca/band/on 0`. Values are 1-based, matching the console surface and the config's
    /// `channel_base`.
    #[serde(default)]
    pub names: HashMap<String, NameSpec>,
}

/// How to reach the console.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConsoleConfig {
    /// Explicit console address. Takes precedence over `discover`.
    #[serde(default)]
    pub address: Option<String>,
    /// CIDR to scan for a console when `address` is unset, e.g. `"192.168.50.0/24"`.
    #[serde(default)]
    pub discover: Option<String>,
    /// Local interface address to bind the console socket to.
    ///
    /// Set this to the address of a wired USB-Ethernet adapter to keep the cue path off Wi-Fi;
    /// without it the OS picks a route, which may silently fail over to a wireless interface.
    #[serde(default)]
    pub bind_address: Option<String>,
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default = "default_response_timeout_ms")]
    pub response_timeout_ms: u64,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        Self {
            address: None,
            discover: None,
            bind_address: None,
            connect_timeout_ms: default_connect_timeout_ms(),
            response_timeout_ms: default_response_timeout_ms(),
        }
    }
}

fn default_connect_timeout_ms() -> u64 {
    3000
}

fn default_response_timeout_ms() -> u64 {
    1000
}

/// Which transport QLab will use to reach the bridge.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// Plain OSC datagrams. The right choice on loopback: no handshake, no head-of-line blocking.
    #[default]
    Udp,
    /// OSC over TCP with double-END SLIP framing (RFC 1055), as OSC 1.1 and QLab require.
    /// Use when the bridge is on a different machine from QLab.
    Tcp,
}

/// Where the bridge listens for OSC.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OscConfig {
    pub listen: String,
    #[serde(default)]
    pub transport: Transport,
    /// The OSC channel number that maps to the console's channel index 0.
    ///
    /// RCP is 0-based (`InCh 0` is the console's channel 1) but operators and the console surface
    /// are 1-based, so this defaults to 1 and the conversion happens here rather than in cues.
    #[serde(default = "default_channel_base")]
    pub channel_base: u32,
}

impl Default for OscConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:53100".to_owned(),
            transport: Transport::default(),
            channel_base: default_channel_base(),
        }
    }
}

fn default_channel_base() -> u32 {
    1
}

/// Rate limits protecting the console.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Minimum spacing between commands sent to the console.
    #[serde(default = "default_min_interval_ms")]
    pub min_command_interval_ms: u64,
    /// Step rate for bridge-driven fades.
    #[serde(default = "default_fade_tick_hz")]
    pub fade_tick_hz: u32,
    /// Coalesce superseded fader moves instead of queueing every one.
    #[serde(default = "default_true")]
    pub coalesce: bool,
    /// Maximum actions buffered from OSC before the oldest coalescable ones are dropped.
    #[serde(default = "default_queue_depth")]
    pub queue_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            min_command_interval_ms: default_min_interval_ms(),
            fade_tick_hz: default_fade_tick_hz(),
            coalesce: true,
            queue_depth: default_queue_depth(),
        }
    }
}

fn default_min_interval_ms() -> u64 {
    5
}

fn default_fade_tick_hz() -> u32 {
    30
}

fn default_true() -> bool {
    true
}

fn default_queue_depth() -> usize {
    1024
}

/// Optional health reporting back out over OSC.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HealthConfig {
    /// Where to send `/bridge/status` and `/bridge/error`. Unset disables health output.
    #[serde(default)]
    pub destination: Option<String>,
    /// How often to repeat the current status, in seconds. `0` sends only on change.
    #[serde(default)]
    pub heartbeat_s: u64,
}

/// A named strip index or set of indices, as written in the `[names]` table.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum NameSpec {
    /// A single 1-based index: `lead_vox = 5`.
    One(u32),
    /// A list of 1-based indices: `band = [1, 3, 5]`.
    Many(Vec<u32>),
    /// A range or comma-separated list: `band = "1-8"`, `spot = "1,4,7"`, `pair = "1-2,9"`.
    Spec(String),
}

impl NameSpec {
    /// Expand to 1-based indices, rejecting anything that is not a number, range, or list of them.
    pub fn expand(&self) -> Result<Vec<u32>> {
        match self {
            Self::One(n) => Ok(vec![*n]),
            Self::Many(v) => Ok(v.clone()),
            Self::Spec(s) => {
                let mut out = Vec::new();
                for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    match part.split_once('-') {
                        Some((lo, hi)) => {
                            let lo: u32 = lo.trim().parse().with_context(|| {
                                format!("{part:?}: range start is not a number")
                            })?;
                            let hi: u32 = hi
                                .trim()
                                .parse()
                                .with_context(|| format!("{part:?}: range end is not a number"))?;
                            if hi < lo {
                                bail!("{part:?}: range end is before its start");
                            }
                            out.extend(lo..=hi);
                        }
                        None => out.push(
                            part.parse()
                                .with_context(|| format!("{part:?} is not a number"))?,
                        ),
                    }
                }
                if out.is_empty() {
                    bail!("{s:?} does not name any channels");
                }
                Ok(out)
            }
        }
    }
}

impl Config {
    /// Read and validate a config file.
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config
            .validate()
            .with_context(|| format!("validating {}", path.display()))?;
        Ok(config)
    }

    /// Check everything that can be checked without touching the network.
    ///
    /// Called before a config is allowed to replace a running one, so a bad edit is rejected
    /// rather than applied.
    pub fn validate(&self) -> Result<()> {
        if self.console.address.is_none() && self.console.discover.is_none() {
            bail!("console.address or console.discover must be set");
        }
        if let Some(addr) = &self.console.address {
            resolve_console_addr(addr)?;
        }
        if let Some(bind) = &self.console.bind_address {
            bind.parse::<std::net::IpAddr>()
                .with_context(|| format!("console.bind_address {bind:?} is not an IP address"))?;
        }
        self.osc
            .listen
            .parse::<SocketAddr>()
            .with_context(|| format!("osc.listen {:?} is not an address:port", self.osc.listen))?;
        if let Some(dest) = &self.health.destination {
            dest.parse::<SocketAddr>()
                .with_context(|| format!("health.destination {dest:?} is not an address:port"))?;
        }
        if self.limits.fade_tick_hz == 0 || self.limits.fade_tick_hz > 100 {
            bail!(
                "limits.fade_tick_hz must be between 1 and 100 (got {})",
                self.limits.fade_tick_hz
            );
        }
        if self.limits.queue_depth == 0 {
            bail!("limits.queue_depth must be at least 1");
        }

        for (name, spec) in &self.names {
            let indices = spec
                .expand()
                .with_context(|| format!("names.{name} is not a valid channel selector"))?;
            if indices.iter().any(|&i| i < self.osc.channel_base) {
                bail!(
                    "names.{name} includes an index below channel_base ({})",
                    self.osc.channel_base
                );
            }
            // A name that looks like a number would shadow, or be shadowed by, a plain index.
            if name.chars().all(|c| c.is_ascii_digit()) {
                bail!("names.{name} is all digits, which collides with plain channel numbers");
            }
        }

        Ok(())
    }

    /// Names expanded to 0-based console indices, ready for the router.
    pub fn resolved_names(&self) -> Result<HashMap<String, Vec<u32>>> {
        let base = self.osc.channel_base;
        self.names
            .iter()
            .map(|(name, spec)| {
                let indices = spec
                    .expand()?
                    .into_iter()
                    .map(|i| i.saturating_sub(base))
                    .collect();
                Ok((name.clone(), indices))
            })
            .collect()
    }

    pub fn min_command_interval(&self) -> Duration {
        Duration::from_millis(self.limits.min_command_interval_ms)
    }

    pub fn fade_tick(&self) -> Duration {
        Duration::from_micros(1_000_000 / u64::from(self.limits.fade_tick_hz.max(1)))
    }

    /// The example config shipped alongside the binary, used by `yamaha-osc init`.
    pub fn example() -> &'static str {
        include_str!("../../../config/yamaha-osc.example.toml")
    }
}

/// Parse a console address, defaulting the port to the RCP port if none is given.
pub fn resolve_console_addr(addr: &str) -> Result<SocketAddr> {
    if let Ok(sa) = addr.parse::<SocketAddr>() {
        return Ok(sa);
    }
    if let Ok(ip) = addr.parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, yamaha_rcp::DEFAULT_PORT));
    }
    bail!("console.address {addr:?} is not an IP address or address:port")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Result<Config> {
        let c: Config = toml::from_str(toml)?;
        c.validate()?;
        Ok(c)
    }

    #[test]
    fn the_shipped_example_config_is_valid() {
        let c: Config = toml::from_str(Config::example()).expect("example config must parse");
        c.validate().expect("example config must validate");
    }

    #[test]
    fn a_minimal_config_works() {
        let c = parse(
            r#"[console]
address = "192.168.50.20:49280"
"#,
        )
        .unwrap();
        assert_eq!(c.osc.channel_base, 1);
        assert_eq!(c.limits.min_command_interval_ms, 5);
    }

    #[test]
    fn a_bare_ip_gets_the_default_rcp_port() {
        let c = parse(
            r#"[console]
address = "192.168.50.20"
"#,
        )
        .unwrap();
        let addr = resolve_console_addr(c.console.address.as_ref().unwrap()).unwrap();
        assert_eq!(addr.port(), 49280);
    }

    /// A silently-ignored typo in a key name is the failure mode that surfaces mid-show.
    #[test]
    fn unknown_keys_are_rejected_rather_than_ignored() {
        let err = parse(
            r#"[console]
address = "192.168.50.20:49280"
conect_timeout_ms = 500
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("conect_timeout_ms"), "{err}");
    }

    #[test]
    fn a_console_must_be_addressable_somehow() {
        let err = parse("[osc]\nlisten = \"127.0.0.1:53100\"\n").unwrap_err();
        assert!(err.to_string().contains("console.address"), "{err}");
    }

    #[test]
    fn name_specs_expand_to_indices() {
        assert_eq!(NameSpec::One(5).expand().unwrap(), [5]);
        assert_eq!(NameSpec::Many(vec![1, 3]).expand().unwrap(), [1, 3]);
        assert_eq!(NameSpec::Spec("1-4".into()).expand().unwrap(), [1, 2, 3, 4]);
        assert_eq!(NameSpec::Spec("1-2,9".into()).expand().unwrap(), [1, 2, 9]);
    }

    #[test]
    fn a_backwards_range_is_an_error_not_an_empty_set() {
        assert!(NameSpec::Spec("8-1".into()).expand().is_err());
        assert!(NameSpec::Spec("".into()).expand().is_err());
        assert!(NameSpec::Spec("lead".into()).expand().is_err());
    }

    #[test]
    fn names_are_resolved_to_zero_based_console_indices() {
        let c = parse(
            r#"[console]
address = "192.168.50.20:49280"

[names]
lead_vox = 5
band = "1-3"
"#,
        )
        .unwrap();
        let names = c.resolved_names().unwrap();
        assert_eq!(names["lead_vox"], [4], "channel 5 is console index 4");
        assert_eq!(names["band"], [0, 1, 2]);
    }

    #[test]
    fn an_all_digit_name_is_rejected_because_it_would_shadow_a_channel_number() {
        let err = parse(
            r#"[console]
address = "192.168.50.20:49280"

[names]
12 = 5
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("all digits"), "{err}");
    }

    #[test]
    fn fade_tick_hz_is_bounded() {
        for hz in ["0", "500"] {
            let err = parse(&format!(
                "[console]\naddress = \"1.2.3.4\"\n\n[limits]\nfade_tick_hz = {hz}\n"
            ))
            .unwrap_err();
            assert!(err.to_string().contains("fade_tick_hz"), "{err}");
        }
    }

    #[test]
    fn fade_tick_converts_hz_to_an_interval() {
        let c = parse("[console]\naddress = \"1.2.3.4\"\n\n[limits]\nfade_tick_hz = 20\n").unwrap();
        assert_eq!(c.fade_tick(), Duration::from_millis(50));
    }
}
