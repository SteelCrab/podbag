//! Localized numbers, sizes and times: the parts of `Intl` that app.js uses
//! (RelativeTimeFormat numeric "auto", NumberFormat units, percent), for the 7 languages.
//! Close to what a browser prints, not a full CLDR implementation.

#[derive(Clone, Copy)]
enum Unit {
    Year,
    Month,
    Week,
    Day,
    Hour,
    Minute,
}

const UNITS: [(Unit, f64); 6] = [
    (Unit::Year, 31_536_000.),
    (Unit::Month, 2_592_000.),
    (Unit::Week, 604_800.),
    (Unit::Day, 86_400.),
    (Unit::Hour, 3_600.),
    (Unit::Minute, 60.),
];

/// JavaScript's Math.round (halves go up).
fn js_round(x: f64) -> i64 {
    (x + 0.5).floor() as i64
}

/// A number with at most `digits` decimals, in the language's style.
pub fn number(x: f64, digits: u32, lang: &str) -> String {
    let scale = 10f64.powi(digits as i32);
    let v = (x * scale).round() / scale;
    let mut s = format!("{v:.*}", digits as usize);
    if s.contains('.') {
        s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    if s == "-0" {
        s = "0".into();
    }
    if matches!(lang, "es" | "vi") {
        s.replace('.', ",")
    } else {
        s
    }
}

fn unit_word(unit: Unit, n: i64, lang: &str) -> &'static str {
    let one = n.abs() == 1;
    match (lang, unit) {
        ("ko", Unit::Year) => "년",
        ("ko", Unit::Month) => "개월",
        ("ko", Unit::Week) => "주",
        ("ko", Unit::Day) => "일",
        ("ko", Unit::Hour) => "시간",
        ("ko", Unit::Minute) => "분",
        ("ja", Unit::Year) => "年",
        ("ja", Unit::Month) => "か月",
        ("ja", Unit::Week) => "週間",
        ("ja", Unit::Day) => "日",
        ("ja", Unit::Hour) => "時間",
        ("ja", Unit::Minute) => "分",
        ("zh-CN", Unit::Year) => "年",
        ("zh-CN", Unit::Month) => "个月",
        ("zh-CN", Unit::Week) => "周",
        ("zh-CN", Unit::Day) => "天",
        ("zh-CN", Unit::Hour) => "小时",
        ("zh-CN", Unit::Minute) => "分钟",
        ("es", Unit::Year) => {
            if one {
                "año"
            } else {
                "años"
            }
        }
        ("es", Unit::Month) => {
            if one {
                "mes"
            } else {
                "meses"
            }
        }
        ("es", Unit::Week) => {
            if one {
                "semana"
            } else {
                "semanas"
            }
        }
        ("es", Unit::Day) => {
            if one {
                "día"
            } else {
                "días"
            }
        }
        ("es", Unit::Hour) => {
            if one {
                "hora"
            } else {
                "horas"
            }
        }
        ("es", Unit::Minute) => {
            if one {
                "minuto"
            } else {
                "minutos"
            }
        }
        ("fil", Unit::Year) => "taon",
        ("fil", Unit::Month) => "buwan",
        ("fil", Unit::Week) => "linggo",
        ("fil", Unit::Day) => "araw",
        ("fil", Unit::Hour) => "oras",
        ("fil", Unit::Minute) => "minuto",
        ("vi", Unit::Year) => "năm",
        ("vi", Unit::Month) => "tháng",
        ("vi", Unit::Week) => "tuần",
        ("vi", Unit::Day) => "ngày",
        ("vi", Unit::Hour) => "giờ",
        ("vi", Unit::Minute) => "phút",
        (_, Unit::Year) => {
            if one {
                "year"
            } else {
                "years"
            }
        }
        (_, Unit::Month) => {
            if one {
                "month"
            } else {
                "months"
            }
        }
        (_, Unit::Week) => {
            if one {
                "week"
            } else {
                "weeks"
            }
        }
        (_, Unit::Day) => {
            if one {
                "day"
            } else {
                "days"
            }
        }
        (_, Unit::Hour) => {
            if one {
                "hour"
            } else {
                "hours"
            }
        }
        (_, Unit::Minute) => {
            if one {
                "minute"
            } else {
                "minutes"
            }
        }
    }
}

/// numeric "auto": the words for one unit back ("yesterday", "last week").
fn last(unit: Unit, lang: &str) -> Option<&'static str> {
    let words = match lang {
        "ko" => ["작년", "지난달", "지난주", "어제"],
        "ja" => ["昨年", "先月", "先週", "昨日"],
        "zh-CN" => ["去年", "上个月", "上周", "昨天"],
        "es" => ["el año pasado", "el mes pasado", "la semana pasada", "ayer"],
        "fil" => [
            "nakaraang taon",
            "nakaraang buwan",
            "nakaraang linggo",
            "kahapon",
        ],
        "vi" => ["năm ngoái", "tháng trước", "tuần trước", "Hôm qua"],
        _ => ["last year", "last month", "last week", "yesterday"],
    };
    match unit {
        Unit::Year => Some(words[0]),
        Unit::Month => Some(words[1]),
        Unit::Week => Some(words[2]),
        Unit::Day => Some(words[3]),
        _ => None,
    }
}

fn relative(n: i64, unit: Unit, lang: &str) -> String {
    if n == -1
        && let Some(w) = last(unit, lang)
    {
        return w.to_string();
    }
    let a = n.abs();
    let w = unit_word(unit, n, lang);
    let past = n < 0;
    match (lang, past) {
        ("ko", true) => format!("{a}{w} 전"),
        ("ko", false) => format!("{a}{w} 후"),
        ("ja", true) => format!("{a} {w}前"),
        ("ja", false) => format!("{a} {w}後"),
        ("zh-CN", true) => format!("{a}{w}前"),
        ("zh-CN", false) => format!("{a}{w}后"),
        ("es", true) => format!("hace {a} {w}"),
        ("es", false) => format!("dentro de {a} {w}"),
        ("fil", true) => format!("{a} {w} ang nakalipas"),
        ("fil", false) => format!("sa loob ng {a} {w}"),
        ("vi", true) => format!("{a} {w} trước"),
        ("vi", false) => format!("sau {a} {w} nữa"),
        (_, true) => format!("{a} {w} ago"),
        (_, false) => format!("in {a} {w}"),
    }
}

/// app.js `ago`: "2 hours ago", "yesterday", "now". `when` and `now` are Unix seconds.
pub fn ago(when: i64, now: i64, lang: &str) -> String {
    if when == 0 {
        return String::new();
    }
    let diff = (when - now) as f64;
    for (unit, size) in UNITS {
        if diff.abs() >= size {
            return relative(js_round(diff / size), unit, lang);
        }
    }
    match lang {
        "ko" => "지금",
        "ja" => "今",
        "zh-CN" => "现在",
        "es" => "ahora",
        "fil" => "ngayon",
        "vi" => "bây giờ",
        _ => "now",
    }
    .to_string()
}

/// Unix seconds of an RFC 3339 UTC time ("2026-11-20T01:12:00Z" or with fractions), or 0.
pub fn parse_rfc3339(s: &str) -> i64 {
    let b = s.as_bytes();
    if b.len() < 19 {
        return 0;
    }
    let num = |r: std::ops::Range<usize>| s.get(r).and_then(|x| x.parse::<i64>().ok());
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(se)) = (
        num(0..4),
        num(5..7),
        num(8..10),
        num(11..13),
        num(14..16),
        num(17..19),
    ) else {
        return 0;
    };
    // Days from civil (Howard Hinnant's algorithm, public domain).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + h * 3600 + mi * 60 + se
}

/// RFC 3339 UTC text for Unix seconds (the inverse of `parse_rfc3339`).
#[cfg(test)]
fn rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil from days (Howard Hinnant's algorithm, public domain).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// A value with a short unit: "52.4 MB" (en), "52.4MB" (ko), "52,4 MB" (es).
fn with_unit(value: String, unit: &str, lang: &str) -> String {
    match lang {
        "ko" | "zh-CN" => format!("{value}{unit}"),
        _ => format!("{value} {unit}"),
    }
}

/// app.js `size`: bytes as GB, MB, kB or bytes.
pub fn size(bytes: u64, lang: &str) -> String {
    let b = bytes as f64;
    for (unit, n) in [("GB", 1e9), ("MB", 1e6), ("kB", 1e3)] {
        if b >= n {
            return with_unit(number(b / n, 1, lang), unit, lang);
        }
    }
    let unit = match lang {
        "ko" => "바이트",
        "zh-CN" => "字节",
        "es" => "B",
        _ => "byte",
    };
    with_unit(bytes.to_string(), unit, lang)
}

/// app.js `mib`: memory in MiB as "512 MB" or "1.5 GB"; "—" for none.
pub fn mib(m: Option<u64>, lang: &str) -> String {
    match m {
        None => "—".into(),
        Some(m) if m >= 1024 => with_unit(number(m as f64 / 1024., 1, lang), "GB", lang),
        Some(m) => with_unit(number(m as f64, 1, lang), "MB", lang),
    }
}

/// app.js `percent`: 12.5 → "12.5%".
pub fn percent(p: f64, lang: &str) -> String {
    let v = number(p, 1, lang);
    match lang {
        "es" => format!("{v} %"),
        _ => format!("{v}%"),
    }
}

fn short_unit(unit: Unit, n: i64, lang: &str) -> String {
    let w = match (lang, unit) {
        ("ko", Unit::Day) => "일",
        ("ko", Unit::Hour) => "시간",
        ("ko", _) => "분",
        ("ja", Unit::Day) => "日",
        ("ja", Unit::Hour) => "時間",
        ("ja", _) => "分",
        ("zh-CN", Unit::Day) => "天",
        ("zh-CN", Unit::Hour) => "小时",
        ("zh-CN", _) => "分钟",
        ("es", Unit::Day) => "d",
        ("es", Unit::Hour) => "h",
        ("es", _) => "min",
        ("fil", Unit::Day) => "araw",
        ("fil", Unit::Hour) => "oras",
        ("fil", _) => "min",
        ("vi", Unit::Day) => "ngày",
        ("vi", Unit::Hour) => "giờ",
        ("vi", _) => "phút",
        (_, Unit::Day) => {
            if n == 1 {
                "day"
            } else {
                "days"
            }
        }
        (_, Unit::Hour) => "hr",
        (_, _) => "min",
    };
    with_unit(n.to_string(), w, lang)
}

/// app.js `duration`: "2 hr 6 min", "3 min", "2 days"; "—" for none.
pub fn duration(secs: Option<u64>, lang: &str) -> String {
    let Some(secs) = secs else {
        return "—".into();
    };
    let s = secs as i64;
    if s >= 86_400 {
        return short_unit(Unit::Day, s / 86_400, lang);
    }
    if s >= 3600 {
        return format!(
            "{} {}",
            short_unit(Unit::Hour, s / 3600, lang),
            short_unit(Unit::Minute, s % 3600 / 60, lang)
        );
    }
    short_unit(Unit::Minute, (s / 60).max(1), lang)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_like_the_browser() {
        let now = 1_800_000_000;
        assert_eq!(ago(now - 7200, now, "en"), "2 hours ago");
        assert_eq!(ago(now - 86_400, now, "en"), "yesterday");
        assert_eq!(ago(now - 30, now, "en"), "now");
        assert_eq!(ago(now - 7200, now, "ko"), "2시간 전");
        assert_eq!(ago(0, now, "en"), "");
    }

    #[test]
    fn sizes_and_memory() {
        assert_eq!(size(52_400_000, "en"), "52.4 MB");
        assert_eq!(size(52_400_000, "ko"), "52.4MB");
        assert_eq!(size(52_400_000, "es"), "52,4 MB");
        assert_eq!(size(512, "en"), "512 byte");
        assert_eq!(mib(Some(1536), "en"), "1.5 GB");
        assert_eq!(mib(None, "en"), "—");
        assert_eq!(percent(12.5, "en"), "12.5%");
        assert_eq!(duration(Some(7560), "en"), "2 hr 6 min");
    }

    #[test]
    fn rfc3339_round_trips() {
        let t = parse_rfc3339("2026-11-20T01:12:00Z");
        assert_eq!(rfc3339(t), "2026-11-20T01:12:00.000Z");
        assert_eq!(parse_rfc3339("x"), 0);
    }
}
