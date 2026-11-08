//! The outputs of a skin's callbacks that run outside the expressions: on
//! the selection and result screens, the bounded runtime's (docs/stage3.md
//! S7-07). Each callback `k` writes two field slots after the schema's
//! fields, its value `lua.<k>` and its availability `lua.<k>.ok`; an
//! expression reads them by those names ([`crate::expr::Scope::outputs`]).
//!
//! The play presentation program (S7-06d) writes the same slots by the
//! same names. This module is their shared home: the program's branch
//! adds its own copy of [`OutTy`] and [`output`] until the two meet, and
//! the second to land keeps this one.

/// A callback output's type: a `draw` gives a boolean, a number's `value`
/// an integer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutTy {
    Bool,
    Int,
}

/// The slot, type and whether it is the `ok` flag of output `name` after
/// `base` schema fields, for the outputs `signature`: `lua.<k>` and
/// `lua.<k>.ok`, `k` a decimal index with no leading zero; `None` for
/// any other name.
#[must_use]
pub fn output(base: usize, signature: &[OutTy], name: &str) -> Option<(u16, OutTy, bool)> {
    let rest = name.strip_prefix("lua.")?;
    let (k, ok) = match rest.strip_suffix(".ok") {
        Some(k) => (k, true),
        None => (rest, false),
    };
    if k.is_empty() || !k.bytes().all(|b| b.is_ascii_digit()) || (k.len() > 1 && k.starts_with('0'))
    {
        return None;
    }
    let k: usize = k.parse().ok()?;
    let ty = *signature.get(k)?;
    let slot = base
        .checked_add(k.checked_mul(2)?)?
        .checked_add(usize::from(ok))?;
    Some((u16::try_from(slot).ok()?, ty, ok))
}

/// The field slots `n` outputs take after the schema's fields.
#[must_use]
pub const fn slots(n: usize) -> usize {
    n.saturating_mul(2)
}

#[cfg(test)]
mod tests {
    use super::{OutTy, output};

    #[test]
    fn names_resolve_after_the_fields() {
        let sig = [OutTy::Bool, OutTy::Int];
        assert_eq!(output(10, &sig, "lua.0"), Some((10, OutTy::Bool, false)));
        assert_eq!(output(10, &sig, "lua.0.ok"), Some((11, OutTy::Bool, true)));
        assert_eq!(output(10, &sig, "lua.1"), Some((12, OutTy::Int, false)));
        for bad in ["lua.2", "lua.01", "lua.", "lua.x", "lua.1.okk", "x.0"] {
            assert_eq!(output(10, &sig, bad), None, "{bad}");
        }
    }
}
