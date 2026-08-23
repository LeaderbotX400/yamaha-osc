//! Turning an OSC message into console actions.
//!
//! The address grammar is deliberately small and regular:
//!
//! ```text
//! /<strip>/<selector>/<parameter> [args...]
//! ```
//!
//! where `<strip>` is one of `ch stin fxrtn dca mix mtrx st mono`, `<selector>` is a channel
//! number, a configured name, or a pattern (see [`crate::address`]), and `<parameter>` is one of a
//! fixed vocabulary. A selector covering several channels produces one action per channel, so a
//! single cue can move a group.

use std::str::FromStr;
use std::time::Duration;

use rosc::{OscMessage, OscType};
use yamaha_rcp::{Curve, LabelColor, ParamTable, SceneList, Strip};

use crate::address::{Resolver, SelectorError};
use crate::value;

/// What a level change came from, so a fade's own steps do not cancel the fade.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// A direct `/fader` set, or any other explicit write. Cancels a fade on the same target.
    Direct,
    /// A step emitted by a running bridge fade.
    Fade,
}

/// Something on the console that carries a level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    /// A strip's own fader.
    Fader { strip: Strip, index: u32 },
    /// A send from a strip to a bus.
    Send { strip: Strip, index: u32, dest: u32 },
}

impl Target {
    /// The RCP path this target writes to.
    pub fn level_path(self) -> String {
        match self {
            Self::Fader { strip, .. } => strip.path("Fader/Level"),
            Self::Send { strip, .. } => {
                strip.path(&format!("{}/Level", strip.send_leaf().unwrap_or("ToMix")))
            }
        }
    }

    /// The instance index on that path.
    pub fn index(self) -> u32 {
        match self {
            Self::Fader { index, .. } | Self::Send { index, .. } => index,
        }
    }

    /// The sub-index on that path.
    pub fn sub(self) -> u32 {
        match self {
            Self::Fader { .. } => 0,
            Self::Send { dest, .. } => dest,
        }
    }
}

/// Something on the console that can be switched on or off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Switch {
    Strip { strip: Strip, index: u32 },
    Send { strip: Strip, index: u32, dest: u32 },
    MuteMaster { group: u32 },
}

/// A bridge-driven fade.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FadeSpec {
    pub target: Target,
    /// Where to start. `None` means read the console's current level first.
    pub from: Option<i32>,
    pub to: i32,
    pub duration: Duration,
    pub curve: Curve,
}

/// One console operation.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Set a level, in raw console units.
    SetLevel {
        target: Target,
        raw: i32,
        source: Source,
    },
    /// Switch something on or off.
    SetSwitch { switch: Switch, on: bool },
    /// Rename a strip.
    SetLabel {
        strip: Strip,
        index: u32,
        name: String,
    },
    /// Recolour a strip.
    SetColor {
        strip: Strip,
        index: u32,
        color: LabelColor,
    },
    /// Recall a scene.
    RecallScene { list: SceneList, scene: u32 },
    /// Start a bridge-driven ramp.
    StartFade(FadeSpec),
    /// Take every input, DCA and master fader to -∞.
    Panic,
    /// Re-read the config file.
    Reload,
}

impl Action {
    /// Whether this action may be superseded by a newer one for the same target.
    ///
    /// Only continuous level moves qualify: for a fader mid-fade, only the newest value matters.
    /// Everything else is discrete — dropping a mute or a scene recall would be a show error, so
    /// they are always delivered.
    pub fn coalesce_key(&self) -> Option<Target> {
        match self {
            Self::SetLevel { target, .. } => Some(*target),
            _ => None,
        }
    }
}

/// Why a message could not be translated.
#[derive(Debug, thiserror::Error)]
pub enum RouteError {
    #[error("unknown address {0:?}")]
    UnknownAddress(String),
    #[error("{address:?} takes {expected}, got {got} argument(s)")]
    WrongArity {
        address: String,
        expected: &'static str,
        got: usize,
    },
    #[error("{address:?}: argument {position} should be {expected}, got {got}")]
    BadArgument {
        address: String,
        position: usize,
        expected: &'static str,
        got: String,
    },
    #[error("{address:?}: {source}")]
    BadSelector {
        address: String,
        #[source]
        source: SelectorError,
    },
    #[error("{0}")]
    Unsupported(String),
}

/// Translates OSC messages into actions.
#[derive(Clone, Debug)]
pub struct Router {
    resolver: Resolver,
    params: &'static ParamTable,
}

impl Router {
    pub fn new(resolver: Resolver) -> Self {
        Self {
            resolver,
            params: ParamTable::tf(),
        }
    }

    /// Translate one message. A selector covering several channels yields several actions.
    pub fn route(&self, msg: &OscMessage) -> Result<Vec<Action>, RouteError> {
        let address = msg.addr.trim_end_matches('/');
        let segments: Vec<&str> = address.trim_start_matches('/').split('/').collect();
        let args = &msg.args;

        match segments.as_slice() {
            ["bridge", "panic"] => Ok(vec![Action::Panic]),
            ["bridge", "reload"] => Ok(vec![Action::Reload]),

            ["scene", "recall"] => self.scene(address, args),

            ["mutegroup", selector] => {
                let on = self.arg_bool(address, args, 0)?;
                let count = self.count("MIXER:Current/MuteMaster/On");
                Ok(self
                    .select(address, selector, count)?
                    .into_iter()
                    .map(|group| Action::SetSwitch {
                        switch: Switch::MuteMaster { group },
                        on,
                    })
                    .collect())
            }

            [strip, selector, rest @ ..] => {
                let Some(strip) = strip_from_word(strip) else {
                    return Err(RouteError::UnknownAddress(address.to_owned()));
                };
                let count = self.count(&strip.path("Fader/Level"));
                let indices = self.select(address, selector, count)?;
                self.parameter(address, strip, &indices, rest, args)
            }

            _ => Err(RouteError::UnknownAddress(address.to_owned())),
        }
    }

    /// How many instances a path has, per the console's parameter table.
    fn count(&self, path: &str) -> u32 {
        self.params.get(path).map_or(0, |p| p.instances())
    }

    fn select(&self, address: &str, selector: &str, count: u32) -> Result<Vec<u32>, RouteError> {
        self.resolver
            .resolve(selector, count)
            .map_err(|source| RouteError::BadSelector {
                address: address.to_owned(),
                source,
            })
    }

    /// Dispatch on the parameter part of the address.
    fn parameter(
        &self,
        address: &str,
        strip: Strip,
        indices: &[u32],
        rest: &[&str],
        args: &[OscType],
    ) -> Result<Vec<Action>, RouteError> {
        match rest {
            ["fader"] => {
                let db = self.arg_f32(address, args, 0)?;
                let path = strip.path("Fader/Level");
                let raw = self.db_to_raw(&path, db);
                Ok(self.levels(strip, indices, None, raw))
            }
            ["fader", "raw"] => {
                let raw = self.arg_f32(address, args, 0)? as i32;
                Ok(self.levels(strip, indices, None, raw))
            }
            ["on"] => {
                let on = self.arg_bool(address, args, 0)?;
                Ok(self.switches(strip, indices, None, on))
            }
            ["mute"] => {
                // Muting a channel turns the console's Fader/On *off*; the two addresses exist so
                // a cue can be written whichever way round reads more clearly.
                let muted = self.arg_bool(address, args, 0)?;
                Ok(self.switches(strip, indices, None, !muted))
            }
            ["fade"] => {
                let spec = self.fade_args(address, args)?;
                let path = strip.path("Fader/Level");
                Ok(indices
                    .iter()
                    .map(|&index| {
                        Action::StartFade(FadeSpec {
                            target: Target::Fader { strip, index },
                            from: spec.2.map(|db| self.db_to_raw(&path, db)),
                            to: self.db_to_raw(&path, spec.0),
                            duration: spec.1,
                            curve: spec.3,
                        })
                    })
                    .collect())
            }
            ["name"] => {
                let name = self.arg_string(address, args, 0)?;
                Ok(indices
                    .iter()
                    .map(|&index| Action::SetLabel {
                        strip,
                        index,
                        name: name.clone(),
                    })
                    .collect())
            }
            ["color"] | ["colour"] => {
                let text = self.arg_string(address, args, 0)?;
                let color = LabelColor::from_str(&text).map_err(|_| RouteError::BadArgument {
                    address: address.to_owned(),
                    position: 0,
                    expected: "a label colour (Purple Pink Red Orange Yellow Blue SkyBlue Green)",
                    got: text.clone(),
                })?;
                Ok(indices
                    .iter()
                    .map(|&index| Action::SetColor {
                        strip,
                        index,
                        color,
                    })
                    .collect())
            }
            ["send", bus, dest, leaf] => self.send(address, strip, indices, bus, dest, leaf, args),

            _ => Err(RouteError::UnknownAddress(address.to_owned())),
        }
    }

    /// Sends: `/ch/5/send/mix/3/fader`.
    ///
    /// The bus word is redundant — an input strip can only send to a mix, an output strip only to
    /// a matrix — but requiring it keeps the cue readable and catches a cue aimed at the wrong
    /// kind of bus.
    #[allow(clippy::too_many_arguments)]
    fn send(
        &self,
        address: &str,
        strip: Strip,
        indices: &[u32],
        bus: &str,
        dest: &str,
        leaf: &str,
        args: &[OscType],
    ) -> Result<Vec<Action>, RouteError> {
        let Some(send_leaf) = strip.send_leaf() else {
            return Err(RouteError::Unsupported(format!(
                "{address:?}: {strip:?} strips have no sends"
            )));
        };
        let expected_word = match send_leaf {
            "ToMix" => "mix",
            _ => "mtrx",
        };
        if bus != expected_word {
            return Err(RouteError::Unsupported(format!(
                "{address:?}: this strip sends to {expected_word}, not {bus:?}"
            )));
        }

        let path = strip.path(&format!("{send_leaf}/Level"));
        let dest_count = self.params.get(&path).map_or(0, |p| p.subs());
        let dests = self.select(address, dest, dest_count)?;

        let targets = |dests: &[u32]| -> Vec<Target> {
            indices
                .iter()
                .flat_map(|&index| {
                    dests.iter().map(move |&d| Target::Send {
                        strip,
                        index,
                        dest: d,
                    })
                })
                .collect()
        };

        match leaf {
            "fader" => {
                let db = self.arg_f32(address, args, 0)?;
                let raw = self.db_to_raw(&path, db);
                Ok(targets(&dests)
                    .into_iter()
                    .map(|target| Action::SetLevel {
                        target,
                        raw,
                        source: Source::Direct,
                    })
                    .collect())
            }
            "on" => {
                let on = self.arg_bool(address, args, 0)?;
                Ok(indices
                    .iter()
                    .flat_map(|&index| {
                        dests.iter().map(move |&d| Action::SetSwitch {
                            switch: Switch::Send {
                                strip,
                                index,
                                dest: d,
                            },
                            on,
                        })
                    })
                    .collect())
            }
            "fade" => {
                let (to_db, duration, from_db, curve) = self.fade_args(address, args)?;
                Ok(targets(&dests)
                    .into_iter()
                    .map(|target| {
                        Action::StartFade(FadeSpec {
                            target,
                            from: from_db.map(|db| self.db_to_raw(&path, db)),
                            to: self.db_to_raw(&path, to_db),
                            duration,
                            curve,
                        })
                    })
                    .collect())
            }
            _ => Err(RouteError::UnknownAddress(address.to_owned())),
        }
    }

    fn scene(&self, address: &str, args: &[OscType]) -> Result<Vec<Action>, RouteError> {
        if args.len() != 2 {
            return Err(RouteError::WrongArity {
                address: address.to_owned(),
                expected: "a scene list (a|b) and a scene number",
                got: args.len(),
            });
        }
        let list_text = self.arg_string(address, args, 0)?;
        let list = SceneList::from_str(&list_text).map_err(|_| RouteError::BadArgument {
            address: address.to_owned(),
            position: 0,
            expected: "a scene list: a or b",
            got: list_text,
        })?;
        let scene = self.arg_u32(address, args, 1)?;
        Ok(vec![Action::RecallScene { list, scene }])
    }

    /// `<to_db> <duration_ms> [curve] [from_db]` in any order after the first two.
    fn fade_args(
        &self,
        address: &str,
        args: &[OscType],
    ) -> Result<(f32, Duration, Option<f32>, Curve), RouteError> {
        if args.len() < 2 || args.len() > 4 {
            return Err(RouteError::WrongArity {
                address: address.to_owned(),
                expected:
                    "a target level, a duration in ms, and optionally a curve and start level",
                got: args.len(),
            });
        }
        let to = self.arg_f32(address, args, 0)?;
        let ms = self.arg_u32(address, args, 1)?;

        let mut curve = Curve::Linear;
        let mut from = None;
        for (position, arg) in args.iter().enumerate().skip(2) {
            match arg {
                // A trailing number is the start level; anything else must be a curve name.
                OscType::String(s) if parse_curve(s).is_some() => {
                    curve = parse_curve(s).expect("just checked");
                }
                other => {
                    from = Some(value::as_f32(other).ok_or_else(|| RouteError::BadArgument {
                        address: address.to_owned(),
                        position,
                        expected: "a curve name (linear|cosine|exp) or a start level in dB",
                        got: value::type_name(other).to_owned(),
                    })?);
                }
            }
        }

        Ok((to, Duration::from_millis(u64::from(ms)), from, curve))
    }

    fn levels(&self, strip: Strip, indices: &[u32], dest: Option<u32>, raw: i32) -> Vec<Action> {
        indices
            .iter()
            .map(|&index| Action::SetLevel {
                target: match dest {
                    Some(dest) => Target::Send { strip, index, dest },
                    None => Target::Fader { strip, index },
                },
                raw,
                source: Source::Direct,
            })
            .collect()
    }

    fn switches(&self, strip: Strip, indices: &[u32], dest: Option<u32>, on: bool) -> Vec<Action> {
        indices
            .iter()
            .map(|&index| Action::SetSwitch {
                switch: match dest {
                    Some(dest) => Switch::Send { strip, index, dest },
                    None => Switch::Strip { strip, index },
                },
                on,
            })
            .collect()
    }

    /// Convert dB to raw units using the parameter's own scale, falling back to hundredths for a
    /// path the table does not know.
    fn db_to_raw(&self, path: &str, db: f32) -> i32 {
        match self.params.get(path) {
            Ok(info) => info.db_to_raw(db),
            Err(_) => (db * 100.0).round() as i32,
        }
    }

    // ---- argument accessors -----------------------------------------------------------------

    fn arg<'a>(
        &self,
        address: &str,
        args: &'a [OscType],
        position: usize,
        expected: &'static str,
    ) -> Result<&'a OscType, RouteError> {
        args.get(position).ok_or_else(|| RouteError::WrongArity {
            address: address.to_owned(),
            expected,
            got: args.len(),
        })
    }

    fn arg_f32(&self, address: &str, args: &[OscType], position: usize) -> Result<f32, RouteError> {
        let arg = self.arg(address, args, position, "a level in dB")?;
        value::as_f32(arg).ok_or_else(|| RouteError::BadArgument {
            address: address.to_owned(),
            position,
            expected: "a number",
            got: value::type_name(arg).to_owned(),
        })
    }

    fn arg_u32(&self, address: &str, args: &[OscType], position: usize) -> Result<u32, RouteError> {
        let arg = self.arg(address, args, position, "a whole number")?;
        value::as_u32(arg).ok_or_else(|| RouteError::BadArgument {
            address: address.to_owned(),
            position,
            expected: "a whole, non-negative number",
            got: value::type_name(arg).to_owned(),
        })
    }

    fn arg_bool(
        &self,
        address: &str,
        args: &[OscType],
        position: usize,
    ) -> Result<bool, RouteError> {
        let arg = self.arg(address, args, position, "0 or 1")?;
        value::as_bool(arg).ok_or_else(|| RouteError::BadArgument {
            address: address.to_owned(),
            position,
            expected: "0 or 1 (or on/off, true/false)",
            got: value::type_name(arg).to_owned(),
        })
    }

    fn arg_string(
        &self,
        address: &str,
        args: &[OscType],
        position: usize,
    ) -> Result<String, RouteError> {
        let arg = self.arg(address, args, position, "some text")?;
        value::as_string(arg).ok_or_else(|| RouteError::BadArgument {
            address: address.to_owned(),
            position,
            expected: "text",
            got: value::type_name(arg).to_owned(),
        })
    }
}

/// Map the strip word in an address to a console strip.
fn strip_from_word(word: &str) -> Option<Strip> {
    Some(match word {
        "ch" | "input" => Strip::Input,
        "stin" => Strip::StereoInput,
        "fxrtn" => Strip::FxReturn,
        "dca" => Strip::Dca,
        "mix" => Strip::Mix,
        "mtrx" | "matrix" => Strip::Matrix,
        "st" | "stereo" => Strip::Stereo,
        "mono" => Strip::Mono,
        _ => return None,
    })
}

fn parse_curve(s: &str) -> Option<Curve> {
    match s.trim().to_ascii_lowercase().as_str() {
        "linear" | "lin" => Some(Curve::Linear),
        "cosine" | "cos" | "s" | "scurve" => Some(Curve::Cosine),
        "exp" | "exponential" => Some(Curve::Exponential),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn router() -> Router {
        let mut names = HashMap::new();
        names.insert("lead_vox".to_owned(), vec![4]);
        names.insert("band".to_owned(), vec![0, 1, 2]);
        Router::new(Resolver::new(names, 1))
    }

    fn msg(addr: &str, args: Vec<OscType>) -> OscMessage {
        OscMessage {
            addr: addr.to_owned(),
            args,
        }
    }

    fn route(addr: &str, args: Vec<OscType>) -> Vec<Action> {
        router().route(&msg(addr, args)).expect("should route")
    }

    #[test]
    fn a_fader_set_converts_db_and_one_based_channels() {
        assert_eq!(
            route("/ch/5/fader", vec![OscType::Float(-10.0)]),
            [Action::SetLevel {
                target: Target::Fader {
                    strip: Strip::Input,
                    index: 4
                },
                raw: -1000,
                source: Source::Direct,
            }]
        );
    }

    /// QLab writes `-10` as an int; it must reach the same value as `-10.0`.
    #[test]
    fn an_integer_level_is_treated_as_db_not_raw_units() {
        assert_eq!(
            route("/ch/1/fader", vec![OscType::Int(-10)]),
            route("/ch/1/fader", vec![OscType::Float(-10.0)])
        );
    }

    #[test]
    fn raw_units_are_available_on_a_separate_address() {
        assert_eq!(
            route("/ch/1/fader/raw", vec![OscType::Int(-1000)]),
            [Action::SetLevel {
                target: Target::Fader {
                    strip: Strip::Input,
                    index: 0
                },
                raw: -1000,
                source: Source::Direct,
            }]
        );
    }

    #[test]
    fn negative_infinity_reaches_the_console_sentinel() {
        let Action::SetLevel { raw, .. } =
            route("/ch/1/fader", vec![OscType::String("-inf".into())])[0].clone()
        else {
            panic!("expected a level");
        };
        assert_eq!(raw, yamaha_rcp::NEG_INF);
    }

    #[test]
    fn a_name_addresses_its_channel() {
        assert_eq!(
            route("/ch/lead_vox/fader", vec![OscType::Int(0)]),
            route("/ch/5/fader", vec![OscType::Int(0)])
        );
    }

    #[test]
    fn a_group_name_fans_out_to_every_channel_in_it() {
        let actions = route("/ch/band/on", vec![OscType::Int(0)]);
        assert_eq!(actions.len(), 3);
        assert!(actions
            .iter()
            .all(|a| matches!(a, Action::SetSwitch { on: false, .. })));
    }

    #[test]
    fn patterns_fan_out_too() {
        assert_eq!(route("/ch/{1,3}/on", vec![OscType::Int(1)]).len(), 2);
        assert_eq!(route("/dca/*/on", vec![OscType::Int(1)]).len(), 8);
    }

    /// `/on` and `/mute` are opposites, and the console's own parameter is `Fader/On`.
    #[test]
    fn mute_is_the_inverse_of_on() {
        let muted = route("/ch/1/mute", vec![OscType::Int(1)]);
        let off = route("/ch/1/on", vec![OscType::Int(0)]);
        assert_eq!(muted, off);
    }

    #[test]
    fn every_strip_word_maps_to_a_strip() {
        for (word, strip) in [
            ("ch", Strip::Input),
            ("stin", Strip::StereoInput),
            ("fxrtn", Strip::FxReturn),
            ("dca", Strip::Dca),
            ("mix", Strip::Mix),
            ("mtrx", Strip::Matrix),
            ("st", Strip::Stereo),
            ("mono", Strip::Mono),
        ] {
            let actions = route(&format!("/{word}/1/on"), vec![OscType::Int(1)]);
            assert_eq!(
                actions,
                [Action::SetSwitch {
                    switch: Switch::Strip { strip, index: 0 },
                    on: true
                }],
                "{word}"
            );
        }
    }

    #[test]
    fn fades_carry_their_duration_and_curve() {
        let actions = route(
            "/ch/1/fade",
            vec![
                OscType::Int(-40),
                OscType::Int(3000),
                OscType::String("cosine".into()),
            ],
        );
        assert_eq!(
            actions,
            [Action::StartFade(FadeSpec {
                target: Target::Fader {
                    strip: Strip::Input,
                    index: 0
                },
                from: None,
                to: -4000,
                duration: Duration::from_secs(3),
                curve: Curve::Cosine,
            })]
        );
    }

    #[test]
    fn a_fade_can_be_given_an_explicit_start_level() {
        let actions = route(
            "/ch/1/fade",
            vec![OscType::Int(-40), OscType::Int(1000), OscType::Int(0)],
        );
        let Action::StartFade(spec) = &actions[0] else {
            panic!("expected a fade");
        };
        assert_eq!(spec.from, Some(0));
    }

    #[test]
    fn a_fade_defaults_to_a_linear_curve() {
        let actions = route("/ch/1/fade", vec![OscType::Int(-40), OscType::Int(1000)]);
        let Action::StartFade(spec) = &actions[0] else {
            panic!("expected a fade");
        };
        assert_eq!(spec.curve, Curve::Linear);
        assert_eq!(spec.from, None);
    }

    #[test]
    fn sends_require_the_right_bus_word() {
        let actions = route("/ch/1/send/mix/3/fader", vec![OscType::Int(-12)]);
        assert_eq!(
            actions,
            [Action::SetLevel {
                target: Target::Send {
                    strip: Strip::Input,
                    index: 0,
                    dest: 2
                },
                raw: -1200,
                source: Source::Direct,
            }]
        );
        // An input strip sends to a mix, not a matrix.
        assert!(router()
            .route(&msg("/ch/1/send/mtrx/1/fader", vec![OscType::Int(0)]))
            .is_err());
        // An output strip sends to a matrix, not a mix.
        assert!(router()
            .route(&msg("/mix/1/send/mtrx/1/fader", vec![OscType::Int(0)]))
            .is_ok());
        assert!(router()
            .route(&msg("/mix/1/send/mix/1/fader", vec![OscType::Int(0)]))
            .is_err());
    }

    #[test]
    fn strips_without_sends_are_rejected_clearly() {
        let err = router()
            .route(&msg("/dca/1/send/mix/1/fader", vec![OscType::Int(0)]))
            .unwrap_err();
        assert!(err.to_string().contains("no sends"), "{err}");
    }

    #[test]
    fn mute_groups_are_addressable() {
        assert_eq!(
            route("/mutegroup/3", vec![OscType::Int(1)]),
            [Action::SetSwitch {
                switch: Switch::MuteMaster { group: 2 },
                on: true
            }]
        );
        // The console has six.
        assert!(router()
            .route(&msg("/mutegroup/7", vec![OscType::Int(1)]))
            .is_err());
    }

    #[test]
    fn scene_recall_takes_a_bank_and_a_number() {
        assert_eq!(
            route(
                "/scene/recall",
                vec![OscType::String("a".into()), OscType::Int(12)]
            ),
            [Action::RecallScene {
                list: SceneList::A,
                scene: 12
            }]
        );
    }

    #[test]
    fn a_fractional_scene_number_is_rejected_rather_than_truncated() {
        assert!(router()
            .route(&msg(
                "/scene/recall",
                vec![OscType::String("a".into()), OscType::Float(12.7)]
            ))
            .is_err());
    }

    #[test]
    fn labels_and_colours_route() {
        assert_eq!(
            route("/ch/1/name", vec![OscType::String("Lead Vox".into())]),
            [Action::SetLabel {
                strip: Strip::Input,
                index: 0,
                name: "Lead Vox".to_owned()
            }]
        );
        assert_eq!(
            route("/ch/1/color", vec![OscType::String("Blue".into())]),
            [Action::SetColor {
                strip: Strip::Input,
                index: 0,
                color: LabelColor::Blue
            }]
        );
        // Both spellings, because half the world writes it the other way.
        assert_eq!(
            route("/ch/1/colour", vec![OscType::String("blue".into())]),
            route("/ch/1/color", vec![OscType::String("Blue".into())])
        );
    }

    #[test]
    fn bridge_control_addresses() {
        assert_eq!(route("/bridge/panic", vec![]), [Action::Panic]);
        assert_eq!(route("/bridge/reload", vec![]), [Action::Reload]);
    }

    #[test]
    fn a_trailing_slash_is_tolerated() {
        assert_eq!(
            route("/ch/1/fader/", vec![OscType::Int(0)]),
            route("/ch/1/fader", vec![OscType::Int(0)])
        );
    }

    #[test]
    fn unknown_addresses_are_reported_with_the_address() {
        let err = router()
            .route(&msg("/ch/1/gain", vec![OscType::Int(0)]))
            .unwrap_err();
        assert!(err.to_string().contains("/ch/1/gain"), "{err}");

        let err = router()
            .route(&msg("/nonsense/1/fader", vec![OscType::Int(0)]))
            .unwrap_err();
        assert!(err.to_string().contains("/nonsense/1/fader"), "{err}");
    }

    #[test]
    fn a_missing_argument_says_what_was_expected() {
        let err = router().route(&msg("/ch/1/fader", vec![])).unwrap_err();
        assert!(err.to_string().contains("dB"), "{err}");
    }

    #[test]
    fn only_level_moves_are_coalescable() {
        let level = &route("/ch/1/fader", vec![OscType::Int(0)])[0];
        assert!(level.coalesce_key().is_some());

        for addr in ["/ch/1/on", "/mutegroup/1"] {
            let action = &route(addr, vec![OscType::Int(1)])[0];
            assert!(
                action.coalesce_key().is_none(),
                "{addr} must never be dropped"
            );
        }
        assert!(Action::Panic.coalesce_key().is_none());
        assert!(
            route("/ch/1/fade", vec![OscType::Int(0), OscType::Int(100)])[0]
                .coalesce_key()
                .is_none()
        );
    }

    #[test]
    fn curve_names_have_forgiving_spellings() {
        for name in ["linear", "lin", "LINEAR"] {
            assert_eq!(parse_curve(name), Some(Curve::Linear), "{name}");
        }
        for name in ["cosine", "cos", "s", "scurve"] {
            assert_eq!(parse_curve(name), Some(Curve::Cosine), "{name}");
        }
        assert_eq!(parse_curve("wobble"), None);
    }
}
