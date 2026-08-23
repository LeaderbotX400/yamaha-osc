//! The console's parameter table: which paths exist, their index ranges, types and scaling.
//!
//! Yamaha publish no protocol specification, but the consoles describe themselves: a TF replies to
//! an info query with one `prminfo` line per parameter. A captured TF table is embedded here
//! (`params/tf.txt`) and parsed at first use, so the crate can validate a command *before* putting
//! it on the wire instead of discovering the problem from an `ERROR` reply mid-cue.
//!
//! Line format (whitespace separated, quoted strings may contain spaces):
//!
//! ```text
//! OK prminfo 0 "MIXER:Current/InCh/Fader/Level" 40 0 -32768 1000 -32768 "dB" integer any rw 100
//! ^  ^       ^ ^                                ^  ^ ^      ^    ^      ^    ^       ^   ^  ^
//! |  kind    | path                             |  | min    max  default unit type   |   |  scale
//! status     id                                 |  sub-count                         |   rw
//!                                               instance count                       access
//! ```
//!
//! A leading `--` instead of `OK` marks a parameter the queried console does not have.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::wire::tokenize;
use crate::Error;

/// The captured TF-series parameter table.
const TF_TABLE: &str = include_str!("../params/tf.txt");

/// The console's value for negative infinity on a dB parameter. Not `min - 1`, not a small number:
/// the console uses this exact sentinel, and a fader set to it reads as "-∞" on the surface.
pub const NEG_INF: i32 = -32768;

/// What kind of value a parameter carries on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    /// A bare integer. dB parameters are integers scaled by [`ParamInfo::scale`].
    Integer,
    /// A double-quoted string.
    Str,
    /// An integer enumeration the protocol notes call "binary".
    Binary,
    /// Metering data, delivered as a packed hex blob.
    Meter,
    /// No value (a bare command such as scene increment).
    None,
}

impl ValueKind {
    fn parse(s: &str) -> Self {
        match s {
            "integer" => Self::Integer,
            "string" => Self::Str,
            "binary" => Self::Binary,
            "mtr" => Self::Meter,
            _ => Self::None,
        }
    }
}

/// One row of the console's parameter table.
#[derive(Clone, Debug)]
pub struct ParamInfo {
    /// Full RCP path, e.g. `MIXER:Current/InCh/Fader/Level`.
    pub path: String,
    /// Number of instances — channels, DCAs, buses. `0` means the parameter is a singleton.
    pub count: u32,
    /// Number of sub-indices — e.g. the 20 mix destinations of `InCh/ToMix/Level`.
    /// `0` means the only valid sub-index is `0`.
    pub sub_count: u32,
    /// Lowest value the console accepts. For dB parameters this is [`NEG_INF`].
    pub min: i32,
    /// Highest value the console accepts. For string parameters this is the maximum length.
    pub max: i32,
    /// Unit, e.g. `dB`. Empty for unitless parameters.
    pub unit: String,
    /// Wire representation of the value.
    pub kind: ValueKind,
    /// Whether the console accepts `set` for this path.
    pub writable: bool,
    /// Whether the console accepts `get` for this path.
    pub readable: bool,
    /// Fixed-point scale: `100` for dB (so `-1000` is -10.00 dB), `1` for everything else.
    pub scale: i32,
}

impl ParamInfo {
    /// Number of valid instance indices (`0..instances()`).
    pub fn instances(&self) -> u32 {
        self.count.max(1)
    }

    /// Number of valid sub-indices (`0..subs()`).
    pub fn subs(&self) -> u32 {
        self.sub_count.max(1)
    }

    /// Whether this parameter is expressed in decibels.
    pub fn is_db(&self) -> bool {
        self.unit == "dB"
    }

    /// Check that `index` and `sub` address a real instance of this parameter.
    pub fn check_indices(&self, index: u32, sub: u32) -> Result<(), Error> {
        if index >= self.instances() {
            return Err(Error::IndexOutOfRange {
                path: self.path.clone(),
                what: "instance",
                index,
                count: self.instances(),
            });
        }
        if sub >= self.subs() {
            return Err(Error::IndexOutOfRange {
                path: self.path.clone(),
                what: "sub",
                index: sub,
                count: self.subs(),
            });
        }
        Ok(())
    }

    /// Clamp an integer into the console's accepted range.
    ///
    /// Sending an out-of-range value earns an `ERROR` reply and no change, which during a show is
    /// strictly worse than the nearest legal value.
    pub fn clamp(&self, value: i32) -> i32 {
        value.clamp(self.min, self.max)
    }

    /// Convert decibels to the console's scaled integer, mapping anything at or below the minimum
    /// to the [`NEG_INF`] sentinel.
    ///
    /// Rounds half away from zero so a UI value of -0.005 dB does not silently become +0.00.
    pub fn db_to_raw(&self, db: f32) -> i32 {
        if !db.is_finite() {
            return if db.is_sign_negative() { NEG_INF } else { self.max };
        }
        let scaled = (db * self.scale as f32).round();
        if scaled <= self.min as f32 {
            return self.min;
        }
        self.clamp(scaled as i32)
    }

    /// Convert the console's scaled integer back to decibels. [`NEG_INF`] becomes `f32::NEG_INFINITY`.
    pub fn raw_to_db(&self, raw: i32) -> f32 {
        if raw == NEG_INF {
            return f32::NEG_INFINITY;
        }
        raw as f32 / self.scale as f32
    }
}

/// A console's full parameter table, indexed by path.
#[derive(Debug)]
pub struct ParamTable {
    by_path: HashMap<String, ParamInfo>,
}

impl ParamTable {
    /// The embedded TF-series table.
    ///
    /// Note that the capture is from a console reporting 40 input channels; a TF1 has 32. Indices
    /// past a specific console's real channel count pass validation here and are rejected by the
    /// console itself, which is the safer direction to be wrong in.
    pub fn tf() -> &'static ParamTable {
        static TF: OnceLock<ParamTable> = OnceLock::new();
        TF.get_or_init(|| {
            ParamTable::parse(TF_TABLE).expect("embedded TF parameter table must parse")
        })
    }

    /// Parse a captured table. Lines the console marked unavailable (`--`) are skipped, as are
    /// blank lines and anything too short to be a table row.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let mut by_path = HashMap::new();

        for line in text.lines() {
            let t = tokenize(line);
            if t.len() < 14 || t[0] != "OK" {
                continue;
            }

            let num = |i: usize| -> i32 { t[i].parse().unwrap_or(0) };
            let path = t[3].clone();
            let rw = &t[12];

            by_path.insert(
                path.clone(),
                ParamInfo {
                    path,
                    count: num(4).max(0) as u32,
                    sub_count: num(5).max(0) as u32,
                    min: num(6),
                    max: num(7),
                    unit: t[9].clone(),
                    kind: ValueKind::parse(&t[10]),
                    writable: rw.contains('w'),
                    readable: rw.contains('r'),
                    scale: if num(13) == 0 { 1 } else { num(13) },
                },
            );
        }

        Ok(Self { by_path })
    }

    /// Look up a parameter, or fail with [`Error::UnknownParam`].
    pub fn get(&self, path: &str) -> Result<&ParamInfo, Error> {
        self.by_path
            .get(path)
            .ok_or_else(|| Error::UnknownParam(path.to_owned()))
    }

    /// Look up a parameter and check it is writable and that the indices are in range.
    pub fn check_set(&self, path: &str, index: u32, sub: u32) -> Result<&ParamInfo, Error> {
        let info = self.get(path)?;
        if !info.writable {
            return Err(Error::ReadOnly(info.path.clone()));
        }
        info.check_indices(index, sub)?;
        Ok(info)
    }

    /// Look up a parameter and check it is readable and that the indices are in range.
    pub fn check_get(&self, path: &str, index: u32, sub: u32) -> Result<&ParamInfo, Error> {
        let info = self.get(path)?;
        info.check_indices(index, sub)?;
        Ok(info)
    }

    /// Every path in the table, unordered. Used by the bridge's `dump-params` output.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.by_path.keys().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_tf_table_parses() {
        let t = ParamTable::tf();
        assert!(t.paths().count() > 90, "table looks truncated");
    }

    #[test]
    fn input_fader_matches_the_captured_table() {
        let p = ParamTable::tf().get("MIXER:Current/InCh/Fader/Level").unwrap();
        assert_eq!(p.instances(), 40);
        assert_eq!(p.subs(), 1, "Fader/Level has no sub-index");
        assert_eq!(p.min, NEG_INF);
        assert_eq!(p.max, 1000);
        assert_eq!(p.scale, 100);
        assert!(p.is_db());
        assert!(p.writable && p.readable);
    }

    #[test]
    fn sends_have_a_real_sub_index_range() {
        let p = ParamTable::tf().get("MIXER:Current/InCh/ToMix/Level").unwrap();
        assert_eq!(p.instances(), 40);
        assert_eq!(p.subs(), 20);
        assert!(p.check_indices(39, 19).is_ok());
        assert!(p.check_indices(39, 20).is_err());
    }

    #[test]
    fn unavailable_rows_are_skipped() {
        // "-- prminfo 49 MIXER:Current/DcaCh/Fader/Level" is marked absent on this console.
        let t = ParamTable::tf();
        assert!(t.get("MIXER:Current/DCA/Fader/Level").is_ok());
        assert!(t.get("MIXER:Current/DcaCh/Fader/Level").is_err());
    }

    #[test]
    fn dca_and_mute_master_counts() {
        let t = ParamTable::tf();
        assert_eq!(t.get("MIXER:Current/DCA/Fader/Level").unwrap().instances(), 8);
        assert_eq!(t.get("MIXER:Current/MuteMaster/On").unwrap().instances(), 6);
        assert_eq!(t.get("MIXER:Current/Mix/Fader/Level").unwrap().instances(), 20);
        assert_eq!(t.get("MIXER:Current/St/Fader/Level").unwrap().instances(), 2);
        assert_eq!(t.get("MIXER:Current/Mono/Fader/Level").unwrap().instances(), 1);
    }

    #[test]
    fn read_only_parameters_are_rejected_for_set() {
        let t = ParamTable::tf();
        // Role is "r" in the table.
        assert!(matches!(
            t.check_set("MIXER:Current/InCh/Role", 0, 0),
            Err(Error::ReadOnly(_))
        ));
    }

    #[test]
    fn out_of_range_channel_is_caught_before_the_wire() {
        let t = ParamTable::tf();
        assert!(t.check_set("MIXER:Current/DCA/Fader/Level", 8, 0).is_err());
        assert!(t.check_set("MIXER:Current/DCA/Fader/Level", 7, 0).is_ok());
    }

    #[test]
    fn db_round_trips_through_the_scale() {
        let p = ParamTable::tf().get("MIXER:Current/InCh/Fader/Level").unwrap();
        assert_eq!(p.db_to_raw(-10.0), -1000);
        assert_eq!(p.db_to_raw(0.0), 0);
        assert_eq!(p.db_to_raw(10.0), 1000);
        assert_eq!(p.raw_to_db(-1000), -10.0);
    }

    #[test]
    fn db_clamps_to_the_consoles_range_rather_than_erroring() {
        let p = ParamTable::tf().get("MIXER:Current/InCh/Fader/Level").unwrap();
        assert_eq!(p.db_to_raw(99.0), 1000, "above max clamps to max");
        assert_eq!(p.db_to_raw(-500.0), NEG_INF, "below min becomes -inf");
    }

    #[test]
    fn negative_infinity_is_the_sentinel_both_ways() {
        let p = ParamTable::tf().get("MIXER:Current/InCh/Fader/Level").unwrap();
        assert_eq!(p.db_to_raw(f32::NEG_INFINITY), NEG_INF);
        assert_eq!(p.raw_to_db(NEG_INF), f32::NEG_INFINITY);
    }

    #[test]
    fn db_rounds_rather_than_truncating() {
        let p = ParamTable::tf().get("MIXER:Current/InCh/Fader/Level").unwrap();
        // 0.005 dB * 100 = 0.5 -> 1, not 0. Truncation would lose the last step of a fade.
        assert_eq!(p.db_to_raw(-3.456), -346);
        assert_eq!(p.db_to_raw(-3.454), -345);
    }
}
