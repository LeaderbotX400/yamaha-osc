//! Coercing OSC argument types into the values the console wants.
//!
//! QLab types arguments by how they are written in the cue: `-40` is an int, `-40.0` is a float,
//! `"off"` is a string, and `\T` / `\F` are booleans. An operator should not have to remember
//! which one a given address wants, so every accessor here accepts any reasonable spelling.

use rosc::OscType;

/// Read an argument as a number, accepting every numeric OSC type plus a numeric string.
///
/// `\I` (impulse) becomes positive infinity and the string `"-inf"` becomes negative infinity,
/// which is how a fader reaches the console's -∞ position.
pub fn as_f32(arg: &OscType) -> Option<f32> {
    match arg {
        OscType::Int(v) => Some(*v as f32),
        OscType::Long(v) => Some(*v as f32),
        OscType::Float(v) => Some(*v),
        OscType::Double(v) => Some(*v as f32),
        OscType::Inf => Some(f32::INFINITY),
        // Rust's float parser already accepts "inf", "-inf" and "infinity".
        OscType::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Read an argument as a whole number, rejecting a fractional float rather than truncating it.
///
/// Truncating would turn a mistyped `/scene/recall a 12.7` into a silent recall of scene 12, which
/// during a show is worse than a clear error.
pub fn as_u32(arg: &OscType) -> Option<u32> {
    let value = match arg {
        OscType::Int(v) => *v as f64,
        OscType::Long(v) => *v as f64,
        OscType::Float(v) => *v as f64,
        OscType::Double(v) => *v,
        OscType::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    if value.fract() != 0.0 || value < 0.0 || value > f64::from(u32::MAX) {
        return None;
    }
    Some(value as u32)
}

/// Read an argument as a boolean.
///
/// Accepts `\T`/`\F`, any number (zero is false), and the spellings an operator is likely to type.
pub fn as_bool(arg: &OscType) -> Option<bool> {
    match arg {
        OscType::Bool(v) => Some(*v),
        OscType::Int(v) => Some(*v != 0),
        OscType::Long(v) => Some(*v != 0),
        OscType::Float(v) => Some(*v != 0.0),
        OscType::Double(v) => Some(*v != 0.0),
        OscType::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "on" | "yes" | "in" => Some(true),
            "0" | "false" | "off" | "no" | "out" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Read an argument as text. Numbers are accepted so `/ch/1/name 42` works.
pub fn as_string(arg: &OscType) -> Option<String> {
    match arg {
        OscType::String(s) => Some(s.clone()),
        OscType::Char(c) => Some(c.to_string()),
        OscType::Int(v) => Some(v.to_string()),
        OscType::Long(v) => Some(v.to_string()),
        OscType::Float(v) => Some(v.to_string()),
        OscType::Double(v) => Some(v.to_string()),
        OscType::Bool(v) => Some(v.to_string()),
        _ => None,
    }
}

/// A short name for an argument's type, for error messages.
pub fn type_name(arg: &OscType) -> &'static str {
    match arg {
        OscType::Int(_) => "int",
        OscType::Long(_) => "long",
        OscType::Float(_) => "float",
        OscType::Double(_) => "double",
        OscType::String(_) => "string",
        OscType::Bool(_) => "bool",
        OscType::Char(_) => "char",
        OscType::Blob(_) => "blob",
        OscType::Time(_) => "time",
        OscType::Color(_) => "color",
        OscType::Midi(_) => "midi",
        OscType::Array(_) => "array",
        OscType::Nil => "nil",
        OscType::Inf => "impulse",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QLab writes `-40` as an int and `-40.0` as a float; both must reach the same fader value.
    #[test]
    fn numbers_are_accepted_however_qlab_types_them() {
        assert_eq!(as_f32(&OscType::Int(-40)), Some(-40.0));
        assert_eq!(as_f32(&OscType::Float(-40.0)), Some(-40.0));
        assert_eq!(as_f32(&OscType::Double(-40.0)), Some(-40.0));
        assert_eq!(as_f32(&OscType::Long(-40)), Some(-40.0));
        assert_eq!(as_f32(&OscType::String("-40.5".into())), Some(-40.5));
    }

    #[test]
    fn negative_infinity_can_be_written_as_a_string() {
        assert_eq!(
            as_f32(&OscType::String("-inf".into())),
            Some(f32::NEG_INFINITY)
        );
        assert_eq!(
            as_f32(&OscType::String("-infinity".into())),
            Some(f32::NEG_INFINITY)
        );
    }

    #[test]
    fn impulse_reads_as_positive_infinity() {
        assert_eq!(as_f32(&OscType::Inf), Some(f32::INFINITY));
    }

    #[test]
    fn non_numeric_arguments_are_rejected() {
        assert_eq!(as_f32(&OscType::String("loud".into())), None);
        assert_eq!(as_f32(&OscType::Nil), None);
    }

    #[test]
    fn booleans_accept_every_reasonable_spelling() {
        for t in [
            OscType::Bool(true),
            OscType::Int(1),
            OscType::Float(1.0),
            OscType::String("on".into()),
            OscType::String("TRUE".into()),
            OscType::String("yes".into()),
        ] {
            assert_eq!(as_bool(&t), Some(true), "{t:?}");
        }
        for f in [
            OscType::Bool(false),
            OscType::Int(0),
            OscType::Float(0.0),
            OscType::String("off".into()),
            OscType::String("false".into()),
        ] {
            assert_eq!(as_bool(&f), Some(false), "{f:?}");
        }
        assert_eq!(as_bool(&OscType::String("maybe".into())), None);
        assert_eq!(as_bool(&OscType::Nil), None);
    }

    /// A mistyped scene number must fail loudly rather than being truncated into a real scene.
    #[test]
    fn fractional_values_are_rejected_where_a_whole_number_is_required() {
        assert_eq!(as_u32(&OscType::Float(12.7)), None);
        assert_eq!(as_u32(&OscType::Float(12.0)), Some(12));
        assert_eq!(as_u32(&OscType::Int(12)), Some(12));
        assert_eq!(as_u32(&OscType::Int(-1)), None);
        assert_eq!(as_u32(&OscType::String("12".into())), Some(12));
    }

    #[test]
    fn strings_accept_numbers_so_a_numeric_label_works() {
        assert_eq!(as_string(&OscType::Int(42)), Some("42".to_owned()));
        assert_eq!(
            as_string(&OscType::String("Lead Vox".into())),
            Some("Lead Vox".to_owned())
        );
        assert_eq!(as_string(&OscType::Nil), None);
    }
}
