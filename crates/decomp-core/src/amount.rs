//! Amounts held as whole numbers of a smallest unit, and written as people
//! write them: credits in millionths, tokens in hundred-millionths.

/// Writes `value` smallest units, of which `places` decimal places make one,
/// with at least two decimals and as many more as it takes to show it exactly.
pub(crate) fn format(value: u64, places: u32) -> String {
    let one = 10u64.pow(places);
    let fraction = format!("{:0width$}", value % one, width = places as usize);
    let fraction = fraction.trim_end_matches('0');
    format!("{}.{fraction:0<2}", value / one)
}

/// Reads an amount such as `12` or `12.5` as smallest units, of which `places`
/// decimal places make one. `None` if it is not such an amount or too large.
pub(crate) fn parse(text: &str, places: u32) -> Option<u64> {
    let width = places as usize;
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if fraction.len() > width || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let whole: u64 = whole.parse().ok()?;
    let fraction: u64 = format!("{fraction:0<width$}").parse().ok()?;
    whole.checked_mul(10u64.pow(places))?.checked_add(fraction)
}
