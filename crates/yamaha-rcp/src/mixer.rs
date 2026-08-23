//! The typed console API: command builders over [`Connection`] with table-checked parameters.

use std::fmt::Display;
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::trace;

use crate::connection::{Connection, ConnectionConfig, ConnectionState, Notification};
use crate::params::{ParamInfo, ParamTable, NEG_INF};
use crate::wire::{quote, tokenize};
use crate::Error;

/// A group of channels or buses that share a parameter layout.
///
/// Every strip below has `Fader/Level`, `Fader/On`, `Label/Name` and `Label/Color` at the same
/// leaf paths, which is what lets one set of methods address all of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Strip {
    /// Mono input channel (`InCh`).
    Input,
    /// Stereo input channel (`StInCh`).
    StereoInput,
    /// Effect return channel (`FxRtnCh`).
    FxReturn,
    /// DCA group (`DCA`).
    Dca,
    /// Mix bus (`Mix`).
    Mix,
    /// Matrix bus (`Mtrx`).
    Matrix,
    /// Stereo master (`St`).
    Stereo,
    /// Mono master (`Mono`).
    Mono,
}

impl Strip {
    /// The RCP path prefix for this strip.
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Input => "MIXER:Current/InCh",
            Self::StereoInput => "MIXER:Current/StInCh",
            Self::FxReturn => "MIXER:Current/FxRtnCh",
            Self::Dca => "MIXER:Current/DCA",
            Self::Mix => "MIXER:Current/Mix",
            Self::Matrix => "MIXER:Current/Mtrx",
            Self::Stereo => "MIXER:Current/St",
            Self::Mono => "MIXER:Current/Mono",
        }
    }

    /// Build a full path for a leaf such as `Fader/Level`.
    pub fn path(self, leaf: &str) -> String {
        format!("{}/{leaf}", self.prefix())
    }

    /// The leaf prefix for this strip's sends: input-side strips feed mixes, output-side strips
    /// feed matrices. `None` for strips with no sends of their own.
    pub fn send_leaf(self) -> Option<&'static str> {
        match self {
            Self::Input | Self::StereoInput | Self::FxReturn => Some("ToMix"),
            Self::Mix | Self::Stereo | Self::Mono => Some("ToMtrx"),
            Self::Dca | Self::Matrix => None,
        }
    }
}

/// The colours a channel strip label can take.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum LabelColor {
    Purple,
    Pink,
    Red,
    Orange,
    Yellow,
    Blue,
    SkyBlue,
    Green,
}

impl LabelColor {
    /// Every colour, in the console's own order.
    pub const ALL: [LabelColor; 8] = [
        Self::Purple,
        Self::Pink,
        Self::Red,
        Self::Orange,
        Self::Yellow,
        Self::Blue,
        Self::SkyBlue,
        Self::Green,
    ];
}

impl Display for LabelColor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Purple => "Purple",
            Self::Pink => "Pink",
            Self::Red => "Red",
            Self::Orange => "Orange",
            Self::Yellow => "Yellow",
            Self::Blue => "Blue",
            Self::SkyBlue => "SkyBlue",
            Self::Green => "Green",
        };
        f.write_str(s)
    }
}

impl FromStr for LabelColor {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim_matches('"').to_lowercase().as_str() {
            "purple" => Ok(Self::Purple),
            "pink" => Ok(Self::Pink),
            "red" => Ok(Self::Red),
            "orange" => Ok(Self::Orange),
            "yellow" => Ok(Self::Yellow),
            "blue" => Ok(Self::Blue),
            "skyblue" | "sky_blue" | "sky blue" => Ok(Self::SkyBlue),
            "green" => Ok(Self::Green),
            _ => Err(Error::LabelColorParse(s.to_owned())),
        }
    }
}

/// Which of the console's two scene banks a scene lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum SceneList {
    A,
    B,
}

impl Display for SceneList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::A => "scene_a",
            Self::B => "scene_b",
        })
    }
}

impl FromStr for SceneList {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "a" | "scene_a" => Ok(Self::A),
            "b" | "scene_b" => Ok(Self::B),
            _ => Err(Error::SceneListParse(s.to_owned())),
        }
    }
}

/// The shape of a fade over time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Curve {
    /// Constant dB per second. Linear-in-dB is a good general fade law and is what a console
    /// operator moving a fader by hand approximates.
    #[default]
    Linear,
    /// Raised cosine: eases in and out, so the start and end of the move are less noticeable.
    Cosine,
    /// Starts slow and accelerates. Useful for ducking something out from under dialogue.
    Exponential,
}

impl Curve {
    /// Map linear progress `0.0..=1.0` onto eased progress.
    fn shape(self, t: f32) -> f32 {
        match self {
            Self::Linear => t,
            Self::Cosine => 0.5 - 0.5 * (t * std::f32::consts::PI).cos(),
            Self::Exponential => t * t,
        }
    }
}

/// A fader ramp.
#[derive(Clone, Copy, Debug)]
pub struct Fade {
    /// Starting level in raw console units. `None` reads the current level from the console first,
    /// which costs one round trip but guarantees the ramp does not jump on its first step.
    pub from: Option<i32>,
    /// Target level in raw console units.
    pub to: i32,
    /// Total duration of the ramp.
    pub duration: Duration,
    /// Interval between steps.
    pub tick: Duration,
    /// Shape of the ramp.
    pub curve: Curve,
}

impl Fade {
    /// A linear fade to `to` over `duration`, starting from the console's current level.
    pub fn to(to: i32, duration: Duration) -> Self {
        Self {
            from: None,
            to,
            duration,
            tick: Duration::from_millis(33),
            curve: Curve::Linear,
        }
    }

    /// Start from a known level instead of reading it back.
    pub fn from(mut self, from: i32) -> Self {
        self.from = Some(from);
        self
    }

    /// Set the ramp shape.
    pub fn curve(mut self, curve: Curve) -> Self {
        self.curve = curve;
        self
    }

    /// Set the step interval.
    pub fn tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }
}

/// A connected console.
///
/// Cheap to clone; every clone drives the same underlying connection.
#[derive(Clone, Debug)]
pub struct Mixer {
    conn: Connection,
    params: &'static ParamTable,
    confirm: bool,
}

impl Mixer {
    /// Connect to a console at `addr` with default tuning.
    ///
    /// Unlike the previous `TFMixer::new`, this really does open the connection before returning,
    /// so a wrong address or an unreachable console is reported here rather than at the first cue.
    pub async fn connect(addr: SocketAddr) -> Result<Self, Error> {
        Self::connect_with(addr, ConnectionConfig::default()).await
    }

    /// Connect with explicit tuning.
    pub async fn connect_with(addr: SocketAddr, config: ConnectionConfig) -> Result<Self, Error> {
        Ok(Self {
            conn: Connection::connect(addr, config).await?,
            params: ParamTable::tf(),
            confirm: true,
        })
    }

    /// Whether writes wait for the console to acknowledge them.
    ///
    /// The default is `true`, which surfaces a rejected command to the caller. A cue path should
    /// set this to `false`: it takes a network round trip out of the critical path, and a failure
    /// is logged instead of returned.
    pub fn confirm_writes(mut self, confirm: bool) -> Self {
        self.confirm = confirm;
        self
    }

    /// Watch the connection state.
    pub fn state(&self) -> tokio::sync::watch::Receiver<ConnectionState> {
        self.conn.state()
    }

    /// Whether the console is reachable right now.
    pub fn is_connected(&self) -> bool {
        self.conn.is_connected()
    }

    /// Subscribe to unsolicited console notifications.
    pub fn notifications(&self) -> tokio::sync::broadcast::Receiver<Notification> {
        self.conn.notifications()
    }

    /// The parameter table used to validate commands.
    pub fn params(&self) -> &'static ParamTable {
        self.params
    }

    // ---- primitives -------------------------------------------------------------------------

    /// Write an integer parameter, clamping it into the console's accepted range.
    async fn set_int(&self, path: &str, index: u32, sub: u32, value: i32) -> Result<(), Error> {
        let info = self.params.check_set(path, index, sub)?;
        let value = info.clamp(value);
        let cmd = format!("set {path} {index} {sub} {value}");
        self.write(cmd).await
    }

    /// Write a string parameter.
    async fn set_str(&self, path: &str, index: u32, sub: u32, value: &str) -> Result<(), Error> {
        let info = self.params.check_set(path, index, sub)?;
        // The table's `max` for a string parameter is its maximum length; over-long labels are
        // rejected outright by the console, so truncate rather than lose the whole command.
        let truncated: String = value.chars().take(info.max.max(0) as usize).collect();
        let cmd = format!("set {path} {index} {sub} {}", quote(&truncated));
        self.write(cmd).await
    }

    /// Send a write, waiting for acknowledgement only when [`Mixer::confirm_writes`] is set.
    async fn write(&self, cmd: String) -> Result<(), Error> {
        if self.confirm {
            self.conn.request(cmd).await.map(|_| ())
        } else {
            self.conn.send(cmd).await
        }
    }

    /// Read a parameter and return the value token of the reply.
    async fn get_value(&self, path: &str, index: u32, sub: u32) -> Result<String, Error> {
        self.params.check_get(path, index, sub)?;
        let cmd = format!("get {path} {index} {sub}");
        let response = self.conn.request(cmd).await?;
        tokenize(&response)
            .pop()
            .ok_or_else(|| Error::parse(response, "reply had no value token"))
    }

    /// Read an integer parameter.
    async fn get_int(&self, path: &str, index: u32, sub: u32) -> Result<i32, Error> {
        let value = self.get_value(path, index, sub).await?;
        value
            .parse()
            .map_err(|e| Error::parse(&value, format!("expected an integer: {e}")))
    }

    /// Read a boolean parameter. The console encodes these as `0` / `1`.
    async fn get_bool(&self, path: &str, index: u32, sub: u32) -> Result<bool, Error> {
        Ok(self.get_int(path, index, sub).await? != 0)
    }

    /// Look up a parameter's table entry, for dB conversion.
    fn info(&self, path: &str) -> Result<&'static ParamInfo, Error> {
        self.params.get(path)
    }

    // ---- faders -----------------------------------------------------------------------------

    /// Read a fader level in raw console units (hundredths of a dB; [`NEG_INF`] for -∞).
    pub async fn fader_level(&self, strip: Strip, index: u32) -> Result<i32, Error> {
        self.get_int(&strip.path("Fader/Level"), index, 0).await
    }

    /// Set a fader level in raw console units.
    pub async fn set_fader_level(&self, strip: Strip, index: u32, level: i32) -> Result<(), Error> {
        self.set_int(&strip.path("Fader/Level"), index, 0, level)
            .await
    }

    /// Read a fader level in decibels. Returns [`f32::NEG_INFINITY`] for a fader at -∞.
    pub async fn fader_db(&self, strip: Strip, index: u32) -> Result<f32, Error> {
        let path = strip.path("Fader/Level");
        let raw = self.get_int(&path, index, 0).await?;
        Ok(self.info(&path)?.raw_to_db(raw))
    }

    /// Set a fader level in decibels, clamping into range and mapping anything at or below the
    /// console's minimum to -∞.
    pub async fn set_fader_db(&self, strip: Strip, index: u32, db: f32) -> Result<(), Error> {
        let path = strip.path("Fader/Level");
        let raw = self.info(&path)?.db_to_raw(db);
        self.set_int(&path, index, 0, raw).await
    }

    // ---- on / mute --------------------------------------------------------------------------

    /// Whether the strip is on — that is, **audible**.
    ///
    /// This mirrors the console's own `Fader/On` parameter, where `1` means the channel is passing
    /// audio. The previous `muted()` returned this value directly and so reported the exact
    /// opposite of the truth; [`Mixer::muted`] is now its genuine inverse.
    pub async fn on(&self, strip: Strip, index: u32) -> Result<bool, Error> {
        self.get_bool(&strip.path("Fader/On"), index, 0).await
    }

    /// Turn a strip on (audible) or off.
    pub async fn set_on(&self, strip: Strip, index: u32, on: bool) -> Result<(), Error> {
        self.set_int(&strip.path("Fader/On"), index, 0, i32::from(on))
            .await
    }

    /// Whether the strip is muted. The inverse of [`Mixer::on`].
    pub async fn muted(&self, strip: Strip, index: u32) -> Result<bool, Error> {
        Ok(!self.on(strip, index).await?)
    }

    /// Mute or unmute a strip.
    pub async fn set_muted(&self, strip: Strip, index: u32, muted: bool) -> Result<(), Error> {
        self.set_on(strip, index, !muted).await
    }

    // ---- labels -----------------------------------------------------------------------------

    /// Read a strip's name.
    pub async fn label(&self, strip: Strip, index: u32) -> Result<String, Error> {
        self.get_value(&strip.path("Label/Name"), index, 0).await
    }

    /// Set a strip's name. Longer names are truncated to the console's limit.
    pub async fn set_label(&self, strip: Strip, index: u32, label: &str) -> Result<(), Error> {
        self.set_str(&strip.path("Label/Name"), index, 0, label)
            .await
    }

    /// Read a strip's label colour.
    pub async fn color(&self, strip: Strip, index: u32) -> Result<LabelColor, Error> {
        self.get_value(&strip.path("Label/Color"), index, 0)
            .await?
            .parse()
    }

    /// Set a strip's label colour.
    pub async fn set_color(
        &self,
        strip: Strip,
        index: u32,
        color: LabelColor,
    ) -> Result<(), Error> {
        self.set_str(&strip.path("Label/Color"), index, 0, &color.to_string())
            .await
    }

    // ---- sends ------------------------------------------------------------------------------

    /// The send path from `strip`, or an error if the strip has no sends.
    fn send_path(strip: Strip, leaf: &str) -> Result<String, Error> {
        let send = strip
            .send_leaf()
            .ok_or_else(|| Error::UnknownParam(strip.path(&format!("Send/{leaf}"))))?;
        Ok(strip.path(&format!("{send}/{leaf}")))
    }

    /// Read a send level in decibels (input strips send to mixes, output strips to matrices).
    pub async fn send_db(&self, strip: Strip, index: u32, dest: u32) -> Result<f32, Error> {
        let path = Self::send_path(strip, "Level")?;
        let raw = self.get_int(&path, index, dest).await?;
        Ok(self.info(&path)?.raw_to_db(raw))
    }

    /// Set a send level in decibels.
    pub async fn set_send_db(
        &self,
        strip: Strip,
        index: u32,
        dest: u32,
        db: f32,
    ) -> Result<(), Error> {
        let path = Self::send_path(strip, "Level")?;
        let raw = self.info(&path)?.db_to_raw(db);
        self.set_int(&path, index, dest, raw).await
    }

    /// Whether a send is on.
    pub async fn send_on(&self, strip: Strip, index: u32, dest: u32) -> Result<bool, Error> {
        self.get_bool(&Self::send_path(strip, "On")?, index, dest)
            .await
    }

    /// Turn a send on or off.
    pub async fn set_send_on(
        &self,
        strip: Strip,
        index: u32,
        dest: u32,
        on: bool,
    ) -> Result<(), Error> {
        self.set_int(&Self::send_path(strip, "On")?, index, dest, i32::from(on))
            .await
    }

    // ---- mute masters and scenes ------------------------------------------------------------

    /// Whether a mute master (mute group) is engaged.
    pub async fn mute_master(&self, group: u32) -> Result<bool, Error> {
        self.get_bool("MIXER:Current/MuteMaster/On", group, 0).await
    }

    /// Engage or release a mute master.
    pub async fn set_mute_master(&self, group: u32, engaged: bool) -> Result<(), Error> {
        self.set_int("MIXER:Current/MuteMaster/On", group, 0, i32::from(engaged))
            .await
    }

    /// Recall a scene from one of the console's two banks.
    pub async fn recall_scene(&self, list: SceneList, scene: u8) -> Result<(), Error> {
        self.write(format!("ssrecall_ex {list} {scene}")).await
    }

    /// Send a raw RCP command and return the console's reply.
    ///
    /// An escape hatch for parameters this crate does not model. The command is not validated
    /// against the parameter table.
    pub async fn raw(&self, command: impl Into<String>) -> Result<String, Error> {
        self.conn.request(command).await
    }

    // ---- fades ------------------------------------------------------------------------------

    /// Ramp a fader over time.
    ///
    /// Runs to completion; drop or abort the future to stop the ramp early, which leaves the fader
    /// wherever the last step put it. The final value is always written exactly, so rounding in
    /// the intermediate steps cannot leave a fader a hundredth of a dB off target.
    pub async fn fade(&self, strip: Strip, index: u32, fade: Fade) -> Result<(), Error> {
        let path = strip.path("Fader/Level");
        let info = self.params.check_set(&path, index, 0)?;

        let from = match fade.from {
            Some(v) => v,
            None => self.fader_level(strip, index).await?,
        };
        let from = info.clamp(from);
        let to = info.clamp(fade.to);

        // A zero-length (or sub-tick) fade is a jump. The previous implementation computed
        // `duration_ms / 50` here and divided by the result, panicking with a divide-by-zero for
        // any duration under 50 ms.
        let steps = (fade.duration.as_millis() / fade.tick.as_millis().max(1)) as u32;
        if steps <= 1 || from == to {
            return self.set_int(&path, index, 0, to).await;
        }

        // -∞ is a sentinel, not a value on the dB scale; ramping through it would send the fader
        // to the bottom on the first step. Start the ramp from the console's real minimum instead
        // and let the final write place the sentinel.
        let ramp_from = if from == NEG_INF { info.min } else { from };
        let ramp_to = if to == NEG_INF { info.min } else { to };
        let span = (ramp_to - ramp_from) as f32;

        let mut interval = tokio::time::interval(fade.tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await; // fires immediately

        for step in 1..steps {
            interval.tick().await;
            let t = fade.curve.shape(step as f32 / steps as f32);
            let value = ramp_from + (span * t).round() as i32;
            trace!(%path, index, value, "fade step");
            self.set_int(&path, index, 0, value).await?;
        }

        interval.tick().await;
        self.set_int(&path, index, 0, to).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_paths_match_the_parameter_table() {
        let t = ParamTable::tf();
        for strip in [
            Strip::Input,
            Strip::StereoInput,
            Strip::FxReturn,
            Strip::Dca,
            Strip::Mix,
            Strip::Matrix,
            Strip::Stereo,
            Strip::Mono,
        ] {
            for leaf in ["Fader/Level", "Fader/On", "Label/Name", "Label/Color"] {
                let path = strip.path(leaf);
                assert!(t.get(&path).is_ok(), "{path} missing from the table");
            }
        }
    }

    #[test]
    fn send_paths_match_the_parameter_table() {
        let t = ParamTable::tf();
        for strip in [Strip::Input, Strip::StereoInput, Strip::FxReturn] {
            let p = Mixer::send_path(strip, "Level").unwrap();
            assert!(p.ends_with("/ToMix/Level"), "{p}");
            assert!(t.get(&p).is_ok(), "{p} missing from the table");
        }
        for strip in [Strip::Mix, Strip::Stereo, Strip::Mono] {
            let p = Mixer::send_path(strip, "Level").unwrap();
            assert!(p.ends_with("/ToMtrx/Level"), "{p}");
            assert!(t.get(&p).is_ok(), "{p} missing from the table");
        }
    }

    #[test]
    fn strips_without_sends_are_rejected_rather_than_building_a_bogus_path() {
        assert!(Mixer::send_path(Strip::Dca, "Level").is_err());
        assert!(Mixer::send_path(Strip::Matrix, "Level").is_err());
    }

    #[test]
    fn label_color_round_trips_through_the_wire_format() {
        for c in LabelColor::ALL {
            assert_eq!(c.to_string().parse::<LabelColor>().unwrap(), c);
            // The console quotes colours in replies.
            assert_eq!(format!("\"{c}\"").parse::<LabelColor>().unwrap(), c);
        }
        assert!("chartreuse".parse::<LabelColor>().is_err());
    }

    #[test]
    fn scene_list_accepts_both_the_short_and_wire_spellings() {
        assert_eq!("a".parse::<SceneList>().unwrap(), SceneList::A);
        assert_eq!("scene_b".parse::<SceneList>().unwrap(), SceneList::B);
        assert_eq!(SceneList::A.to_string(), "scene_a");
        assert!("c".parse::<SceneList>().is_err());
    }

    #[test]
    fn curves_all_span_zero_to_one() {
        for curve in [Curve::Linear, Curve::Cosine, Curve::Exponential] {
            assert!((curve.shape(0.0) - 0.0).abs() < 1e-5, "{curve:?} at 0");
            assert!((curve.shape(1.0) - 1.0).abs() < 1e-5, "{curve:?} at 1");
        }
    }

    #[test]
    fn cosine_curve_is_symmetric_about_its_midpoint() {
        let c = Curve::Cosine;
        assert!((c.shape(0.5) - 0.5).abs() < 1e-5);
        assert!((c.shape(0.25) + c.shape(0.75) - 1.0).abs() < 1e-5);
    }
}
