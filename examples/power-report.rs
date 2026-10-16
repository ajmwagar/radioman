//! Summarize rx_power/rtl_power CSV without interpreting peaks as decoded signals.
//! Usage: cargo run --example power-report -- SCAN.csv [CHANNEL_HZ ...]
use std::{collections::BTreeMap, env, fs};

fn parse(text: &str) -> Result<BTreeMap<i64, f64>, String> {
    let mut bins: BTreeMap<i64, (f64, usize)> = BTreeMap::new();
    for (line, row) in text.lines().enumerate() {
        let fields: Vec<_> = row.split(',').map(str::trim).collect();
        if fields.len() < 7 {
            return Err(format!("line {}: missing power bins", line + 1));
        }
        let number = |i: usize| {
            fields[i]
                .parse::<f64>()
                .map_err(|_| format!("line {}: invalid number", line + 1))
        };
        let low = number(2)?;
        let high = number(3)?;
        let step = number(4)?;
        if !low.is_finite() || !high.is_finite() || !step.is_finite() || step <= 0.0 || high <= low
        {
            return Err(format!("line {}: invalid frequency span", line + 1));
        }
        for i in 6..fields.len() {
            let power = number(i)?;
            if !power.is_finite() {
                return Err(format!("line {}: nonfinite power", line + 1));
            }
            let hz = low + (i - 6) as f64 * step;
            if hz > high + step {
                return Err("bins exceed declared span".into());
            }
            let entry = bins.entry(hz.round() as i64).or_default();
            entry.0 += 10_f64.powf(power / 10.0);
            entry.1 += 1;
        }
    }
    if bins.is_empty() {
        return Err("no spectrum rows".into());
    }
    Ok(bins
        .into_iter()
        .map(|(hz, (sum, count))| (hz, 10.0 * (sum / count as f64).log10()))
        .collect())
}
fn median(mut values: Vec<f64>) -> Option<f64> {
    values.sort_by(f64::total_cmp);
    values.get(values.len() / 2).copied()
}
fn main() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let path = args.next().ok_or("expected CSV path")?;
    if fs::metadata(&path).map_err(|e| e.to_string())?.len() > 8 * 1024 * 1024 {
        return Err("CSV exceeds 8 MiB".into());
    }
    let bins = parse(&fs::read_to_string(path).map_err(|e| e.to_string())?)?;
    println!(
        "{} averaged bins; relative tool units, not calibrated dBm",
        bins.len()
    );
    let floor = median(bins.values().copied().collect()).unwrap();
    let mut peaks: Vec<_> = bins.iter().collect();
    peaks.sort_by(|a, b| b.1.total_cmp(a.1));
    let mut chosen: Vec<i64> = Vec::new();
    for (&hz, &power) in peaks {
        if chosen.iter().any(|previous| (previous - hz).abs() < 20000) {
            continue;
        }
        println!(
            "Peak {:.6} MHz: {:.1} dB, {:.1} dB above band median",
            hz as f64 / 1e6,
            power,
            power - floor
        );
        chosen.push(hz);
        if chosen.len() == 10 {
            break;
        }
    }
    for target in args {
        let hz = target.parse::<i64>().map_err(|e| e.to_string())?;
        let signal = bins
            .iter()
            .filter(|(f, _)| (**f - hz).abs() <= 8000)
            .map(|(_, v)| *v)
            .max_by(f64::total_cmp)
            .ok_or("channel outside scan")?;
        let background = median(
            bins.iter()
                .filter(|(f, _)| (15000..=22000).contains(&(**f - hz).abs()))
                .map(|(_, v)| *v)
                .collect(),
        )
        .ok_or("missing channel sidebands")?;
        println!(
            "Channel {:.3} MHz: peak {:.1} dB above nearby median (spectral contrast, not demodulated SNR)",
            hz as f64 / 1e6,
            signal - background
        );
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn averages_in_linear_power() {
        let b = parse("d,t,100,102,1,2,0,10\nd,t,100,102,1,2,10,0").unwrap();
        assert!((b[&100] - 7.4036269).abs() < 1e-6);
        assert_eq!(b[&100], b[&101]);
    }
    #[test]
    fn rejects_missing_or_invalid_data() {
        for v in ["", "d,t,100,102,0,2,1", "d,t,100,102,1,2,NaN", "partial"] {
            assert!(parse(v).is_err());
        }
    }
}
