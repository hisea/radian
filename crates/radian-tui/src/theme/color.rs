//! The color grammar of a theme file (ADR 0029 decision 2): hex, `oklch()`, `okhsl()`,
//! 0–255 indices, variable references and `""`. Everything here is a **pure function** of its
//! inputs, so the conversion tables can be pinned against pi's own implementation.
//!
//! The OKLab/OKHSL math is a direct port of pi's port of Björn Ottosson's reference
//! implementation (<https://bottosson.github.io/posts/colorpicker/>, MIT), so a color a pi
//! theme names and the color Radian renders are the same bytes. Out-of-gamut `oklch` is
//! clipped by reducing chroma (pi's bisection); `okhsl` is in-gamut by construction. For a
//! terminal without truecolor (`COLORTERM` is neither `truecolor` nor `24bit`, ADR 0029
//! decision 2) [`approximate_256`] picks the nearest 256-color palette entry with the same
//! algorithm pi uses, so both render the same fallback.

/// An sRGB color, channels 0–255.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// One parsed color value from a theme file or a `[tui.colors]` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorValue {
    /// A concrete sRGB color (`#hex`, `oklch()`, `okhsl()`, or a resolved variable).
    Rgb(Rgb),
    /// A 256-color palette index, 0–255.
    Indexed(u8),
    /// `""`: the terminal's default foreground or background.
    Default,
}

// The Oklab matrices and the OKHSL fit constants (pi's `oklab.js`, Ottosson's reference).
const LINEAR_SRGB_TO_LMS: [[f64; 3]; 3] = [
    [
        0.412_221_469_470_762_9,
        0.536_332_537_261_734_9,
        0.051_445_993_267_502_2,
    ],
    [
        0.211_903_495_817_825_1,
        0.680_699_550_645_234_4,
        0.107_396_953_536_940_5,
    ],
    [
        0.088_302_459_190_056_4,
        0.281_718_839_136_121_5,
        0.629_978_701_673_822_2,
    ],
];
const LAB_TO_LMS: [[f64; 3]; 3] = [
    [1.0, 0.396_337_777_376_174_9, 0.215_803_757_309_913_6],
    [1.0, -0.105_561_345_815_658_6, -0.063_854_172_825_813_3],
    [1.0, -0.089_484_177_529_811_9, -1.291_485_548_019_409_2],
];
const LMS_TO_LINEAR_SRGB: [[f64; 3]; 3] = [
    [
        4.076_741_636_075_958_3,
        -3.307_711_539_258_062_9,
        0.230_969_903_182_104_3,
    ],
    [
        -1.268_437_973_285_031_5,
        2.609_757_349_287_688_2,
        -0.341_319_376_002_657,
    ],
    [
        -0.004_196_076_138_675_6,
        -0.703_418_617_935_936_2,
        1.707_614_694_074_611_7,
    ],
];

const K1: f64 = 0.206;
const K2: f64 = 0.03;
const K3: f64 = (1.0 + K1) / (1.0 + K2);

fn multiply(m: [[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

/// sRGB transfer function: linear to encoded channel, both 0–1.
fn linear_to_srgb(value: f64) -> f64 {
    if value > 0.003_130_8 {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    } else {
        12.92 * value
    }
}

/// Oklab `[L, a, b]` to linear sRGB `[r, g, b]` (0–1, may leave the gamut).
fn oklab_to_linear_srgb(lab: [f64; 3]) -> [f64; 3] {
    let lms = multiply(LAB_TO_LMS, lab).map(|value| value.powi(3));
    multiply(LMS_TO_LINEAR_SRGB, lms)
}

/// Linear sRGB `[r, g, b]` to sRGB channels (0–255, rounded), clipping out-of-gamut channels.
fn linear_srgb_to_rgb(linear: [f64; 3]) -> Rgb {
    let [r, g, b] =
        linear.map(|value| (linear_to_srgb(value).clamp(0.0, 1.0) * 255.0).round() as u8);
    Rgb { r, g, b }
}

/// Whether every linear channel is (within epsilon) inside the sRGB gamut.
fn in_srgb_gamut(linear: [f64; 3]) -> bool {
    const EPSILON: f64 = 1e-7;
    linear
        .iter()
        .all(|&c| (-EPSILON..=1.0 + EPSILON).contains(&c))
}

/// OKLCH `[L 0..1, C, H degrees]` to sRGB. Out-of-gamut chroma is reduced: the largest chroma
/// that stays inside sRGB is found by bisection (20 steps, pi's `oklchToRgb`), so the hue is
/// kept and `oklch(100% 0.3 150)` still maps to white.
fn oklch_to_rgb(l: f64, c: f64, h: f64) -> Rgb {
    let radians = h * std::f64::consts::PI / 180.0;
    let (cos, sin) = (radians.cos(), radians.sin());
    let at_chroma = |chroma: f64| oklab_to_linear_srgb([l, chroma * cos, chroma * sin]);
    let direct = at_chroma(c);
    if in_srgb_gamut(direct) {
        return linear_srgb_to_rgb(direct);
    }
    let mut low = 0.0;
    let mut high = c;
    let mut linear = at_chroma(0.0);
    for _ in 0..20 {
        let chroma = (low + high) / 2.0;
        let candidate = at_chroma(chroma);
        if in_srgb_gamut(candidate) {
            low = chroma;
            linear = candidate;
        } else {
            high = chroma;
        }
    }
    linear_srgb_to_rgb(linear)
}

/// Rate of change of each cube-root LMS component along a chroma direction `(a, b)`.
fn lms_slopes(a: f64, b: f64) -> [f64; 3] {
    [
        LAB_TO_LMS[0][1] * a + LAB_TO_LMS[0][2] * b,
        LAB_TO_LMS[1][1] * a + LAB_TO_LMS[1][2] * b,
        LAB_TO_LMS[2][1] * a + LAB_TO_LMS[2][2] * b,
    ]
}

/// The `(a, b)` half-plane where each sRGB channel clips first, and the polynomial
/// approximating the maximum saturation there (Ottosson's fit).
const SATURATION_FIT: [([[f64; 2]; 1], [f64; 5]); 3] = [
    (
        [[-1.881_703_1, -0.809_365_01]],
        [
            1.190_862_77,
            1.765_767_28,
            0.596_626_41,
            0.755_151_97,
            0.567_712_45,
        ],
    ),
    (
        [[1.814_440_8, -1.194_452_67]],
        [
            0.739_565_15,
            -0.459_544_04,
            0.082_854_27,
            0.125_410_73,
            -0.145_032_04,
        ],
    ),
    (
        [[0.131_107_58, 1.813_339_71]],
        [
            1.357_336_52,
            -0.009_157_99,
            -1.151_302_1,
            -0.505_596_06,
            0.006_921_67,
        ],
    ),
];

/// Largest saturation (C/L) inside sRGB for hue `(a, b)`: polynomial fit plus one Halley step.
fn max_saturation(a: f64, b: f64) -> f64 {
    // The blue row is the fallback (pi: `index === 2 || x*a + y*b > 1`).
    let channel = SATURATION_FIT
        .iter()
        .position(|(half_plane, _)| half_plane[0][0] * a + half_plane[0][1] * b > 1.0)
        .unwrap_or(2);
    let [k0, k1, k2, k3, k4] = SATURATION_FIT[channel].1;
    let weights = LMS_TO_LINEAR_SRGB[channel];
    let saturation = k0 + k1 * a + k2 * b + k3 * a * a + k4 * a * b;
    let slopes = lms_slopes(a, b);
    let base = slopes.map(|k| 1.0 + saturation * k);
    let dot =
        |values: [f64; 3]| weights[0] * values[0] + weights[1] * values[1] + weights[2] * values[2];
    let f = dot(base.map(|value| value.powi(3)));
    let f1 = dot(std::array::from_fn(|i| 3.0 * slopes[i] * base[i] * base[i]));
    let f2 = dot(std::array::from_fn(|i| {
        6.0 * slopes[i] * slopes[i] * base[i]
    }));
    saturation - (f * f1) / (f1 * f1 - 0.5 * f * f2)
}

/// Oklab lightness and chroma of the most saturated sRGB color of hue `(a, b)`.
fn cusp(a: f64, b: f64) -> (f64, f64) {
    let saturation = max_saturation(a, b);
    let linear = oklab_to_linear_srgb([1.0, saturation * a, saturation * b]);
    let brightest = linear[0].max(linear[1]).max(linear[2]);
    let lightness = (1.0 / brightest).cbrt();
    (lightness, lightness * saturation)
}

/// Chroma where the constant-lightness line at `lightness` leaves the sRGB gamut.
fn max_chroma(a: f64, b: f64, lightness: f64, cusp: (f64, f64)) -> f64 {
    let (cusp_l, cusp_c) = cusp;
    if lightness <= cusp_l {
        return (cusp_c * lightness) / cusp_l;
    }
    // Upper half: triangle edge, then one Halley step against each channel reaching 1.
    let t = (cusp_c * (lightness - 1.0)) / (cusp_l - 1.0);
    let slopes = lms_slopes(a, b);
    let lms = slopes.map(|k| lightness + t * k);
    let cubes = lms.map(|value| value.powi(3));
    let first = std::array::from_fn(|i| 3.0 * slopes[i] * lms[i] * lms[i]);
    let second = std::array::from_fn(|i| 6.0 * slopes[i] * slopes[i] * lms[i]);
    let dot = |row: [f64; 3], values: [f64; 3]| {
        row[0] * values[0] + row[1] * values[1] + row[2] * values[2]
    };
    let steps = LMS_TO_LINEAR_SRGB.map(|row| {
        let f = dot(row, cubes) - 1.0;
        let f1 = dot(row, first);
        let f2 = dot(row, second);
        let u = f1 / (f1 * f1 - 0.5 * f * f2);
        if u >= 0.0 { -f * u } else { f64::MAX }
    });
    t + steps[0].min(steps[1]).min(steps[2])
}

/// OKHSL's chroma reference points at lightness `L` and hue `(a, b)`: `(c0, c_mid, c_max)`.
fn chroma_stops(l: f64, a: f64, b: f64) -> (f64, f64, f64) {
    let peak = cusp(a, b);
    let c_max = max_chroma(a, b, l, peak);
    let k = c_max / (l * (peak.1 / peak.0)).min((1.0 - l) * (peak.1 / (1.0 - peak.0)));
    let mid_s = 0.115_169_93
        + 1.0
            / (7.447_789_7
                + 4.159_012_4 * b
                + a * (-2.195_573_47
                    + 1.751_984_01 * b
                    + a * (-2.137_049_48 - 10.023_010_43 * b
                        + a * (-4.248_945_61 + 5.387_708_19 * b + 4.698_910_13 * a))));
    let mid_t = 0.112_396_42
        + 1.0
            / (1.613_203_2 - 0.681_243_79 * b
                + a * (0.403_706_12
                    + 0.901_481_23 * b
                    + a * (-0.270_879_43
                        + 0.612_239_9 * b
                        + a * (0.002_992_15 - 0.453_995_68 * b - 0.146_618_72 * a))));
    let c_mid = 0.9
        * k
        * (1.0 / (1.0 / (l * mid_s).powi(4) + 1.0 / ((1.0 - l) * mid_t).powi(4)))
            .sqrt()
            .sqrt();
    let c0 = (1.0 / (1.0 / (l * 0.4).powi(2) + 1.0 / ((1.0 - l) * 0.8).powi(2))).sqrt();
    (c0, c_mid, c_max)
}

/// OKHSL lightness to Oklab lightness (the inverse "toe" function).
fn okhsl_to_oklab_lightness(x: f64) -> f64 {
    (x * x + K1 * x) / (K3 * (x + K2))
}

/// OKHSL `[H degrees, S 0..1, L 0..1]` to sRGB channels (0–255, rounded). Saturation is
/// relative to the most the sRGB gamut allows at that hue and lightness, so the result is
/// always in gamut (pi's `okhslToRgb`).
pub fn okhsl_to_rgb(hue: f64, saturation: f64, lightness: f64) -> Rgb {
    let l = okhsl_to_oklab_lightness(lightness);
    let mut lab = [l, 0.0, 0.0];
    if l > 0.0 && l < 1.0 && saturation > 0.0 {
        let angle = 2.0 * std::f64::consts::PI * (hue.rem_euclid(360.0)) / 360.0;
        let (a, b) = (angle.cos(), angle.sin());
        let (c0, c_mid, c_max) = chroma_stops(l, a, b);
        // Chroma rises from 0 through c_mid at s = 0.8 to c_max at s = 1.
        let chroma = if saturation < 0.8 {
            let t = 1.25 * saturation;
            let k1 = 0.8 * c0;
            (t * k1) / (1.0 - (1.0 - k1 / c_mid) * t)
        } else {
            let t = 5.0 * (saturation - 0.8);
            let k1 = (0.2 * c_mid * c_mid * 1.25 * 1.25) / c0;
            c_mid + (t * k1) / (1.0 - (1.0 - k1 / (c_max - c_mid)) * t)
        };
        lab = [l, chroma * a, chroma * b];
    }
    linear_srgb_to_rgb(oklab_to_linear_srgb(lab))
}

/// The 256-color cube's channel levels.
const CUBE_VALUES: [u8; 6] = [0, 95, 135, 175, 215, 255];
/// The 256-color gray ramp, indexes 232–255.
const GRAY_VALUES: [u8; 24] = [
    8, 18, 28, 38, 48, 58, 68, 78, 88, 98, 108, 118, 128, 138, 148, 158, 168, 178, 188, 198, 208,
    218, 228, 238,
];

fn find_closest(values: &[u8], target: u8) -> usize {
    let mut closest = 0;
    let mut closest_distance = i32::MAX;
    for (index, &value) in values.iter().enumerate() {
        let distance = (i32::from(target) - i32::from(value)).abs();
        if distance < closest_distance {
            closest = index;
            closest_distance = distance;
        }
    }
    closest
}

/// Weighted squared RGB distance (pi's `colorDistance`: the luma coefficients).
fn color_distance(first: Rgb, second: Rgb) -> f64 {
    let dr = f64::from(first.r) - f64::from(second.r);
    let dg = f64::from(first.g) - f64::from(second.g);
    let db = f64::from(first.b) - f64::from(second.b);
    dr * dr * 0.299 + dg * dg * 0.587 + db * db * 0.114
}

/// The nearest 256-color palette entry for an RGB color, pi's `rgbToAnsi256`: the color cube
/// per channel, unless a nearly-gray color is closer to the gray ramp.
pub fn approximate_256(rgb: Rgb) -> u8 {
    let r_index = find_closest(&CUBE_VALUES, rgb.r);
    let g_index = find_closest(&CUBE_VALUES, rgb.g);
    let b_index = find_closest(&CUBE_VALUES, rgb.b);
    let cube = Rgb::new(
        CUBE_VALUES[r_index],
        CUBE_VALUES[g_index],
        CUBE_VALUES[b_index],
    );
    let cube_index = 16 + 36 * r_index + 6 * g_index + b_index;
    let luma = (0.299 * f64::from(rgb.r) + 0.587 * f64::from(rgb.g) + 0.114 * f64::from(rgb.b))
        .round() as u8;
    let gray_offset = find_closest(&GRAY_VALUES, luma);
    let gray_value = GRAY_VALUES[gray_offset];
    let spread = rgb.r.max(rgb.g).max(rgb.b) - rgb.r.min(rgb.g).min(rgb.b);
    let gray = Rgb::new(gray_value, gray_value, gray_value);
    if i32::from(spread) < 10 && color_distance(rgb, gray) < color_distance(rgb, cube) {
        232 + u8::try_from(gray_offset).unwrap_or(23)
    } else {
        u8::try_from(cube_index).unwrap_or(255)
    }
}

/// Whether a terminal renders truecolor: `COLORTERM` is `truecolor` or `24bit` (ADR 0029
/// decision 2; compared case-insensitively, like pi). Anything else approximates to 256.
pub fn truecolor_for(colorterm: Option<&str>) -> bool {
    colorterm.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "truecolor" | "24bit"
        )
    })
}

/// [`truecolor_for`] for this process's `COLORTERM`.
pub fn truecolor_env() -> bool {
    truecolor_for(std::env::var("COLORTERM").ok().as_deref())
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

fn is_hex_digit(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte) || (b'A'..=b'F').contains(&byte)
}

/// Scans one pi number (`[+-]?(digits[.digits]|.digits)(e[+-]digits)?`) at `i`, advancing it.
fn scan_number(bytes: &[u8], i: &mut usize) -> Option<f64> {
    let start = *i;
    if *i < bytes.len() && (bytes[*i] == b'+' || bytes[*i] == b'-') {
        *i += 1;
    }
    let digits_before = *i;
    while *i < bytes.len() && bytes[*i].is_ascii_digit() {
        *i += 1;
    }
    let had_integer = *i > digits_before;
    let had_fraction = if *i < bytes.len() && bytes[*i] == b'.' {
        *i += 1;
        let frac_start = *i;
        while *i < bytes.len() && bytes[*i].is_ascii_digit() {
            *i += 1;
        }
        *i > frac_start
    } else {
        false
    };
    if !had_integer && !had_fraction {
        return None;
    }
    // An exponent.
    if *i < bytes.len() && (bytes[*i] == b'e' || bytes[*i] == b'E') {
        let exponent = *i;
        *i += 1;
        if *i < bytes.len() && (bytes[*i] == b'+' || bytes[*i] == b'-') {
            *i += 1;
        }
        let exp_start = *i;
        while *i < bytes.len() && bytes[*i].is_ascii_digit() {
            *i += 1;
        }
        if *i == exp_start {
            *i = exponent; // `e` must be followed by digits; rewind.
        }
    }
    std::str::from_utf8(&bytes[start..*i]).ok()?.parse().ok()
}

fn skip_spaces(bytes: &[u8], i: &mut usize) {
    while *i < bytes.len() && bytes[*i] == b' ' {
        *i += 1;
    }
}

/// `true` when `bytes[i..]` starts with `prefix` (ASCII case-insensitive).
fn starts_with_ignore_case(bytes: &[u8], i: usize, prefix: &str) -> bool {
    let prefix = prefix.as_bytes();
    bytes.len() >= i + prefix.len() && bytes[i..i + prefix.len()].eq_ignore_ascii_case(prefix)
}

fn parse_hue(bytes: &[u8], i: &mut usize) -> Option<f64> {
    let hue = scan_number(bytes, i)?;
    skip_spaces(bytes, i);
    if starts_with_ignore_case(bytes, *i, "deg") {
        *i += 3;
        skip_spaces(bytes, i);
    }
    Some(hue)
}

/// `oklch(L C H)`: `L` optionally a percentage, `C` plain, `H` in degrees with an optional
/// `deg` (pi's `OKLCH_PATTERN`).
fn parse_oklch(value: &str) -> Option<ColorValue> {
    let bytes = value.as_bytes();
    let mut i = 6; // "oklch("
    skip_spaces(bytes, &mut i);
    let lightness = scan_number(bytes, &mut i)?;
    let percent = if i < bytes.len() && bytes[i] == b'%' {
        i += 1;
        true
    } else {
        false
    };
    if percent {
        if !(0.0..=100.0).contains(&lightness) {
            return None;
        }
    } else if !(0.0..=1.0).contains(&lightness) {
        return None;
    }
    let lightness = if percent {
        lightness / 100.0
    } else {
        lightness
    };
    skip_spaces(bytes, &mut i);
    let chroma = scan_number(bytes, &mut i)?;
    skip_spaces(bytes, &mut i);
    let hue = parse_hue(bytes, &mut i)?;
    skip_spaces(bytes, &mut i);
    if i != bytes.len() - 1 || bytes[i] != b')' || chroma < 0.0 {
        return None;
    }
    Some(ColorValue::Rgb(oklch_to_rgb(
        lightness,
        chroma,
        hue.rem_euclid(360.0),
    )))
}

/// `okhsl(H S L)`: `H` in degrees with an optional `deg`, `S` and `L` optionally percentages
/// (pi's `OKHSL_PATTERN`).
fn parse_okhsl(value: &str) -> Option<ColorValue> {
    let bytes = value.as_bytes();
    let mut i = 6; // "okhsl("
    skip_spaces(bytes, &mut i);
    let hue = parse_hue(bytes, &mut i)?;
    let percent = |i: &mut usize| -> Option<f64> {
        let value = scan_number(bytes, i)?;
        if *i < bytes.len() && bytes[*i] == b'%' {
            *i += 1;
            if !(0.0..=100.0).contains(&value) {
                return None;
            }
            return Some(value / 100.0);
        }
        if !(0.0..=1.0).contains(&value) {
            return None;
        }
        Some(value)
    };
    skip_spaces(bytes, &mut i);
    let saturation = percent(&mut i)?;
    skip_spaces(bytes, &mut i);
    let lightness = percent(&mut i)?;
    skip_spaces(bytes, &mut i);
    if i != bytes.len() - 1 || bytes[i] != b')' {
        return None;
    }
    Some(ColorValue::Rgb(okhsl_to_rgb(
        hue.rem_euclid(360.0),
        saturation,
        lightness,
    )))
}

/// Parses one color *text* value: `""` (terminal default), `#rgb`/`#rrggbb`, `oklch(..)`,
/// `okhsl(..)`. `None` means "not a color literal" — for a theme file that makes it a
/// **variable reference**, which the caller resolves (pi's `resolveVarRefs`).
pub fn parse_color_value(value: &str) -> Option<ColorValue> {
    if value.is_empty() {
        return Some(ColorValue::Default);
    }
    if let Some(rest) = value.strip_prefix('#') {
        let digits = rest.as_bytes();
        let expanded: Option<String> =
            match digits.len() {
                3 => Some(digits.iter().map(|&d| (d, d)).fold(
                    String::new(),
                    |mut acc, (hi, lo)| {
                        acc.push(hi as char);
                        acc.push(lo as char);
                        acc
                    },
                )),
                6 if digits.iter().all(|&d| is_hex_digit(d)) => Some(rest.to_owned()),
                _ => None,
            };
        let expanded = expanded?;
        if !expanded.bytes().all(is_hex_digit) {
            return None;
        }
        let channel = |range: std::ops::Range<usize>| u8::from_str_radix(&expanded[range], 16).ok();
        return Some(ColorValue::Rgb(Rgb::new(
            channel(0..2)?,
            channel(2..4)?,
            channel(4..6)?,
        )));
    }
    if starts_with_ignore_case(value.as_bytes(), 0, "oklch(") {
        return parse_oklch(value);
    }
    if starts_with_ignore_case(value.as_bytes(), 0, "okhsl(") {
        return parse_okhsl(value);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(r: u8, g: u8, b: u8) -> ColorValue {
        ColorValue::Rgb(Rgb::new(r, g, b))
    }

    /// Reference values produced by pi's own implementation (`@earendil-works/pi-tui`
    /// `dist/colors.js`, `colorToRgb(parseColor(..))`), read on 2026-10-13.
    #[test]
    fn oklch_matches_pis_conversion() {
        for (text, expected) in [
            ("oklch(62% 0.1 200)", rgb(28, 152, 158)),
            ("oklch(0.62 0.1 200)", rgb(28, 152, 158)),
            ("oklch(50% 0.2 145)", rgb(0, 121, 30)),
            ("oklch(85% 0.05 90)", rgb(218, 205, 169)),
        ] {
            assert_eq!(parse_color_value(text), Some(expected), "{text}");
        }
    }

    /// Out-of-gamut OKLCH is clipped by reducing chroma, keeping the hue.
    #[test]
    fn oklch_out_of_gamut_is_clipped_not_rejected() {
        // Both reference values from pi (its bisection path).
        assert_eq!(
            parse_color_value("oklch(70% 0.4 30)"),
            Some(rgb(255, 101, 81))
        );
        assert_eq!(
            parse_color_value("oklch(100% 0.3 150)"),
            Some(rgb(255, 255, 255)),
            "an achromatic lightness of 1 is white whatever the chroma"
        );
    }

    /// Reference values produced by pi's implementation for `okhsl(..)`.
    #[test]
    fn okhsl_matches_pis_conversion() {
        for (text, expected) in [
            ("okhsl(250 60% 55%)", rgb(78, 136, 194)),
            ("okhsl(30 100% 50%)", rgb(222, 17, 0)),
            ("okhsl(0 0% 0%)", rgb(0, 0, 0)),
            ("okhsl(0 0% 100%)", rgb(255, 255, 255)),
            ("okhsl(145 78% 61%)", rgb(87, 169, 92)),
            ("okhsl(200 50% 25%)", rgb(29, 64, 66)),
        ] {
            assert_eq!(parse_color_value(text), Some(expected), "{text}");
        }
    }

    #[test]
    fn hex_parses_short_and_long_and_rejects_other_lengths() {
        assert_eq!(parse_color_value("#DCD7BA"), Some(rgb(0xDC, 0xD7, 0xBA)));
        assert_eq!(parse_color_value("#0af"), Some(rgb(0, 0xAA, 0xFF)));
        assert_eq!(parse_color_value("#12"), None);
        assert_eq!(parse_color_value("#12345"), None);
        assert_eq!(parse_color_value("#1234567"), None);
        assert_eq!(parse_color_value("#gggggg"), None);
    }

    /// Pi's grammar: optional spaces around the numbers, `deg` suffix on the hue.
    #[test]
    fn functional_notation_follows_pis_grammar() {
        assert_eq!(
            parse_color_value("oklch( 62% 0.1 200 )"),
            parse_color_value("oklch(62% 0.1 200)")
        );
        assert_eq!(
            parse_color_value("okhsl(250deg 0.6 0.55)"),
            parse_color_value("okhsl(250 60% 55%)")
        );
        assert_eq!(
            parse_color_value("oklch(62% 0.1 200deg)"),
            Some(rgb(28, 152, 158))
        );
        assert_eq!(parse_color_value("oklch(62% 0.1 200 extra)"), None);
        assert_eq!(parse_color_value("oklch(62% 0.1)"), None);
        assert_eq!(parse_color_value("oklch(150% 0.1 200)"), None);
        assert_eq!(parse_color_value("okhsl(250 101% 55%)"), None);
    }

    #[test]
    fn empty_means_terminal_default_and_other_text_is_a_variable_reference() {
        assert_eq!(parse_color_value(""), Some(ColorValue::Default));
        assert_eq!(parse_color_value("accent"), None);
    }

    /// Reference indexes produced by pi's implementation (`foregroundAnsi(.., "256color")`).
    /// Both branches are pinned: `#1F1F28` (spread 9) goes to the gray ramp, `#2A2A37`
    /// (spread 13) to the cube.
    #[test]
    fn ansi256_approximation_matches_pis_algorithm() {
        for (hex, index) in [
            ("#DCD7BA", 187),
            ("#957FB8", 103),
            ("#1F1F28", 234),
            ("#FF0000", 196),
            ("#FEFEFE", 231),
            ("#727169", 242),
            ("#E46876", 168),
            ("#000000", 16),
            ("#FFFFFF", 231),
            ("#808080", 244),
            ("#010101", 16),
            ("#2A2A37", 17),
        ] {
            let Rgb { r, g, b } = match parse_color_value(hex) {
                Some(ColorValue::Rgb(rgb)) => rgb,
                other => panic!("{hex} did not parse: {other:?}"),
            };
            assert_eq!(approximate_256(Rgb::new(r, g, b)), index, "{hex}");
        }
    }

    #[test]
    fn truecolor_follows_colorterm() {
        assert!(truecolor_for(Some("truecolor")));
        assert!(truecolor_for(Some("24bit")));
        assert!(truecolor_for(Some("TRUECOLOR")));
        assert!(truecolor_for(Some(" truecolor ")));
        assert!(!truecolor_for(Some("256color")));
        assert!(!truecolor_for(Some("")));
        assert!(!truecolor_for(None));
    }
}
