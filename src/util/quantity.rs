//! Convert Kubernetes resource quantities (like `10Gi` or `500M`) to bytes.

/// Parse a Kubernetes quantity string into a number of bytes.
///
/// Supports binary suffixes (Ki, Mi, Gi, Ti, Pi, Ei), decimal suffixes
/// (k, M, G, T, P, E), the milli suffix (m) and exponents (like `1e9`).
/// Fractions are rounded up to the next whole byte.
pub fn parse_bytes(quantity: &str) -> Result<u64, String> {
    let value = quantity.trim();
    if value.is_empty() {
        return Err("quantity is empty".into());
    }

    // "1e3" is an exponent, but "1Ei" is a suffix: only digits may follow the "e".
    let exponent = value
        .find(['e', 'E'])
        .filter(|pos| *pos > 0)
        .and_then(|pos| Some((pos, value[pos + 1..].parse::<i32>().ok()?)));

    let (number, multiplier, divisor): (&str, u128, u128) = if let Some((pos, exp)) = exponent {
        let power = 10u128
            .checked_pow(exp.unsigned_abs())
            .ok_or_else(|| format!("quantity {quantity:?} is out of range"))?;
        if exp >= 0 {
            (&value[..pos], power, 1)
        } else {
            (&value[..pos], 1, power)
        }
    } else {
        let split = value
            .find(|c: char| c.is_ascii_alphabetic())
            .unwrap_or(value.len());
        let (number, suffix) = value.split_at(split);
        let (multiplier, divisor) = match suffix {
            "" => (1, 1),
            "Ki" => (1 << 10, 1),
            "Mi" => (1 << 20, 1),
            "Gi" => (1 << 30, 1),
            "Ti" => (1 << 40, 1),
            "Pi" => (1 << 50, 1),
            "Ei" => (1 << 60, 1),
            "m" => (1, 1000),
            "k" => (1_000, 1),
            "M" => (1_000_000, 1),
            "G" => (1_000_000_000, 1),
            "T" => (1_000_000_000_000, 1),
            "P" => (1_000_000_000_000_000, 1),
            "E" => (1_000_000_000_000_000_000, 1),
            other => return Err(format!("unknown suffix {other:?} in quantity {quantity:?}")),
        };
        (number, multiplier, divisor)
    };

    let number = number.strip_prefix('+').unwrap_or(number);
    if number.starts_with('-') {
        return Err(format!("quantity {quantity:?} must not be negative"));
    }
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty() && fraction.is_empty() {
        return Err(format!("quantity {quantity:?} has no number"));
    }
    if !whole
        .chars()
        .chain(fraction.chars())
        .all(|c| c.is_ascii_digit())
    {
        return Err(format!("invalid number in quantity {quantity:?}"));
    }

    // Work with an exact integer: digits / 10^len(fraction).
    let digits: u128 = format!("{whole}{fraction}")
        .parse()
        .map_err(|_| format!("quantity {quantity:?} is too large"))?;
    let scale = 10u128
        .checked_pow(fraction.len() as u32)
        .ok_or_else(|| format!("quantity {quantity:?} has too many decimal places"))?;
    let numerator = digits
        .checked_mul(multiplier)
        .ok_or_else(|| format!("quantity {quantity:?} is too large"))?;
    let denominator = scale
        .checked_mul(divisor)
        .ok_or_else(|| format!("quantity {quantity:?} is out of range"))?;
    let bytes = numerator.div_ceil(denominator);
    u64::try_from(bytes).map_err(|_| format!("quantity {quantity:?} is too large"))
}

#[cfg(test)]
mod tests {
    use super::parse_bytes;

    #[test]
    fn parses_plain_numbers() {
        assert_eq!(parse_bytes("0"), Ok(0));
        assert_eq!(parse_bytes("1234"), Ok(1234));
        assert_eq!(parse_bytes("+5"), Ok(5));
    }

    #[test]
    fn parses_binary_and_decimal_suffixes() {
        assert_eq!(parse_bytes("1Ki"), Ok(1024));
        assert_eq!(parse_bytes("10Gi"), Ok(10 * 1024 * 1024 * 1024));
        assert_eq!(parse_bytes("10G"), Ok(10_000_000_000));
        assert_eq!(parse_bytes("1k"), Ok(1000));
        assert_eq!(parse_bytes("2E"), Ok(2_000_000_000_000_000_000));
        assert_eq!(parse_bytes("1Ei"), Ok(1 << 60));
    }

    #[test]
    fn parses_fractions_and_exponents() {
        assert_eq!(parse_bytes("1.5Gi"), Ok(1536 * 1024 * 1024));
        assert_eq!(parse_bytes("0.5k"), Ok(500));
        assert_eq!(parse_bytes("1e3"), Ok(1000));
        assert_eq!(parse_bytes("1.5e3"), Ok(1500));
        assert_eq!(parse_bytes("1e-3"), Ok(1));
        // Fractions of a byte round up.
        assert_eq!(parse_bytes("1500m"), Ok(2));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse_bytes("").is_err());
        assert!(parse_bytes("-1Gi").is_err());
        assert!(parse_bytes("10GB").is_err());
        assert!(parse_bytes("abc").is_err());
        assert!(parse_bytes("Gi").is_err());
        assert!(parse_bytes("100Ei").is_err());
    }
}
