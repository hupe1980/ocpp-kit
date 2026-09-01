//! [`Decimal`], the exact base-10 number every OCPP quantity is carried in.
//!
//! Re-exported as [`crate::types::Decimal`]; see that type for what it is and why it is not
//! an `f64`.

// Fixed-point arithmetic is width juggling on purpose: a mantissa moves between `i64`,
// `i128` and `u128` so that an intermediate cannot overflow, and every one of those moves is
// bounded by the check on the line above it. Spelling that out as one `#[allow]` per cast
// buries the checks in attributes without making any of them safer.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use core::cmp::Ordering;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::ops::{Add, AddAssign, Div, Mul, Neg, Sub, SubAssign};
use core::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::validate::{Validate, ValidationPath, Violations};

/// An exact base-10 decimal: `mantissa × 10⁻ˢᶜᵃˡᵉ`.
///
/// This is what every OCPP `number` is carried in. The schemas say `"type": "number"` and the
/// obvious reading of that is `f64`; it is the wrong one, for three reasons that all cost
/// money:
///
/// * **Resolution is a claim.** A meter reporting `2935.600` kWh is stating three decimals of
///   accuracy. As an `f64`, `2935.600` and `2935.6` are the same value and the claim is gone.
///   A `Decimal` keeps the scale the station sent, so the value decodes, prints and
///   re-encodes as `2935.600`. OCMF says the same of its own readings: the representation
///   "must not be transformed … thus potentially the number of valid digits" (Tab. 7, `RV`).
/// * **Energy is a subtraction.** OCPP *defines* a session's energy as the difference of two
///   register readings, and in `f64` `10.1 - 0.1` is `10.000000000000002`.
/// * **The 2.1 Tariff and Cost block is money.**
///
/// ```
/// use ocpp_kit::decimal;
/// use ocpp_kit::types::Decimal;
///
/// let register: Decimal = "2935.600".parse().unwrap();
/// assert_eq!(register.scale(), 3);
/// assert_eq!(register.to_string(), "2935.600");
///
/// // The difference OCPP defines a session's energy as, exactly.
/// assert_eq!(decimal!(20.2) - decimal!(10.1), decimal!(10.1));
/// ```
///
/// Literals are written with [`decimal!`](crate::decimal), which parses the source text at
/// compile time; integers convert with `From`. There is deliberately no `From<f64>` — a
/// number that has been through a float has already lost what this type exists to keep — and
/// the conversions that do exist are named [`to_f64_lossy`](Self::to_f64_lossy) and
/// [`from_f64_lossy`](Self::from_f64_lossy) so a signature says what it costs. Nothing else
/// in this crate calls either, and `cargo xtask no-floats` fails the build if an `f32` or
/// `f64` reaches any public signature.
///
/// # What exactly is preserved
///
/// The **fraction digits** — the resolution claim — and therefore the positional spelling:
/// `2935.600` in, `2935.600` out; `5` in, `5` out, not `5.0`. What is *not* preserved is a
/// spelling that carries no information a decimal number has: exponent notation is
/// normalized (`1.5e3` re-encodes as `1500`), as are a leading `+` and leading zeros. Where
/// the exact bytes are load-bearing — a signed OCMF or EDL record — they are not a JSON
/// number at all, and [`metering`](crate::metering) carries those untouched.
///
/// # Range
///
/// The mantissa is an `i64` and the scale is at most [`MAX_SCALE`](Self::MAX_SCALE) (18), so
/// a value carries up to 19 significant digits — a register in Wh with three decimals up to
/// 10¹⁵ Wh. Text with more precision than that is rounded half to even by
/// [`from_ascii`](Self::from_ascii); text whose *integer* part does not fit is refused.
///
/// # Equality
///
/// `PartialEq` and `Ord` compare *numerically*, so `2.50 == 2.5` — that is what a comparison
/// between two readings should answer, and `Hash` agrees with it. The scale is still there
/// and still round-trips; [`eq_exact`](Self::eq_exact) compares it too when the spelling is
/// what matters.
///
/// # Arithmetic
///
/// Addition, subtraction and multiplication are exact, at the scale the operands imply, and
/// the operators panic on overflow exactly as the integer ones do; the `checked_` forms
/// return `None` instead. Division has no exact answer in base ten, so
/// [`checked_div`](Self::checked_div) takes the scale to round at rather than guessing one.
/// A change of unit prefix is [`checked_pow10`](Self::checked_pow10), which moves the point
/// and so cannot round at all.
///
/// # Wire format
///
/// `Serialize` and `Deserialize` speak `serde_json`'s raw-value protocol, which is how the
/// source token reaches this type instead of the `f64` a JSON parser would otherwise round it
/// to. Any other data format falls back to the ordinary numeric path and is lossy in the way
/// that format is lossy. OCPP-J is JSON, so this is the format that matters.
///
/// # Handing the value to another decimal type
///
/// A billing platform generally has a decimal type of its own. The conversion is exact in
/// either direction, so this crate depends on none of them:
///
/// ```ignore
/// let exact = rust_decimal::Decimal::new(reading.mantissa(), u32::from(reading.scale()));
/// let back = ocpp_kit::types::Decimal::new(
///     i64::try_from(exact.mantissa()).unwrap(),
///     u8::try_from(exact.scale()).unwrap(),
/// );
/// ```
///
/// Anything that parses decimal text — `bigdecimal`, a database `NUMERIC` — takes
/// `to_string()`, which is the value at its own scale and never in exponent form.
#[derive(Clone, Copy, Default)]
pub struct Decimal {
    mantissa: i64,
    scale: u8,
}

/// Powers of ten that fit a `u64`, for rendering and rescaling.
const POW10_U64: [u64; 19] = [
    1,
    10,
    100,
    1_000,
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
    1_000_000_000,
    10_000_000_000,
    100_000_000_000,
    1_000_000_000_000,
    10_000_000_000_000,
    100_000_000_000_000,
    1_000_000_000_000_000,
    10_000_000_000_000_000,
    100_000_000_000_000_000,
    1_000_000_000_000_000_000,
];

/// `10ⁿ` as an `i128`, for comparison and arithmetic across scales.
const fn pow10_i128(exp: u32) -> i128 {
    let mut result: i128 = 1;
    let mut left = exp;
    while left > 0 {
        result *= 10;
        left -= 1;
    }
    result
}

/// Room to take one more digit without overflowing a `u128`.
const ACCUMULATE_LIMIT: u128 = u128::MAX / 16;

impl Decimal {
    /// Zero, at scale 0.
    pub const ZERO: Self = Self {
        mantissa: 0,
        scale: 0,
    };

    /// One, at scale 0.
    pub const ONE: Self = Self {
        mantissa: 1,
        scale: 0,
    };

    /// The largest [`scale`](Self::scale) a `Decimal` can carry.
    pub const MAX_SCALE: u8 = 18;

    /// `mantissa × 10⁻ˢᶜᵃˡᵉ`.
    ///
    /// # Panics
    ///
    /// If `scale` exceeds [`MAX_SCALE`](Self::MAX_SCALE). Use [`try_new`](Self::try_new) when
    /// the scale is not known at compile time.
    #[must_use]
    pub const fn new(mantissa: i64, scale: u8) -> Self {
        assert!(
            scale <= Self::MAX_SCALE,
            "a Decimal scale may not exceed Decimal::MAX_SCALE"
        );
        Self { mantissa, scale }
    }

    /// `mantissa × 10⁻ˢᶜᵃˡᵉ`, or `None` when `scale` exceeds [`MAX_SCALE`](Self::MAX_SCALE).
    #[must_use]
    pub const fn try_new(mantissa: i64, scale: u8) -> Option<Self> {
        if scale > Self::MAX_SCALE {
            return None;
        }
        Some(Self { mantissa, scale })
    }

    /// A whole number.
    #[must_use]
    pub const fn from_i64(value: i64) -> Self {
        Self {
            mantissa: value,
            scale: 0,
        }
    }

    /// The mantissa: the value with the decimal point removed.
    #[must_use]
    pub const fn mantissa(self) -> i64 {
        self.mantissa
    }

    /// How many digits follow the decimal point.
    ///
    /// This is the resolution the value was written with, and it is preserved: a station that
    /// sent `2935.600` gets `3` here, and `2935.600` back on the wire.
    #[must_use]
    pub const fn scale(self) -> u8 {
        self.scale
    }

    /// Whether the value is zero, whatever its scale.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.mantissa == 0
    }

    /// `-1`, `0` or `1`.
    #[must_use]
    pub const fn signum(self) -> i32 {
        if self.mantissa < 0 {
            -1
        } else if self.mantissa > 0 {
            1
        } else {
            0
        }
    }

    /// Whether two values have both the same number *and* the same scale — `2.50` and `2.5`
    /// are equal but not identical.
    #[must_use]
    pub const fn eq_exact(self, other: Self) -> bool {
        self.mantissa == other.mantissa && self.scale == other.scale
    }

    /// The same number with every trailing zero removed: `2.500` becomes `2.5`.
    #[must_use]
    pub const fn normalized(mut self) -> Self {
        while self.scale > 0 && self.mantissa % 10 == 0 {
            self.mantissa /= 10;
            self.scale -= 1;
        }
        if self.mantissa == 0 {
            self.scale = 0;
        }
        self
    }

    /// The same number written at `scale`, or `None` when that would lose a digit or
    /// overflow.
    ///
    /// Exact by construction: it never rounds. Use [`round_to`](Self::round_to) to round.
    #[must_use]
    pub const fn rescale(self, scale: u8) -> Option<Self> {
        if scale > Self::MAX_SCALE {
            return None;
        }
        if scale == self.scale {
            return Some(self);
        }
        if scale > self.scale {
            let factor = POW10_U64[(scale - self.scale) as usize];
            let Some(mantissa) = self.mantissa.checked_mul(factor as i64) else {
                return None;
            };
            return Some(Self { mantissa, scale });
        }
        let factor = POW10_U64[(self.scale - scale) as usize] as i64;
        if self.mantissa % factor != 0 {
            return None;
        }
        Some(Self {
            mantissa: self.mantissa / factor,
            scale,
        })
    }

    /// The value rounded to `scale`, half to even, or `None` on overflow or an out-of-range
    /// scale.
    #[must_use]
    pub const fn round_to(self, scale: u8) -> Option<Self> {
        if scale >= self.scale {
            return self.rescale(scale);
        }
        let drop = (self.scale - scale) as u32;
        let negative = self.mantissa < 0;
        let magnitude = self.mantissa.unsigned_abs() as u128;
        let rounded = round_off(magnitude, drop, false);
        if rounded > i64::MAX as u128 {
            return None;
        }
        let mantissa = rounded as i64;
        Some(Self {
            mantissa: if negative { -mantissa } else { mantissa },
            scale,
        })
    }

    // -- arithmetic ---------------------------------------------------------

    /// The sum, at the larger of the two scales, or `None` on overflow.
    #[must_use]
    pub const fn checked_add(self, rhs: Self) -> Option<Self> {
        let scale = if self.scale > rhs.scale {
            self.scale
        } else {
            rhs.scale
        };
        let (Some(a), Some(b)) = (self.to_i128_at(scale), rhs.to_i128_at(scale)) else {
            return None;
        };
        narrow(a + b, scale)
    }

    /// The difference, at the larger of the two scales, or `None` on overflow.
    ///
    /// This is the operation OCPP defines a session's energy as, and the reason this type
    /// exists: `20.2 - 10.1` is `10.1`, not `10.100000000000001`.
    #[must_use]
    pub const fn checked_sub(self, rhs: Self) -> Option<Self> {
        let scale = if self.scale > rhs.scale {
            self.scale
        } else {
            rhs.scale
        };
        let (Some(a), Some(b)) = (self.to_i128_at(scale), rhs.to_i128_at(scale)) else {
            return None;
        };
        narrow(a - b, scale)
    }

    /// The product, at the sum of the two scales, or `None` when that does not fit.
    #[must_use]
    pub const fn checked_mul(self, rhs: Self) -> Option<Self> {
        let scale = self.scale as u32 + rhs.scale as u32;
        let product = self.mantissa as i128 * rhs.mantissa as i128;
        if scale <= Self::MAX_SCALE as u32 {
            return narrow(product, scale as u8);
        }
        // The exact product needs more scale than the type carries; round the surplus away.
        let drop = scale - Self::MAX_SCALE as u32;
        let negative = product < 0;
        let rounded = round_off(product.unsigned_abs(), drop, false);
        if rounded > i64::MAX as u128 {
            return None;
        }
        let mantissa = rounded as i64;
        Some(Self {
            mantissa: if negative { -mantissa } else { mantissa },
            scale: Self::MAX_SCALE,
        })
    }

    /// The quotient at `scale`, rounded half to even, or `None` when `rhs` is zero or the
    /// result does not fit.
    ///
    /// Division is the one operation that has no exact answer in base ten — `1 / 3` does not
    /// terminate — so the scale to round at is a parameter rather than a guess.
    #[must_use]
    pub const fn checked_div(self, rhs: Self, scale: u8) -> Option<Self> {
        if rhs.mantissa == 0 || scale > Self::MAX_SCALE {
            return None;
        }
        // (a / 10^sa) / (b / 10^sb) at `scale` == a * 10^(scale + sb - sa) / b.
        let shift = scale as i32 + rhs.scale as i32 - self.scale as i32;
        let negative = (self.mantissa < 0) != (rhs.mantissa < 0);
        let mut numerator = self.mantissa.unsigned_abs() as u128;
        let denominator = rhs.mantissa.unsigned_abs() as u128;
        // One extra digit, so the quotient can be rounded half to even.
        let shift = shift + 1;
        let mut extra_drop = 0u32;
        if shift >= 0 {
            let up = shift as u32;
            if up > 38 {
                return None;
            }
            let factor = pow10_i128(up) as u128;
            let Some(scaled) = numerator.checked_mul(factor) else {
                return None;
            };
            numerator = scaled;
        } else {
            // Shrinking the numerator instead would throw away the digits the rounding needs,
            // so the denominator grows.
            let down = (-shift) as u32;
            if down > 38 {
                return None;
            }
            extra_drop = down;
        }
        let quotient = if extra_drop > 0 {
            let factor = pow10_i128(extra_drop) as u128;
            let Some(scaled) = denominator.checked_mul(factor) else {
                return None;
            };
            numerator / scaled
        } else {
            numerator / denominator
        };
        let remainder_is_zero = if extra_drop > 0 {
            let factor = pow10_i128(extra_drop) as u128;
            match denominator.checked_mul(factor) {
                Some(scaled) => numerator % scaled == 0,
                None => return None,
            }
        } else {
            numerator % denominator == 0
        };
        let rounded = round_off(quotient, 1, !remainder_is_zero);
        if rounded > i64::MAX as u128 {
            return None;
        }
        let mantissa = rounded as i64;
        Some(Self {
            mantissa: if negative { -mantissa } else { mantissa },
            scale,
        })
    }

    /// The value multiplied by `10^exp`, exactly, or `None` when it does not fit.
    ///
    /// A shift of the decimal point, which is all a change of unit prefix is: kWh to Wh is
    /// `pow10(3)`, and the 2.x `unitOfMeasure.multiplier` is exactly this operation. Unlike a
    /// multiplication by a literal `1000.0` it cannot introduce a rounding error, because it
    /// only moves the point.
    #[must_use]
    pub const fn checked_pow10(self, exp: i32) -> Option<Self> {
        if exp == 0 {
            return Some(self);
        }
        // Nothing outside this range has an answer, and `exp` comes off the wire — the 2.x
        // `unitOfMeasure.multiplier` is whatever the station put there, `i32::MIN` included,
        // and negating that is an overflow rather than a large number.
        if exp > 40 || exp < -40 {
            return if self.mantissa == 0 {
                Some(Self::ZERO)
            } else {
                None
            };
        }
        if exp < 0 {
            // Dividing by a power of ten: add scale, which is exact until the cap is reached.
            let want = self.scale as i32 - exp;
            if want <= Self::MAX_SCALE as i32 {
                return Some(Self {
                    mantissa: self.mantissa,
                    scale: want as u8,
                });
            }
            // Past the cap the mantissa has to shrink instead, which needs trailing zeros.
            let surplus = want - Self::MAX_SCALE as i32;
            let surplus = surplus as u32;
            if surplus > 18 {
                return None;
            }
            let factor = POW10_U64[surplus as usize] as i64;
            if self.mantissa % factor != 0 {
                return None;
            }
            return Some(Self {
                mantissa: self.mantissa / factor,
                scale: Self::MAX_SCALE,
            });
        }
        // Multiplying by a power of ten: spend the scale first, then grow the mantissa.
        let mut left = exp as u32;
        let mut mantissa = self.mantissa;
        let mut scale = self.scale;
        while left > 0 && scale > 0 {
            scale -= 1;
            left -= 1;
        }
        while left > 0 {
            let step = if left > 18 { 18 } else { left };
            let Some(next) = mantissa.checked_mul(POW10_U64[step as usize] as i64) else {
                return None;
            };
            mantissa = next;
            left -= step;
        }
        Some(Self { mantissa, scale })
    }

    /// The absolute value, or `None` for a mantissa of `i64::MIN`.
    #[must_use]
    pub const fn checked_abs(self) -> Option<Self> {
        let Some(mantissa) = self.mantissa.checked_abs() else {
            return None;
        };
        Some(Self {
            mantissa,
            scale: self.scale,
        })
    }

    /// The negation, or `None` for a mantissa of `i64::MIN`.
    #[must_use]
    pub const fn checked_neg(self) -> Option<Self> {
        let Some(mantissa) = self.mantissa.checked_neg() else {
            return None;
        };
        Some(Self {
            mantissa,
            scale: self.scale,
        })
    }

    /// The smaller of two values.
    #[must_use]
    pub const fn min(self, other: Self) -> Self {
        match self.compare(other) {
            Ordering::Greater => other,
            _ => self,
        }
    }

    /// The larger of two values.
    #[must_use]
    pub const fn max(self, other: Self) -> Self {
        match self.compare(other) {
            Ordering::Less => other,
            _ => self,
        }
    }

    /// Numeric comparison, in a `const` context.
    #[must_use]
    pub const fn compare(self, other: Self) -> Ordering {
        let scale = if self.scale > other.scale {
            self.scale
        } else {
            other.scale
        };
        match (self.to_i128_at(scale), other.to_i128_at(scale)) {
            (Some(a), Some(b)) => {
                if a < b {
                    Ordering::Less
                } else if a > b {
                    Ordering::Greater
                } else {
                    Ordering::Equal
                }
            }
            // Unreachable: 19 digits shifted by at most 18 places fits an i128 with room to
            // spare. Falling back on the sign keeps the function total anyway.
            _ => {
                if self.mantissa < other.mantissa {
                    Ordering::Less
                } else if self.mantissa > other.mantissa {
                    Ordering::Greater
                } else {
                    Ordering::Equal
                }
            }
        }
    }

    /// The mantissa this value would have at `scale`, as an `i128`.
    const fn to_i128_at(self, scale: u8) -> Option<i128> {
        if scale < self.scale {
            return None;
        }
        let factor = pow10_i128((scale - self.scale) as u32);
        Some(self.mantissa as i128 * factor)
    }

    // -- text ---------------------------------------------------------------

    /// Parses a decimal from ASCII, exactly.
    ///
    /// Accepts everything JSON's `number` production allows — an optional sign, digits, an
    /// optional fraction, an optional `e`/`E` exponent — plus a leading `+`, leading zeros
    /// and surrounding whitespace, none of which JSON permits but stations send anyway. The
    /// scale of the result is the number of fraction digits written, so `2935.600` parses to
    /// scale 3 and prints back identically.
    ///
    /// # Errors
    ///
    /// [`ParseDecimalError::Invalid`] when the text is not a number at all, and
    /// [`ParseDecimalError::Overflow`] when its integer part needs more than 19 digits.
    /// Excess *fraction* digits are rounded half to even rather than refused, because
    /// refusing a value the schema calls valid is worse than rounding at the nineteenth
    /// significant digit.
    // One parser, in one place: splitting it would hand the pieces a half-parsed state to
    // agree about, which is how parsers grow disagreements.
    #[allow(clippy::too_many_lines)]
    pub const fn from_ascii(bytes: &[u8]) -> Result<Self, ParseDecimalError> {
        let n = bytes.len();
        let mut i = 0usize;
        while i < n && is_space(bytes[i]) {
            i += 1;
        }
        let mut negative = false;
        if i < n && (bytes[i] == b'+' || bytes[i] == b'-') {
            negative = bytes[i] == b'-';
            i += 1;
        }

        let mut magnitude: u128 = 0;
        let mut fraction_digits: i64 = 0;
        let mut skipped_integer_digits: i64 = 0;
        let mut round_digit: u8 = 0;
        let mut sticky = false;
        let mut dropped_any = false;
        let mut seen_digit = false;
        let mut seen_point = false;

        while i < n {
            let byte = bytes[i];
            if byte == b'.' {
                if seen_point {
                    return Err(ParseDecimalError::Invalid);
                }
                seen_point = true;
                i += 1;
                continue;
            }
            if byte < b'0' || byte > b'9' {
                break;
            }
            seen_digit = true;
            let digit = byte - b'0';
            if magnitude < ACCUMULATE_LIMIT {
                magnitude = magnitude * 10 + digit as u128;
                if seen_point {
                    fraction_digits += 1;
                }
            } else {
                // No room left in the accumulator: the digit only decides the rounding.
                if !seen_point {
                    skipped_integer_digits += 1;
                }
                if dropped_any {
                    if digit != 0 {
                        sticky = true;
                    }
                } else {
                    round_digit = digit;
                    dropped_any = true;
                }
            }
            i += 1;
        }
        if !seen_digit {
            return Err(ParseDecimalError::Invalid);
        }
        let mut rounded_off = false;
        if dropped_any {
            if round_digit > 5 || (round_digit == 5 && (sticky || magnitude % 2 == 1)) {
                magnitude += 1;
            }
            rounded_off = true;
        }

        let mut exponent: i64 = 0;
        if i < n && (bytes[i] == b'e' || bytes[i] == b'E') {
            i += 1;
            let mut exponent_negative = false;
            if i < n && (bytes[i] == b'+' || bytes[i] == b'-') {
                exponent_negative = bytes[i] == b'-';
                i += 1;
            }
            let mut seen_exponent_digit = false;
            while i < n && bytes[i] >= b'0' && bytes[i] <= b'9' {
                seen_exponent_digit = true;
                // Clamped: anything past a few hundred is out of range either way.
                if exponent < 1_000_000 {
                    exponent = exponent * 10 + (bytes[i] - b'0') as i64;
                }
                i += 1;
            }
            if !seen_exponent_digit {
                return Err(ParseDecimalError::Invalid);
            }
            if exponent_negative {
                exponent = -exponent;
            }
        }
        while i < n && is_space(bytes[i]) {
            i += 1;
        }
        if i != n {
            return Err(ParseDecimalError::Invalid);
        }

        // value == magnitude × 10^(skipped_integer_digits + exponent - fraction_digits)
        let mut scale = fraction_digits - skipped_integer_digits - exponent;
        if magnitude == 0 {
            // Zero has no significant digits to place; keep the written fraction width when
            // it is representable, so `0.00` stays `0.00`.
            if scale < 0 || scale > Self::MAX_SCALE as i64 {
                scale = 0;
            }
            return Ok(Self {
                mantissa: 0,
                scale: scale as u8,
            });
        }

        if scale < 0 {
            let shift = -scale;
            if shift > 38 {
                return Err(ParseDecimalError::Overflow);
            }
            let factor = pow10_i128(shift as u32) as u128;
            let Some(scaled) = magnitude.checked_mul(factor) else {
                return Err(ParseDecimalError::Overflow);
            };
            magnitude = scaled;
            scale = 0;
        } else if scale > Self::MAX_SCALE as i64 {
            let drop = scale - Self::MAX_SCALE as i64;
            if drop > 39 {
                magnitude = 0;
            } else {
                let dropped = drop as u32;
                magnitude = round_off(magnitude, dropped, false);
            }
            scale = Self::MAX_SCALE as i64;
            rounded_off = true;
        }

        // Whatever is left has to fit an i64, which may cost further fraction digits.
        while magnitude > i64::MAX as u128 {
            if scale == 0 {
                return Err(ParseDecimalError::Overflow);
            }
            magnitude = round_off(magnitude, 1, false);
            scale -= 1;
            rounded_off = true;
        }
        let _ = rounded_off;
        let mantissa = magnitude as i64;
        Ok(Self {
            mantissa: if negative { -mantissa } else { mantissa },
            scale: scale as u8,
        })
    }

    /// Parses a Rust numeric literal, panicking instead of returning an error.
    ///
    /// This is what [`decimal!`](crate::decimal) expands to, so the panic is a compile error
    /// rather than a runtime one. It differs from [`from_ascii`](Self::from_ascii) in
    /// accepting Rust's `_` digit separators — `decimal!(11_000)` is how the number would be
    /// written anywhere else in the source, and JSON never contains one, so the leniency
    /// costs the wire path nothing.
    ///
    /// # Panics
    ///
    /// If the text is not a decimal number.
    #[must_use]
    pub const fn from_literal(text: &str) -> Self {
        let bytes = text.as_bytes();
        // A stack copy with the separators removed; `const` rules out allocating one.
        let mut digits = [0u8; 64];
        let mut len = 0usize;
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] != b'_' {
                assert!(len < 64, "decimal literal is too long");
                digits[len] = bytes[i];
                len += 1;
            }
            i += 1;
        }
        let (kept, _) = digits.split_at(len);
        match Self::from_ascii(kept) {
            Ok(value) => value,
            Err(ParseDecimalError::Invalid) => panic!("not a decimal number"),
            Err(ParseDecimalError::Overflow) => panic!("decimal number out of range"),
        }
    }

    // -- floating point -----------------------------------------------------

    /// The value as an `f64`, correctly rounded.
    ///
    /// Named for what it costs: past 15 significant digits the result is not the number this
    /// `Decimal` holds, and no arithmetic done on it afterwards is exact. Nothing in this
    /// crate calls it.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // The whole point of the name.
    pub fn to_f64_lossy(self) -> f64 {
        const POW10_F64: [f64; 19] = [
            1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15,
            1e16, 1e17, 1e18,
        ];
        let mantissa = self.mantissa as f64;
        mantissa / POW10_F64[self.scale as usize]
    }

    /// The decimal an `f64` is nearest to, or `None` for an infinity, a NaN, or a magnitude
    /// out of range.
    ///
    /// Named for what it costs: the `f64` `0.1` is not one tenth, and this returns the
    /// shortest decimal that reads back as the same `f64` — `0.1` — rather than the value it
    /// actually holds. That is the friendly answer, and it is still a guess about what the
    /// number was before it became a float. Parse the text instead when there is text.
    #[must_use]
    pub fn from_f64_lossy(value: f64) -> Option<Self> {
        if !value.is_finite() {
            return None;
        }
        let mut buffer = TextBuffer::new();
        // The exponent form is the shortest round-tripping spelling and is bounded in length;
        // `{}` would spell 1e300 out in full.
        if fmt::Write::write_fmt(&mut buffer, format_args!("{value:e}")).is_err() {
            return None;
        }
        Self::from_ascii(buffer.as_bytes()).ok()
    }
}

/// Drops `digits` decimal digits from `magnitude`, rounding half to even.
///
/// `sticky` says whether anything non-zero was already discarded below those digits, which is
/// what turns an exact half into a value above it.
const fn round_off(magnitude: u128, digits: u32, sticky: bool) -> u128 {
    if digits == 0 {
        return magnitude;
    }
    if digits > 38 {
        return 0;
    }
    let divisor = pow10_i128(digits) as u128;
    let quotient = magnitude / divisor;
    let remainder = magnitude % divisor;
    let half = divisor / 2;
    if remainder > half || (remainder == half && (sticky || quotient % 2 == 1)) {
        quotient + 1
    } else {
        quotient
    }
}

/// Narrows an `i128` mantissa back to a `Decimal`, or `None` when it does not fit.
const fn narrow(mantissa: i128, scale: u8) -> Option<Decimal> {
    if mantissa > i64::MAX as i128 || mantissa < i64::MIN as i128 {
        return None;
    }
    Some(Decimal {
        mantissa: mantissa as i64,
        scale,
    })
}

const fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

/// Why a decimal could not be parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseDecimalError {
    /// The text is not a number.
    Invalid,
    /// The number needs more than 19 digits before the decimal point.
    Overflow,
}

impl fmt::Display for ParseDecimalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ParseDecimalError::Invalid => "not a decimal number",
            ParseDecimalError::Overflow => "decimal number out of range",
        })
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ParseDecimalError {}

impl FromStr for Decimal {
    type Err = ParseDecimalError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::from_ascii(text.as_bytes())
    }
}

impl fmt::Debug for Decimal {
    /// The number, not its representation: `Decimal(2935.600)`. A `{mantissa, scale}` dump is
    /// what a reader has to decode by hand every time one of these appears in a log line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Decimal({self})")
    }
}

impl fmt::Display for Decimal {
    /// Writes the value at its own scale, so `2935.600` prints as `2935.600`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.scale == 0 {
            return write!(f, "{}", self.mantissa);
        }
        let magnitude = self.mantissa.unsigned_abs();
        let divisor = POW10_U64[self.scale as usize];
        let sign = if self.mantissa < 0 { "-" } else { "" };
        write!(
            f,
            "{sign}{}.{:0width$}",
            magnitude / divisor,
            magnitude % divisor,
            width = self.scale as usize
        )
    }
}

// ---------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------

impl PartialEq for Decimal {
    fn eq(&self, other: &Self) -> bool {
        self.compare(*other) == Ordering::Equal
    }
}

impl Eq for Decimal {}

impl PartialOrd for Decimal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Decimal {
    fn cmp(&self, other: &Self) -> Ordering {
        self.compare(*other)
    }
}

impl Hash for Decimal {
    /// Hashes the normalized value, so `2.50` and `2.5` — which compare equal — hash equal.
    fn hash<H: Hasher>(&self, state: &mut H) {
        let normalized = self.normalized();
        normalized.mantissa.hash(state);
        normalized.scale.hash(state);
    }
}

// ---------------------------------------------------------------------------
// Operators
// ---------------------------------------------------------------------------

macro_rules! binary_op {
    ($trait:ident, $method:ident, $checked:ident, $what:literal) => {
        impl $trait for Decimal {
            type Output = Decimal;
            /// # Panics
            ///
            /// On overflow, like the integer operators. Use the `checked_` form to handle it.
            fn $method(self, rhs: Decimal) -> Decimal {
                self.$checked(rhs)
                    .expect(concat!("Decimal ", $what, " overflowed"))
            }
        }
    };
}

binary_op!(Add, add, checked_add, "addition");
binary_op!(Sub, sub, checked_sub, "subtraction");
binary_op!(Mul, mul, checked_mul, "multiplication");

impl Div for Decimal {
    type Output = Decimal;

    /// Division at [`Decimal::MAX_SCALE`], rounded half to even.
    ///
    /// # Panics
    ///
    /// On division by zero or overflow. [`Decimal::checked_div`] takes the scale to round at
    /// and returns `None` instead, which is what production code should use.
    fn div(self, rhs: Decimal) -> Decimal {
        self.checked_div(rhs, Decimal::MAX_SCALE)
            .expect("Decimal division by zero or overflow")
    }
}

impl Neg for Decimal {
    type Output = Decimal;

    /// # Panics
    ///
    /// Only for a mantissa of `i64::MIN`, which no parsed value has.
    fn neg(self) -> Decimal {
        self.checked_neg().expect("Decimal negation overflowed")
    }
}

impl AddAssign for Decimal {
    fn add_assign(&mut self, rhs: Decimal) {
        *self = *self + rhs;
    }
}

impl SubAssign for Decimal {
    fn sub_assign(&mut self, rhs: Decimal) {
        *self = *self - rhs;
    }
}

impl core::iter::Sum for Decimal {
    fn sum<I: Iterator<Item = Decimal>>(iter: I) -> Decimal {
        iter.fold(Decimal::ZERO, |total, value| total + value)
    }
}

// ---------------------------------------------------------------------------
// Conversions
// ---------------------------------------------------------------------------

macro_rules! from_integer {
    ($($ty:ty),*) => {
        $(
            impl From<$ty> for Decimal {
                fn from(value: $ty) -> Self {
                    Self { mantissa: i64::from(value), scale: 0 }
                }
            }
        )*
    };
}

from_integer!(i8, i16, i32, i64, u8, u16, u32);

impl TryFrom<u64> for Decimal {
    type Error = ParseDecimalError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        i64::try_from(value)
            .map(Self::from_i64)
            .map_err(|_| ParseDecimalError::Overflow)
    }
}

impl TryFrom<Decimal> for i64 {
    type Error = ParseDecimalError;

    /// Succeeds only when the value is a whole number.
    fn try_from(value: Decimal) -> Result<Self, Self::Error> {
        value
            .rescale(0)
            .map(Decimal::mantissa)
            .ok_or(ParseDecimalError::Invalid)
    }
}

// ---------------------------------------------------------------------------
// A stack buffer, so formatting a Decimal never allocates
// ---------------------------------------------------------------------------

/// Enough for a sign, 19 digits, a point and an exponent.
const TEXT_CAPACITY: usize = 40;

struct TextBuffer {
    bytes: [u8; TEXT_CAPACITY],
    len: usize,
}

impl TextBuffer {
    const fn new() -> Self {
        Self {
            bytes: [0; TEXT_CAPACITY],
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    fn as_str(&self) -> &str {
        // Only `core::fmt` writes here, and it only writes UTF-8; the slice ends on a
        // character boundary because a write either fits whole or fails.
        core::str::from_utf8(self.as_bytes()).unwrap_or("0")
    }
}

impl fmt::Write for TextBuffer {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.len + text.len();
        if end > TEXT_CAPACITY {
            return Err(fmt::Error);
        }
        self.bytes[self.len..end].copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// serde
// ---------------------------------------------------------------------------

/// `serde_json`'s private marker for a value carried as raw JSON text.
///
/// Asking for it is how a `Deserialize` impl sees the number *as the peer wrote it* rather
/// than as whatever `f64` the parser would round it to — which is the entire point of this
/// type. `serde_json` answers with the source token; any other data format falls through to
/// `deserialize_any` below and gets the ordinary numeric path.
const RAW_TOKEN: &str = "$serde_json::private::RawValue";

impl Serialize for Decimal {
    /// Writes the number as the peer would have written it: the mantissa at its own scale, so
    /// `2935.600` goes back out as `2935.600` and not as `2935.6`.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;
        let mut buffer = TextBuffer::new();
        if fmt::Write::write_fmt(&mut buffer, format_args!("{self}")).is_err() {
            return Err(serde::ser::Error::custom("decimal is too long to render"));
        }
        let mut raw = serializer.serialize_struct(RAW_TOKEN, 1)?;
        raw.serialize_field(RAW_TOKEN, buffer.as_str())?;
        raw.end()
    }
}

impl<'de> Deserialize<'de> for Decimal {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_newtype_struct(RAW_TOKEN, DecimalVisitor)
    }
}

struct DecimalVisitor;

impl<'de> serde::de::Visitor<'de> for DecimalVisitor {
    type Value = Decimal;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON number")
    }

    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Decimal, E> {
        Ok(Decimal::from_i64(value))
    }

    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Decimal, E> {
        Decimal::try_from(value).map_err(|_| E::custom("number out of range for a decimal"))
    }

    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Decimal, E> {
        Decimal::from_f64_lossy(value)
            .ok_or_else(|| E::custom("number is not a finite decimal in range"))
    }

    /// The data format handed back a newtype rather than raw text — it is not `serde_json`.
    /// Take the ordinary numeric path.
    fn visit_newtype_struct<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Decimal, D::Error> {
        deserializer.deserialize_any(DecimalVisitor)
    }

    /// `serde_json` answers the raw-value request with a one-entry map holding the source
    /// token. Parsing that token is what keeps `2935.600` from becoming `2935.6`.
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Decimal, A::Error> {
        use serde::de::Error as _;
        let Some(RawKey) = map.next_key::<RawKey>()? else {
            return Err(A::Error::custom("expected a JSON number"));
        };
        let RawToken(value) = map.next_value::<RawToken>()?;
        Ok(value)
    }
}

/// The key of `serde_json`'s raw-value map, which is a fixed marker and carries no meaning.
struct RawKey;

impl<'de> Deserialize<'de> for RawKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KeyVisitor;
        impl serde::de::Visitor<'_> for KeyVisitor {
            type Value = RawKey;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a raw value marker")
            }
            fn visit_str<E: serde::de::Error>(self, _value: &str) -> Result<RawKey, E> {
                Ok(RawKey)
            }
        }
        deserializer.deserialize_identifier(KeyVisitor)
    }
}

/// One raw JSON token, parsed as a decimal.
struct RawToken(Decimal);

impl<'de> Deserialize<'de> for RawToken {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TokenVisitor;
        impl serde::de::Visitor<'_> for TokenVisitor {
            type Value = RawToken;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON number")
            }

            fn visit_str<E: serde::de::Error>(self, token: &str) -> Result<RawToken, E> {
                use serde::de::Unexpected;
                // The token is whatever stood where a number belongs. Reporting the *JSON
                // type* of what actually arrived is what keeps `invalid type: …` — and with
                // it the TypeConstraintViolation classification, and the numeric-string
                // repair — working exactly as it does for every other field.
                let trimmed = token.trim();
                let unexpected = match trimmed.as_bytes().first() {
                    Some(b'"') => Some(Unexpected::Str(trimmed.trim_matches('"'))),
                    Some(b'{') => Some(Unexpected::Map),
                    Some(b'[') => Some(Unexpected::Seq),
                    Some(b't') => Some(Unexpected::Bool(true)),
                    Some(b'f') => Some(Unexpected::Bool(false)),
                    Some(b'n') => Some(Unexpected::Unit),
                    _ => None,
                };
                if let Some(unexpected) = unexpected {
                    return Err(E::invalid_type(unexpected, &"a JSON number"));
                }
                Decimal::from_ascii(trimmed.as_bytes())
                    .map(RawToken)
                    .map_err(|error| {
                        let expected: &str = match error {
                            ParseDecimalError::Overflow => "a decimal number in range",
                            ParseDecimalError::Invalid => "a decimal number",
                        };
                        E::invalid_value(Unexpected::Other(trimmed), &expected)
                    })
            }
        }
        deserializer.deserialize_str(TokenVisitor)
    }
}

impl Validate for Decimal {
    /// Nothing to check: unlike an `f64`, a `Decimal` cannot hold a value JSON has no
    /// spelling for. Range constraints are emitted per field by the code generator.
    fn validate_at(&self, _path: &mut ValidationPath, _out: &mut Violations) {}
}

// ---------------------------------------------------------------------------
// Literals
// ---------------------------------------------------------------------------

/// An exact [`Decimal`] literal, parsed at compile time.
///
/// ```
/// use ocpp_kit::decimal;
///
/// let limit = decimal!(32.5);
/// assert_eq!(limit.scale(), 1);
/// assert_eq!(limit.mantissa(), 325);
/// ```
///
/// The digits are read from the source text, so the value is the one written rather than the
/// nearest `f64` to it, and a malformed literal is a compile error.
#[macro_export]
macro_rules! decimal {
    ($($token:tt)+) => {
        const {
            $crate::types::Decimal::from_literal(::core::stringify!($($token)+))
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn the_scale_the_station_sent_survives_the_round_trip() {
        // The whole point: `2935.600` states three decimals of resolution, and an f64 cannot
        // hold that claim — `2935.600` and `2935.6` are the same f64.
        let value: Decimal = serde_json::from_str("2935.600").unwrap();
        assert_eq!(value.scale(), 3);
        assert_eq!(value.mantissa(), 2_935_600);
        assert_eq!(serde_json::to_string(&value).unwrap(), "2935.600");
        assert_eq!(value.to_string(), "2935.600");

        // Equal in value, different in what they claim about the meter.
        let short: Decimal = serde_json::from_str("2935.6").unwrap();
        assert_eq!(value, short);
        assert!(!value.eq_exact(short));
    }

    #[test]
    fn a_difference_of_two_registers_is_exact() {
        // The f64 answer to this is 10.000000000000002.
        let start: Decimal = "0.1".parse().unwrap();
        let stop: Decimal = "10.2".parse().unwrap();
        assert_eq!((stop - start).to_string(), "10.1");
    }

    #[test]
    fn every_json_number_spelling_parses() {
        for (text, expected) in [
            ("0", "0"),
            ("-0", "0"),
            ("42", "42"),
            ("-17.25", "-17.25"),
            ("0.00", "0.00"),
            ("1e3", "1000"),
            ("1.5E+3", "1500"),
            ("15e-1", "1.5"),
            ("2E-4", "0.0002"),
            ("0.000000000000000001", "0.000000000000000001"),
            ("9223372036854775807", "9223372036854775807"),
        ] {
            let value: Decimal = text.parse().unwrap_or_else(|_| panic!("{text}"));
            assert_eq!(value.to_string(), expected, "{text}");
        }
        for text in ["", "-", "abc", "1.2.3", "1e", "NaN", "1 2", "--1"] {
            assert!(text.parse::<Decimal>().is_err(), "{text} should not parse");
        }
    }

    #[test]
    fn excess_precision_rounds_half_to_even_rather_than_failing() {
        // 20 fraction digits: the last two are past what 19 significant digits can hold.
        let value: Decimal = "0.12345678901234567895".parse().unwrap();
        assert_eq!(value.scale(), Decimal::MAX_SCALE);
        assert_eq!(value.to_string(), "0.123456789012345679");
        // An integer that cannot be rounded into range is refused rather than mangled.
        assert_eq!("1e30".parse::<Decimal>(), Err(ParseDecimalError::Overflow));
    }

    #[test]
    fn arithmetic_is_exact_and_checked() {
        let a = decimal!(1.05);
        let b = decimal!(2.1);
        assert_eq!((a + b).to_string(), "3.15");
        assert_eq!((b - a).to_string(), "1.05");
        assert_eq!((a * b).to_string(), "2.205");
        assert_eq!(a.checked_div(b, 4).unwrap().to_string(), "0.5000");
        // Half to even, at the requested scale.
        assert_eq!(
            decimal!(1).checked_div(decimal!(3), 5).unwrap().to_string(),
            "0.33333"
        );
        assert_eq!(
            decimal!(2).checked_div(decimal!(3), 5).unwrap().to_string(),
            "0.66667"
        );
        assert_eq!(decimal!(0.125).round_to(2).unwrap().to_string(), "0.12");
        assert_eq!(decimal!(0.135).round_to(2).unwrap().to_string(), "0.14");
        assert_eq!(Decimal::new(i64::MAX, 0).checked_add(Decimal::ONE), None);
        assert_eq!(decimal!(1).checked_div(Decimal::ZERO, 2), None);
    }

    /// The 2.x `unitOfMeasure.multiplier` is an `i32` straight off the wire, so every value
    /// an `i32` can hold has to have an answer — including the one whose negation overflows.
    #[test]
    fn an_absurd_exponent_is_refused_rather_than_overflowing() {
        assert_eq!(decimal!(1.5).checked_pow10(i32::MIN), None);
        assert_eq!(decimal!(1.5).checked_pow10(i32::MAX), None);
        assert_eq!(Decimal::ZERO.checked_pow10(i32::MIN), Some(Decimal::ZERO));
        assert_eq!(decimal!(1).checked_pow10(19), None);
        assert_eq!(
            decimal!(1).checked_pow10(18).unwrap().mantissa(),
            10i64.pow(18)
        );
    }

    #[test]
    fn a_change_of_unit_prefix_only_moves_the_point() {
        // kWh to Wh, the conversion that is a routine factor-1000 bug.
        let kwh = decimal!(2935.600);
        assert_eq!(kwh.checked_pow10(3).unwrap().to_string(), "2935600");
        assert_eq!(kwh.checked_pow10(-3).unwrap().to_string(), "2.935600");
        assert_eq!(decimal!(1.5).checked_pow10(1).unwrap().to_string(), "15");
    }

    #[test]
    fn comparison_is_numeric_and_ordering_is_total() {
        assert_eq!(decimal!(2.50), decimal!(2.5));
        assert!(decimal!(2.5) < decimal!(10));
        assert!(decimal!(-1) < Decimal::ZERO);
        assert_eq!(decimal!(1.10).min(decimal!(1.2)), decimal!(1.1));
        assert_eq!(decimal!(1.10).max(decimal!(1.2)), decimal!(1.2));
        assert_eq!(decimal!(2.500).normalized().to_string(), "2.5");
    }

    /// Decoding goes through `serde_json::Deserializer`, the leniency repair loop goes
    /// through `serde_json::Value`, and unknown-field detection goes through
    /// `serde_json::to_value`. All three have to see the same number.
    #[test]
    fn every_serde_json_entry_point_agrees() {
        let value: Decimal = serde_json::from_str("1.230").unwrap();
        assert_eq!(serde_json::to_value(value).unwrap().to_string(), "1.23");
        let from_value: Decimal = serde_json::from_value(serde_json::json!(1.23)).unwrap();
        assert_eq!(from_value, decimal!(1.23));
        let from_int: Decimal = serde_json::from_value(serde_json::json!(7)).unwrap();
        assert_eq!(from_int, decimal!(7));

        // And through `serde_path_to_error`, which is what the decoder actually uses.
        let mut de = serde_json::Deserializer::from_str("2935.600");
        let traced: Decimal = serde_path_to_error::deserialize(&mut de).unwrap();
        assert!(traced.eq_exact(decimal!(2935.600)));
    }

    /// The classification the decoder builds on: a string where a number belongs has to read
    /// as a *type* error, or `NumericStrings::Coerce` never fires and the OCPP error code is
    /// wrong.
    #[test]
    fn a_wrong_json_type_still_reads_as_a_type_error() {
        let error = serde_json::from_str::<Decimal>("\"42\"").unwrap_err();
        assert!(
            error.to_string().starts_with("invalid type: string"),
            "{error}"
        );
        assert_eq!(error.classify(), serde_json::error::Category::Data);
        for (text, expected) in [
            ("true", "invalid type: boolean"),
            ("null", "invalid type: null"),
            ("[]", "invalid type: sequence"),
            ("{}", "invalid type: map"),
        ] {
            let error = serde_json::from_str::<Decimal>(text).unwrap_err();
            assert!(error.to_string().starts_with(expected), "{text}: {error}");
        }
    }

    #[test]
    fn floating_point_conversion_is_available_but_has_to_be_asked_for() {
        assert_eq!(Decimal::from_f64_lossy(0.1).unwrap(), decimal!(0.1));
        assert_eq!(
            Decimal::from_f64_lossy(1e18).unwrap().to_string(),
            "1000000000000000000"
        );
        assert_eq!(Decimal::from_f64_lossy(f64::NAN), None);
        // Past 19 digits there is no mantissa to hold it, and saying so beats guessing.
        assert_eq!(Decimal::from_f64_lossy(1e300), None);
        assert!((decimal!(2935.600).to_f64_lossy() - 2935.6).abs() < 1e-9);
    }

    #[test]
    fn equal_values_hash_equally_whatever_their_scale() {
        use core::hash::BuildHasher;
        // One hasher state, or the two hashes would be incomparable whatever the values.
        let state = std::collections::hash_map::RandomState::new();
        assert_eq!(
            state.hash_one(decimal!(2.50)),
            state.hash_one(decimal!(2.5))
        );
        assert_eq!(
            state.hash_one(Decimal::ZERO),
            state.hash_one(decimal!(0.000))
        );
    }
}
