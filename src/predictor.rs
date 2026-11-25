use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Datelike, Duration as ChronoDuration, NaiveDate, TimeZone, Timelike, Utc, Weekday};
use chrono_tz::Asia::Shanghai;
use influxdb2::models::DataPoint;
use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

pub const MODEL_NAME: &str = "uncensored_shift_ewm_v1";
pub const TARGET_LOCATION: &str = "SIP-B25-B26";

pub const START_MIN: usize = 7 * 60;      // 07:00
pub const REF_START_IDX: usize = 30;      // 07:30
pub const REF_END_IDX: usize = 140;       // 09:20 (pre-saturation reference window)
pub const END_MIN: usize = 12 * 60 + 30;  // 12:30
pub const GRID_LEN: usize = END_MIN - START_MIN + 1;

pub const HISTORY_LOOKBACK_DAYS: i64 = 90;
pub const HALF_LIFE_DAYS: f64 = 42.0;
pub const BACKTEST_EVAL_DAYS: usize = 20;
pub const BACKTEST_MIN_TRAIN_DAYS: usize = 15;
pub const BACKTEST_CHECKPOINTS: [usize; 4] = [8 * 60 + 30, 9 * 60, 9 * 60 + 15, 9 * 60 + 30];

const INFLUX_QUERY_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Debug, Clone)]
pub struct DayCurve {
    pub date: NaiveDate,
    pub weekday: Weekday,
    pub curve: Vec<f64>,         // causal 3-min median smoothed, len = GRID_LEN
    pub full_min: Option<usize>, // first minute <= 0 in raw series
}

#[derive(Debug, Clone)]
pub struct GroupTemplate {
    pub cum_demand: Vec<f64>,    // len = GRID_LEN, cumulative uncensored demand from 07:30
    pub sample_days: usize,
    pub member_scales: Vec<(f64, f64)>, // (relative_scale, time_weight) for empirical uncertainty
}

#[derive(Debug, Clone)]
pub struct PredictorModel {
    pub built_date: NaiveDate,
    pub total_workdays: usize,
    pub groups: [GroupTemplate; 3], // 0: Mon-Wed, 1: Thu, 2: Fri
}

#[derive(Debug, Clone)]
pub struct Prediction {
    pub will_fill_prob: f64,
    pub is_full: bool,
    pub predicted_full_min: Option<f64>,
    pub p20_full_min: Option<f64>,
    pub p80_full_min: Option<f64>,
    pub remaining_mins: Option<f64>,
    pub smoothed_free: f64,
}

#[derive(Debug, Clone)]
pub struct BacktestCheckpointReport {
    pub checkpoint_label: String,
    pub sample_days: usize,
    pub filled_days: usize,
    pub mae_mins: f64,
    pub medae_mins: f64,
    pub bias_mins: f64, // mean(pred - actual); persistent positive bias indicates faster fill (e.g. RTO4)
    pub p90_abs_err_mins: f64,
    pub fill_acc: f64,
}

#[derive(Debug, Default)]
pub struct LiveTracker {
    pub active_date: Option<NaiveDate>,
    pub recent_samples: VecDeque<(i64, f64)>, // (unix_secs, free_spaces), last 3 minutes
    pub locked_full_min: Option<usize>,
}

impl LiveTracker {
    pub fn record(&mut self, now_shanghai: DateTime<chrono_tz::Tz>, free_spaces: i64) -> f64 {
        let today = now_shanghai.date_naive();
        if self.active_date != Some(today) {
            self.active_date = Some(today);
            self.recent_samples.clear();
            self.locked_full_min = None;
        }
        let ts = now_shanghai.timestamp();
        self.recent_samples.push_back((ts, free_spaces as f64));
        while let Some(&(old_ts, _)) = self.recent_samples.front() {
            if ts - old_ts > 180 {
                self.recent_samples.pop_front();
            } else {
                break;
            }
        }
        let mut vals: Vec<f64> = self.recent_samples.iter().map(|&(_, v)| v).collect();
        let med = median_in_place(&mut vals).unwrap_or(free_spaces as f64);

        let min_of_day = (now_shanghai.hour() * 60 + now_shanghai.minute()) as usize;
        if free_spaces <= 0 && self.locked_full_min.is_none() && min_of_day >= START_MIN {
            self.locked_full_min = Some(min_of_day);
        }
        med
    }
}

pub fn dow_group(wd: Weekday) -> usize {
    match wd {
        Weekday::Mon | Weekday::Tue | Weekday::Wed => 0,
        Weekday::Thu => 1,
        Weekday::Fri => 2,
        Weekday::Sat | Weekday::Sun => 2,
    }
}

pub fn is_weekday(wd: Weekday) -> bool {
    !matches!(wd, Weekday::Sat | Weekday::Sun)
}

fn median_in_place(vals: &mut [f64]) -> Option<f64> {
    if vals.is_empty() {
        return None;
    }
    vals.sort_by(|a, b| a.total_cmp(b));
    let n = vals.len();
    if n % 2 == 1 {
        Some(vals[n / 2])
    } else {
        Some(0.5 * (vals[n / 2 - 1] + vals[n / 2]))
    }
}

fn weighted_quantile(vals: &[f64], weights: &[f64], q: f64) -> Option<f64> {
    if vals.is_empty() || vals.len() != weights.len() {
        return None;
    }
    let mut pairs: Vec<(f64, f64)> = vals
        .iter()
        .copied()
        .zip(weights.iter().copied())
        .filter(|(v, w)| v.is_finite() && w.is_finite() && *w > 0.0)
        .collect();
    if pairs.is_empty() {
        return None;
    }
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
    let total_w: f64 = pairs.iter().map(|(_, w)| *w).sum();
    if total_w <= 0.0 {
        return None;
    }
    let q = q.clamp(0.0, 1.0);
    // Midpoint CDF: c_i = (cum_before + 0.5 * w_i) / total_w, unbiased for discrete & continuous samples.
    let mut cum_before = 0.0;
    let mut prev_c = 0.0;
    let mut prev_v = pairs[0].0;
    for (i, &(v, w)) in pairs.iter().enumerate() {
        let c = (cum_before + 0.5 * w) / total_w;
        if i == 0 && q <= c {
            return Some(v);
        }
        if i > 0 && q <= c {
            let denom = c - prev_c;
            if denom <= 1e-12 {
                return Some(v);
            }
            let frac = (q - prev_c) / denom;
            return Some(prev_v + frac * (v - prev_v));
        }
        prev_c = c;
        prev_v = v;
        cum_before += w;
    }
    Some(pairs.last().unwrap().0)
}

fn percentile_in_place(vals: &mut [f64], p: f64) -> Option<f64> {
    if vals.is_empty() {
        return None;
    }
    vals.sort_by(|a, b| a.total_cmp(b));
    let idx = p.clamp(0.0, 1.0) * (vals.len() - 1) as f64;
    let lo = idx.floor() as usize;
    let hi = idx.ceil() as usize;
    if lo == hi {
        Some(vals[lo])
    } else {
        Some(vals[lo] + (idx - lo as f64) * (vals[hi] - vals[lo]))
    }
}

/// Parse InfluxDB Flux annotated CSV (columns `_time`, `_value`) into `(UTC DateTime, f64)`.
pub fn parse_flux_csv(csv_text: &str) -> Vec<(DateTime<Utc>, f64)> {
    let mut out = Vec::new();
    let mut time_col: Option<usize> = None;
    let mut val_col: Option<usize> = None;

    for raw_line in csv_text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let cols: Vec<&str> = line.split(',').collect();
        if cols.iter().any(|&c| c == "_time") && cols.iter().any(|&c| c == "_value") {
            time_col = cols.iter().position(|&c| c == "_time");
            val_col = cols.iter().position(|&c| c == "_value");
            continue;
        }
        if let (Some(tc), Some(vc)) = (time_col, val_col)
            && cols.len() > tc
            && cols.len() > vc
            && let (Ok(dt), Ok(v)) = (
                DateTime::parse_from_rfc3339(cols[tc]),
                cols[vc].parse::<f64>(),
            )
        {
            out.push((dt.with_timezone(&Utc), v));
        }
    }
    out
}

/// Fetch 1-minute downsampled history for `TARGET_LOCATION` over the past `lookback_days`.
/// Chunks into 30-day windows so each Flux query stays fast and bounded.
pub async fn fetch_history_curves(
    http: &reqwest::Client,
    influx_url: &str,
    org: &str,
    bucket: &str,
    token: &str,
    lookback_days: i64,
) -> Result<Vec<DayCurve>> {
    let query_url = format!("{}/api/v2/query", influx_url.trim_end_matches('/'));
    let mut raw_points: Vec<(DateTime<Utc>, f64)> = Vec::new();

    let mut start_d = lookback_days;
    while start_d > 0 {
        let stop_d = (start_d - 30).max(0);
        let stop_expr = if stop_d > 0 {
            format!("-{}d", stop_d)
        } else {
            "now()".to_string()
        };
        let flux = format!(
            r#"from(bucket: "{bucket}")
  |> range(start: -{start_d}d, stop: {stop_expr})
  |> filter(fn: (r) => r._measurement == "parking_spaces" and r._field == "free_spaces" and r.location == "{TARGET_LOCATION}")
  |> aggregateWindow(every: 1m, fn: min, createEmpty: false)
  |> keep(columns: ["_time", "_value"])"#
        );

        let resp = tokio::time::timeout(
            INFLUX_QUERY_TIMEOUT,
            http.post(&query_url)
                .query(&[("org", org)])
                .header("Authorization", format!("Token {token}"))
                .header("Content-Type", "application/vnd.flux")
                .header("Accept", "application/csv")
                .body(flux)
                .send(),
        )
        .await
        .map_err(|_| anyhow!("InfluxDB query timed out (-{start_d}d..{stop_expr})"))?
        .context("Failed to send InfluxDB Flux query")?
        .error_for_status()
        .context("InfluxDB Flux query returned HTTP error")?;

        let body = tokio::time::timeout(INFLUX_QUERY_TIMEOUT, resp.text())
            .await
            .map_err(|_| anyhow!("Timed out reading InfluxDB query body"))?
            .context("Failed to read InfluxDB query body")?;

        raw_points.extend(parse_flux_csv(&body));
        start_d = stop_d;
    }

    Ok(build_day_curves(&raw_points))
}

/// Convert raw `(UTC timestamp, free_spaces)` samples into cleaned non-holiday workday curves.
pub fn build_day_curves(points: &[(DateTime<Utc>, f64)]) -> Vec<DayCurve> {
    let mut by_date: BTreeMap<NaiveDate, Vec<(usize, f64)>> = BTreeMap::new();
    for &(utc_ts, val) in points {
        if !val.is_finite() {
            continue;
        }
        let sh = utc_ts.with_timezone(&Shanghai);
        let wd = sh.weekday();
        if !is_weekday(wd) {
            continue;
        }
        let m = (sh.hour() * 60 + sh.minute()) as usize;
        if (START_MIN..=END_MIN).contains(&m) {
            by_date.entry(sh.date_naive()).or_default().push((m - START_MIN, val));
        }
    }

    let mut days = Vec::new();
    for (date, entries) in by_date {
        let mut grid: Vec<Option<f64>> = vec![None; GRID_LEN];
        for (idx, val) in entries {
            grid[idx] = Some(match grid[idx] {
                Some(prev) => prev.min(val),
                None => val,
            });
        }
        let valid_count = grid.iter().filter(|x| x.is_some()).count();
        if valid_count < 200 {
            continue;
        }
        let Some(raw) = interpolate_grid(&grid, 10, 5) else {
            continue;
        };

        // Causal 3-minute median despike (no future leakage)
        let mut smoothed = vec![0.0; GRID_LEN];
        for i in 0..GRID_LEN {
            let lo = i.saturating_sub(2);
            let mut win = raw[lo..=i].to_vec();
            smoothed[i] = median_in_place(&mut win).unwrap_or(raw[i]);
        }

        let min_free = raw.iter().copied().fold(f64::INFINITY, f64::min);
        let drop_total = smoothed[REF_START_IDX] - min_free;
        if drop_total < 150.0 {
            continue; // skip holidays / dead days
        }

        let full_min = raw
            .iter()
            .position(|&v| v <= 0.0)
            .map(|idx| START_MIN + idx);

        days.push(DayCurve {
            date,
            weekday: date.weekday(),
            curve: smoothed,
            full_min,
        });
    }
    days
}

fn interpolate_grid(grid: &[Option<f64>], max_gap: usize, max_edge: usize) -> Option<Vec<f64>> {
    let n = grid.len();
    let mut out = vec![f64::NAN; n];
    let mut last_idx: Option<usize> = None;

    for i in 0..n {
        if let Some(v) = grid[i] {
            out[i] = v;
            if let Some(prev) = last_idx {
                let gap = i - prev - 1;
                if gap > 0 && gap <= max_gap {
                    let v0 = out[prev];
                    for k in 1..=gap {
                        out[prev + k] = v0 + (v - v0) * (k as f64 / (gap + 1) as f64);
                    }
                }
            } else if i <= max_edge {
                for item in out.iter_mut().take(i) {
                    *item = v;
                }
            }
            last_idx = Some(i);
        }
    }
    if let Some(last) = last_idx
        && n - 1 - last <= max_edge
    {
        let v = out[last];
        for item in out.iter_mut().skip(last + 1) {
            *item = v;
        }
    }
    let missing = out.iter().filter(|v| v.is_nan()).count();
    if missing > 10 {
        return None;
    }
    // Fill any tiny remaining NaN conservatively by carrying nearest valid value
    let mut carry = out.iter().copied().find(|v| !v.is_nan())?;
    for v in &mut out {
        if v.is_nan() {
            *v = carry;
        } else {
            carry = *v;
        }
    }
    Some(out)
}

fn compute_time_weights_and_occ(days: &[&DayCurve], ref_date: NaiveDate) -> (Vec<f64>, Vec<f64>, f64) {
    let ln2 = std::f64::consts::LN_2;
    let weights: Vec<f64> = days
        .iter()
        .map(|d| {
            let age = (ref_date - d.date).num_days().max(0) as f64;
            (-ln2 * age / HALF_LIFE_DAYS).exp()
        })
        .collect();
    let occ_refs: Vec<f64> = days
        .iter()
        .map(|d| (d.curve[REF_START_IDX] - d.curve[REF_END_IDX]).max(20.0))
        .collect();
    let med_ref = weighted_quantile(&occ_refs, &weights, 0.5)
        .unwrap_or(100.0)
        .max(20.0);
    (weights, occ_refs, med_ref)
}

fn build_group_template(
    days: &[&DayCurve],
    fallback_days: &[&DayCurve],
    ref_date: NaiveDate,
) -> GroupTemplate {
    let n = days.len();
    if n == 0 {
        return GroupTemplate {
            cum_demand: vec![0.0; GRID_LEN],
            sample_days: 0,
            member_scales: Vec::new(),
        };
    }

    let (weights, occ_refs, med_ref) = compute_time_weights_and_occ(days, ref_date);
    let day_scales: Vec<f64> = occ_refs.iter().map(|&o| (o / med_ref).clamp(0.4, 2.0)).collect();
    let (fb_weights, fb_occ_refs, _) = compute_time_weights_and_occ(fallback_days, ref_date);

    let mut r_raw = vec![0.0; GRID_LEN];
    for t in 1..GRID_LEN {
        let mut step_vals = Vec::new();
        let mut step_w = Vec::new();
        for (i, d) in days.iter().enumerate() {
            if d.curve[t - 1] > 5.0 && d.curve[t] > 5.0 {
                let drop = (d.curve[t - 1] - d.curve[t]) / day_scales[i];
                step_vals.push(drop);
                step_w.push(weights[i]);
            }
        }
        if step_vals.len() >= 3 {
            r_raw[t] = weighted_quantile(&step_vals, &step_w, 0.5).unwrap_or(0.0);
        } else {
            // Fallback to all-weekday shape scaled to THIS group's amplitude (med_ref)
            let mut fb_vals = Vec::new();
            let mut fb_w = Vec::new();
            for (i, d) in fallback_days.iter().enumerate() {
                if d.curve[t - 1] > 5.0 && d.curve[t] > 5.0 {
                    let fb_scale = (fb_occ_refs[i] / med_ref).clamp(0.4, 2.0);
                    let drop = (d.curve[t - 1] - d.curve[t]) / fb_scale;
                    fb_vals.push(drop);
                    fb_w.push(fb_weights[i]);
                }
            }
            if fb_vals.len() >= 3 {
                r_raw[t] = weighted_quantile(&fb_vals, &fb_w, 0.5).unwrap_or(0.0);
            }
        }
    }

    // 7-minute centered moving average smoothing on historical template
    let mut r_smooth = vec![0.0; GRID_LEN];
    for t in 0..GRID_LEN {
        let lo = t.saturating_sub(3);
        let hi = (t + 3).min(GRID_LEN - 1);
        let sum: f64 = r_raw[lo..=hi].iter().sum();
        r_smooth[t] = sum / (hi - lo + 1) as f64;
    }

    let mut cum_demand = vec![0.0; GRID_LEN];
    let mut acc = 0.0;
    for t in REF_START_IDX..GRID_LEN {
        acc += r_smooth[t];
        cum_demand[t] = acc;
    }

    let member_scales: Vec<(f64, f64)> = day_scales.into_iter().zip(weights).collect();
    GroupTemplate {
        cum_demand,
        sample_days: n,
        member_scales,
    }
}

pub fn train_model(days: &[DayCurve], ref_date: NaiveDate) -> Option<PredictorModel> {
    let recent: Vec<&DayCurve> = days
        .iter()
        .filter(|d| d.date < ref_date && (ref_date - d.date).num_days() <= HISTORY_LOOKBACK_DAYS)
        .collect();
    if recent.len() < 5 {
        return None;
    }

    let mut groups: Vec<GroupTemplate> = Vec::with_capacity(3);
    for g in 0..3 {
        let sub: Vec<&DayCurve> = recent
            .iter()
            .copied()
            .filter(|d| dow_group(d.weekday) == g)
            .collect();
        let tpl = if sub.len() >= 4 {
            build_group_template(&sub, &recent, ref_date)
        } else {
            build_group_template(&recent, &recent, ref_date)
        };
        groups.push(tpl);
    }

    Some(PredictorModel {
        built_date: ref_date,
        total_workdays: recent.len(),
        groups: groups.try_into().ok()?,
    })
}

fn first_zero_offset(s_now: f64, cum: &[f64], t_i: usize, scale: f64) -> Option<f64> {
    let base = cum[t_i];
    let mut prev_proj = s_now;
    for k in 1..(GRID_LEN - t_i) {
        let proj = s_now - scale * (cum[t_i + k] - base);
        if proj <= 0.0 {
            let denom = prev_proj - proj;
            let frac = if denom > 1e-6 {
                (prev_proj / denom).clamp(0.0, 1.0)
            } else {
                0.0
            };
            return Some((k - 1) as f64 + frac);
        }
        prev_proj = proj;
    }
    None
}

impl PredictorModel {
    pub fn predict_at(
        &self,
        weekday: Weekday,
        min_of_day: usize,
        s_now: f64,
        locked_full_min: Option<usize>,
    ) -> Option<Prediction> {
        if !(START_MIN..=END_MIN).contains(&min_of_day) {
            return None;
        }
        if let Some(fm) = locked_full_min {
            return Some(Prediction {
                will_fill_prob: 1.0,
                is_full: true,
                predicted_full_min: Some(fm as f64),
                p20_full_min: Some(fm as f64),
                p80_full_min: Some(fm as f64),
                remaining_mins: Some(0.0),
                smoothed_free: s_now.max(0.0),
            });
        }
        if s_now <= 0.0 {
            return Some(Prediction {
                will_fill_prob: 1.0,
                is_full: true,
                predicted_full_min: Some(min_of_day as f64),
                p20_full_min: Some(min_of_day as f64),
                p80_full_min: Some(min_of_day as f64),
                remaining_mins: Some(0.0),
                smoothed_free: 0.0,
            });
        }

        let grp = &self.groups[dow_group(weekday)];
        let t_i = min_of_day - START_MIN;

        let point_offset = first_zero_offset(s_now, &grp.cum_demand, t_i, 1.0);
        let point_min = point_offset.map(|off| min_of_day as f64 + off);

        // Ensemble over group's historical relative scales for will_fill_prob and P20-P80 band
        let mut hit_mins = Vec::new();
        let mut hit_w = Vec::new();
        let mut total_w = 0.0;
        let mut fill_w = 0.0;

        for &(scale, w) in &grp.member_scales {
            total_w += w;
            if let Some(off) = first_zero_offset(s_now, &grp.cum_demand, t_i, scale) {
                fill_w += w;
                hit_mins.push(min_of_day as f64 + off);
                hit_w.push(w);
            }
        }

        let will_fill_prob = if total_w > 0.0 {
            (fill_w / total_w).clamp(0.0, 1.0)
        } else if point_min.is_some() {
            1.0
        } else {
            0.0
        };

        let p20 = weighted_quantile(&hit_mins, &hit_w, 0.2);
        let p50_ens = weighted_quantile(&hit_mins, &hit_w, 0.5);
        let p80 = weighted_quantile(&hit_mins, &hit_w, 0.8);

        let pred_min = point_min.or(p50_ens);
        let remaining_mins = pred_min.map(|m| (m - min_of_day as f64).max(0.0));

        Some(Prediction {
            will_fill_prob,
            is_full: false,
            predicted_full_min: pred_min,
            p20_full_min: p20,
            p80_full_min: p80,
            remaining_mins,
            smoothed_free: s_now,
        })
    }
}

pub fn min_of_day_to_epoch(date: NaiveDate, min_of_day: f64) -> Option<i64> {
    let midnight = date.and_hms_opt(0, 0, 0)?;
    let local_mid = Shanghai.from_local_datetime(&midnight).single()?;
    let secs = (min_of_day * 60.0).round() as i64;
    Some((local_mid + ChronoDuration::seconds(secs)).timestamp())
}

pub fn format_min_of_day(m: f64) -> String {
    let total = m.round().max(0.0) as usize;
    format!("{:02}:{:02}", total / 60, total % 60)
}

pub fn prediction_to_datapoint(
    now_shanghai: DateTime<chrono_tz::Tz>,
    pred: &Prediction,
) -> Result<DataPoint> {
    let today = now_shanghai.date_naive();
    let ts_ns = now_shanghai
        .with_timezone(&Utc)
        .timestamp_nanos_opt()
        .ok_or_else(|| anyhow!("timestamp out of range"))?;

    let mut b = DataPoint::builder("parking_prediction")
        .tag("location", TARGET_LOCATION)
        .tag("model", MODEL_NAME)
        .field("will_fill_prob", pred.will_fill_prob)
        .field("is_full", if pred.is_full { 1_i64 } else { 0_i64 })
        .field("smoothed_free", pred.smoothed_free);

    if let Some(m) = pred.predicted_full_min {
        b = b.field("predicted_full_min", m);
        if let Some(ep) = min_of_day_to_epoch(today, m) {
            b = b.field("predicted_full_epoch", ep);
        }
    }
    if let Some(m) = pred.p20_full_min
        && let Some(ep) = min_of_day_to_epoch(today, m)
    {
        b = b.field("p20_full_epoch", ep);
    }
    if let Some(m) = pred.p80_full_min
        && let Some(ep) = min_of_day_to_epoch(today, m)
    {
        b = b.field("p80_full_epoch", ep);
    }
    if let Some(rem) = pred.remaining_mins {
        b = b.field("remaining_mins", rem);
    }

    b.timestamp(ts_ns)
        .build()
        .map_err(|e| anyhow!("Failed to build prediction DataPoint: {e}"))
}

/// Strict causal rolling backtest over the most recent `eval_days` non-holiday workdays.
/// Each evaluated day only sees strictly earlier days (`d.date < today`).
pub fn run_causal_backtest(days: &[DayCurve], eval_days: usize) -> Vec<BacktestCheckpointReport> {
    if days.len() <= BACKTEST_MIN_TRAIN_DAYS {
        return Vec::new();
    }
    let start_idx = days
        .len()
        .saturating_sub(eval_days)
        .max(BACKTEST_MIN_TRAIN_DAYS);

    let mut reports = Vec::new();
    for &cp in &BACKTEST_CHECKPOINTS {
        let t_i = cp - START_MIN;
        let mut abs_errs = Vec::new();
        let mut signed_errs = Vec::new();
        let mut fill_hits = 0usize;
        let mut fill_total = 0usize;

        for idx in start_idx..days.len() {
            let today = &days[idx];
            let s_now = today.curve[t_i];
            if s_now <= 0.0 {
                continue;
            }
            let Some(model) = train_model(&days[..idx], today.date) else {
                continue;
            };
            let Some(pred) = model.predict_at(today.weekday, cp, s_now, None) else {
                continue;
            };

            let actual_filled = today.full_min.is_some();
            fill_total += 1;
            if (pred.will_fill_prob >= 0.5) == actual_filled {
                fill_hits += 1;
            }

            if let (Some(actual_m), Some(pred_m)) = (today.full_min, pred.predicted_full_min)
                && actual_m > cp
            {
                let err = pred_m - actual_m as f64;
                signed_errs.push(err);
                abs_errs.push(err.abs());
            }
        }

        if fill_total == 0 {
            continue;
        }
        let filled_days = abs_errs.len();
        let mae_mins = if filled_days > 0 {
            abs_errs.iter().sum::<f64>() / filled_days as f64
        } else {
            0.0
        };
        let bias_mins = if filled_days > 0 {
            signed_errs.iter().sum::<f64>() / filled_days as f64
        } else {
            0.0
        };
        let medae_mins = median_in_place(&mut abs_errs.clone()).unwrap_or(0.0);
        let p90_abs_err_mins = percentile_in_place(&mut abs_errs.clone(), 0.90).unwrap_or(0.0);
        let fill_acc = fill_hits as f64 / fill_total as f64;

        reports.push(BacktestCheckpointReport {
            checkpoint_label: format_min_of_day(cp as f64),
            sample_days: fill_total,
            filled_days,
            mae_mins,
            medae_mins,
            bias_mins,
            p90_abs_err_mins,
            fill_acc,
        });
    }
    reports
}

pub fn backtest_to_datapoints(
    now_utc: DateTime<Utc>,
    reports: &[BacktestCheckpointReport],
) -> Result<Vec<DataPoint>> {
    let ts_ns = now_utc
        .timestamp_nanos_opt()
        .ok_or_else(|| anyhow!("timestamp out of range"))?;
    let mut pts = Vec::with_capacity(reports.len());
    for r in reports {
        let dp = DataPoint::builder("parking_prediction_backtest")
            .tag("location", TARGET_LOCATION)
            .tag("model", MODEL_NAME)
            .tag("checkpoint", r.checkpoint_label.as_str())
            .field("mae_mins", r.mae_mins)
            .field("medae_mins", r.medae_mins)
            .field("bias_mins", r.bias_mins)
            .field("p90_abs_err_mins", r.p90_abs_err_mins)
            .field("fill_acc", r.fill_acc)
            .field("sample_days", r.sample_days as i64)
            .field("filled_days", r.filled_days as i64)
            .timestamp(ts_ns)
            .build()
            .map_err(|e| anyhow!("Failed to build backtest DataPoint: {e}"))?;
        pts.push(dp);
    }
    Ok(pts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weighted_quantile_is_unbiased_for_equal_weights() {
        let m = weighted_quantile(&[0.0, 4.0, 6.0], &[1.0, 1.0, 1.0], 0.5).unwrap();
        assert!((m - 4.0).abs() < 1e-9, "got {m}");
        let m2 = weighted_quantile(&[1.0, 3.0, 5.0, 7.0], &[1.0, 1.0, 1.0, 1.0], 0.5).unwrap();
        assert!((m2 - 4.0).abs() < 1e-9, "got {m2}");
    }

    #[test]
    fn parses_flux_annotated_csv() {
        let sample = "\
#group,false,false,true,true
#datatype,string,long,dateTime:RFC3339,double
#default,_result,,,
,result,table,_time,_value
,,0,2026-11-25T00:30:00Z,468
,,0,2026-11-25T01:00:00.123456Z,350.5

,result,table,_time,_value
,,1,2026-11-25T01:30:00Z,62
";
        let pts = parse_flux_csv(sample);
        assert_eq!(pts.len(), 3);
        assert_eq!(pts[0].1, 468.0);
        assert_eq!(pts[1].1, 350.5);
        assert_eq!(pts[2].1, 62.0);
    }

    #[test]
    fn synthetic_shift_prediction_tracks_remaining_time() {
        // Synthetic weekday curves: linear drop of 4 cars/min starting from 480 at 07:30 -> full at 09:30 (570)
        let mut days = Vec::new();
        let base_date = NaiveDate::from_ymd_opt(2026, 9, 7).unwrap(); // Monday
        for w in 0..10 {
            let dt = base_date + ChronoDuration::days(w * 7);
            let mut curve = vec![480.0; GRID_LEN];
            for t in REF_START_IDX..GRID_LEN {
                let elapsed = (t - REF_START_IDX) as f64;
                curve[t] = (480.0 - 4.0 * elapsed).max(0.0);
            }
            days.push(DayCurve {
                date: dt,
                weekday: dt.weekday(),
                curve,
                full_min: Some(START_MIN + REF_START_IDX + 120),
            });
        }
        let target_date = base_date + ChronoDuration::days(10 * 7);
        let model = train_model(&days, target_date).expect("model should train");

        // At 09:00 (540), if 100 cars left at 4/min -> should predict ~09:25 (565)
        let pred = model
            .predict_at(Weekday::Mon, 9 * 60, 100.0, None)
            .expect("prediction");
        let fm = pred.predicted_full_min.expect("should predict full");
        assert!((fm - 565.0).abs() <= 2.5, "got {fm}");
        assert!(pred.will_fill_prob > 0.9);
    }

    #[test]
    fn live_tracker_despikes_and_locks_full_time() {
        let mut tracker = LiveTracker::default();
        let t0 = Shanghai.with_ymd_and_hms(2026, 11, 25, 9, 30, 0).unwrap();
        let t1 = Shanghai.with_ymd_and_hms(2026, 11, 25, 9, 30, 30).unwrap();
        let t2 = Shanghai.with_ymd_and_hms(2026, 11, 25, 9, 31, 0).unwrap();
        assert_eq!(tracker.record(t0, 60), 60.0);
        // Single-tick upward glitch to 490 shouldn't pull median above 60 once next sample arrives
        let _ = tracker.record(t1, 490);
        assert_eq!(tracker.record(t2, 56), 60.0);
        assert_eq!(tracker.locked_full_min, None);

        let t3 = Shanghai.with_ymd_and_hms(2026, 11, 25, 9, 37, 0).unwrap();
        let _ = tracker.record(t3, 0);
        assert_eq!(tracker.locked_full_min, Some(9 * 60 + 37));
    }
}
