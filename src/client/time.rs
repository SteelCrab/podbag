/// Seconds since the epoch for an RFC 3339 UTC time like `2006-01-02T15:04:05.999999999Z`.
pub fn epoch_of(time: &str) -> Option<f64> {
    let (date, clock) = time.split_once('T')?;
    let mut d = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let clock = clock.strip_suffix('Z')?;
    let (hms, fraction) = clock.split_once('.').unwrap_or((clock, "0"));
    let mut t = hms.splitn(3, ':').map(|p| p.parse::<i64>().ok());
    let (h, min, sec) = (t.next()??, t.next()??, t.next()??);
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let fraction: f64 = format!("0.{fraction}").parse().ok()?;
    Some((days * 86_400 + h * 3600 + min * 60 + sec) as f64 + fraction)
}

/// Docker's human durations (go-units HumanDuration).
pub fn human_duration(secs: u64) -> String {
    let minutes = secs / 60;
    let hours = (secs + 1800) / 3600;
    match secs {
        0 => "Less than a second".to_string(),
        1 => "1 second".to_string(),
        s if s < 60 => format!("{s} seconds"),
        _ if minutes == 1 => "About a minute".to_string(),
        _ if minutes < 60 => format!("{minutes} minutes"),
        _ if hours == 1 => "About an hour".to_string(),
        _ if hours < 48 => format!("{hours} hours"),
        _ if hours < 24 * 7 * 2 => format!("{} days", hours / 24),
        _ if hours < 24 * 30 * 2 => format!("{} weeks", hours / 24 / 7),
        _ if hours < 24 * 365 * 2 => format!("{} months", hours / 24 / 30),
        _ => format!("{} years", secs / 3600 / 24 / 365),
    }
}
