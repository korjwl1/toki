use std::collections::HashMap;

use chrono::{NaiveDateTime, TimeZone, Datelike, Weekday};
use chrono_tz::Tz;

use crate::common::types::{ModelUsageSummary, RawEvent, GroupedSummaryMap, SummaryMap};
use crate::db::Database;
use crate::engine::{ReportFilter, ReportGroupBy};
use crate::query_parser::{AggregationFunc, LabelFilter};

/// Token type names corresponding to the 4 token slots.
/// Slot 0 = input, 1 = output, 2 = cache_create, 3 = cache_read.
const TOKEN_TYPE_NAMES: &[&str] = &["input", "output", "cache_create", "cache_read"];

/// Check if a token type name matches a LabelFilter (exact or regex via `|` alternation).
fn type_matches(type_name: &str, filter: &LabelFilter) -> bool {
    if filter.regex {
        // Support simple `|`-separated alternation (e.g. "input|output")
        filter.value.split('|').any(|alt| alt == type_name)
    } else {
        filter.value == type_name
    }
}

/// Given a type filter, return a 4-element mask [input, output, cache_create, cache_read]
/// indicating which token slots to include.
fn type_filter_mask(filter: Option<&LabelFilter>) -> [bool; 4] {
    match filter {
        None => [true; 4],
        Some(f) => {
            let mut mask = [false; 4];
            for (i, &name) in TOKEN_TYPE_NAMES.iter().enumerate() {
                mask[i] = type_matches(name, f);
            }
            mask
        }
    }
}

/// Apply a type filter mask to a ModelUsageSummary, zeroing out non-matching token fields.
fn apply_type_mask(summary: &mut ModelUsageSummary, mask: &[bool; 4]) {
    if !mask[0] { summary.input_tokens = 0; }
    if !mask[1] { summary.output_tokens = 0; }
    if !mask[2] { summary.cache_creation_input_tokens = 0; }
    if !mask[3] { summary.cache_read_input_tokens = 0; }
}

/// Resolve (since, until) from filter into ms timestamps.
fn filter_range(filter: ReportFilter) -> (i64, i64) {
    let since = filter_to_ms(filter.since).unwrap_or(0);
    let until = filter_to_ms(filter.until).unwrap_or(i64::MAX);
    (since, until)
}

fn accumulate_rollup(entry: &mut ModelUsageSummary, rollup: &crate::common::types::RollupValue) {
    entry.input_tokens += rollup.input;
    entry.output_tokens += rollup.output;
    entry.cache_creation_input_tokens += rollup.cache_create;
    entry.cache_read_input_tokens += rollup.cache_read;
    entry.event_count += rollup.count;
}

/// Accumulate a StoredEvent's token counts into a ModelUsageSummary.
fn accumulate_event(entry: &mut ModelUsageSummary, event: &crate::common::types::StoredEvent) {
    entry.input_tokens += event.input_tokens;
    entry.output_tokens += event.output_tokens;
    entry.cache_creation_input_tokens += event.cache_creation_input_tokens;
    entry.cache_read_input_tokens += event.cache_read_input_tokens;
    entry.event_count += 1;
}

/// Report total summary from TSDB rollups (streaming — no intermediate Vec).
pub fn report_summary_from_db(
    db: &Database,
    filter: ReportFilter,
) -> Result<SummaryMap, fjall::Error> {
    let (since, until) = filter_range(filter);
    let mut summaries: SummaryMap = HashMap::new();
    db.for_each_rollup(since, until, |_ts, model, rollup| {
        // Avoid cloning model string when the entry already exists
        if !summaries.contains_key(&model) {
            summaries.insert(model.clone(), ModelUsageSummary {
                model: model.clone(), ..Default::default()
            });
        }
        let entry = summaries.get_mut(&model).unwrap();
        accumulate_rollup(entry, &rollup);
    })?;
    Ok(summaries)
}

/// Report grouped by time bucket from TSDB.
/// Uses fast rollup scan when no session/project filters are set.
/// Falls back to event-level scan when filtering by session or project.
pub fn report_grouped_from_db(
    db: &Database,
    group_by: ReportGroupBy,
    filter: ReportFilter,
    session_filter: Option<&str>,
    project_filter: Option<&str>,
) -> Result<GroupedSummaryMap, fjall::Error> {
    let (since, until) = filter_range(filter);
    let mut grouped: GroupedSummaryMap = HashMap::new();

    if session_filter.is_some() || project_filter.is_some() {
        // Event-level scan for session/project filtering
        let dict = db.load_dict_reverse()?;
        let unknown = String::new();
        db.for_each_event(since, until, |ts, event| {
            let model = dict.get(&event.model_id).unwrap_or(&unknown);
            if let Some(sf) = session_filter {
                let session = dict.get(&event.session_id).unwrap_or(&unknown);
                if !session.starts_with(sf) { return; }
            }
            if let Some(pf) = project_filter {
                let project = resolve_project(&dict, &event);
                if !project.contains(pf) { return; }
            }
            let dt = ts_to_datetime(ts, filter.tz);
            let bucket = bucket_from_datetime(dt, group_by);
            let entry = grouped.entry(bucket).or_default()
                .entry(model.clone()).or_insert_with(|| ModelUsageSummary {
                    model: model.clone(), ..Default::default()
                });
            accumulate_event(entry, &event);
        })?;
    } else {
        // Fast rollup-based scan
        db.for_each_rollup(since, until, |hour_ts, model, rollup| {
            let dt = ts_to_datetime(hour_ts, filter.tz);
            let bucket = bucket_from_datetime(dt, group_by);
            let entry = grouped.entry(bucket).or_default()
                .entry(model.clone()).or_insert_with(|| ModelUsageSummary {
                    model, ..Default::default()
                });
            accumulate_rollup(entry, &rollup);
        })?;
    }

    Ok(grouped)
}

/// Report grouped by session from TSDB events (streaming).
pub fn report_by_session_from_db(
    db: &Database,
    filter: ReportFilter,
) -> Result<GroupedSummaryMap, fjall::Error> {
    let dict = db.load_dict_reverse()?;
    let unknown = String::new();
    let (since, until) = filter_range(filter);
    let mut grouped: GroupedSummaryMap = HashMap::new();
    db.for_each_event(since, until, |_ts, event| {
        let session = dict.get(&event.session_id).unwrap_or(&unknown);
        let model = dict.get(&event.model_id).unwrap_or(&unknown);
        let entry = grouped.entry(session.clone()).or_default()
            .entry(model.clone()).or_insert_with(|| ModelUsageSummary {
                model: model.clone(), ..Default::default()
            });
        accumulate_event(entry, &event);
    })?;
    Ok(grouped)
}

/// Collapse a SummaryMap (model → summary) into a single entry based on aggregation function.
fn apply_aggregation_flat(summaries: &mut SummaryMap, func: AggregationFunc) {
    if summaries.is_empty() { return; }

    // Sum all values
    let mut total = ModelUsageSummary::default();
    for s in summaries.values() {
        total.input_tokens += s.input_tokens;
        total.output_tokens += s.output_tokens;
        total.cache_creation_input_tokens += s.cache_creation_input_tokens;
        total.cache_read_input_tokens += s.cache_read_input_tokens;
        total.event_count += s.event_count;
    }

    match func {
        AggregationFunc::Sum => {
            total.model = "(total)".to_string();
        }
        AggregationFunc::Avg => {
            let count = total.event_count.max(1);
            total.input_tokens /= count;
            total.output_tokens /= count;
            total.cache_creation_input_tokens /= count;
            total.cache_read_input_tokens /= count;
            total.event_count = 1;
            total.model = "(avg/event)".to_string();
        }
        AggregationFunc::Count => {
            let count = total.event_count;
            total.input_tokens = 0;
            total.output_tokens = 0;
            total.cache_creation_input_tokens = 0;
            total.cache_read_input_tokens = 0;
            total.event_count = count;
            total.model = "(count)".to_string();
        }
    }

    summaries.clear();
    summaries.insert(total.model.clone(), total);
}

/// Collapse model dimension within each group of a GroupedSummaryMap.
fn apply_aggregation_grouped(grouped: &mut GroupedSummaryMap, func: AggregationFunc) {
    for models in grouped.values_mut() {
        let mut flat: SummaryMap = std::mem::take(models);
        apply_aggregation_flat(&mut flat, func);
        *models = flat;
    }
}

/// Execute a parsed PromQL-style query against the TSDB.
///
/// `since_ms` and `until_ms` are millisecond timestamps representing the time range
/// (0 and i64::MAX respectively mean "no bound"). The query's `offset` modifier shifts
/// both bounds backward by the specified duration.
pub fn execute_parsed_query(
    db: &Database,
    parsed: &crate::query_parser::Query,
    tz: Option<Tz>,
    pricing: Option<&crate::pricing::PricingTable>,
    sink: &dyn crate::sink::Sink,
    since_ms: i64,
    until_ms: i64,
) -> Result<(), String> {
    use crate::query_parser::Metric;

    // Apply offset: shift time range backward by offset duration
    let offset_ms = parsed.offset.map(|b| b.as_secs() as i64 * 1000).unwrap_or(0);
    let since_ms = since_ms - offset_ms;
    let until_ms = until_ms - offset_ms;

    match parsed.metric {
        Metric::Sessions => {
            let session_prefix = parsed.filter_value("session");
            let project_filter = parsed.filter_value("project");
            let has_time_or_project = since_ms > 0 || until_ms < i64::MAX || project_filter.is_some();

            let sessions = if has_time_or_project {
                // Need event-level scan to filter by time range and/or project
                let dict = db.load_dict_reverse().map_err(|e| e.to_string())?;
                let mut set = std::collections::HashSet::new();
                db.for_each_event(since_ms, until_ms, |_ts, event| {
                    let session = dict.get(&event.session_id).map(|s| s.as_str()).unwrap_or("");
                    if let Some(prefix) = session_prefix {
                        if !session.starts_with(prefix) { return; }
                    }
                    if let Some(proj) = project_filter {
                        let project = resolve_project(&dict, &event);
                        if !project.contains(proj) { return; }
                    }
                    set.insert(session.to_string());
                }).map_err(|e| e.to_string())?;
                let mut list: Vec<String> = set.into_iter().collect();
                list.sort();
                list
            } else {
                // Fast path: index scan only
                let mut list = db.list_sessions().map_err(|e| e.to_string())?;
                if let Some(prefix) = session_prefix {
                    list.retain(|s| s.starts_with(prefix));
                }
                list
            };
            sink.emit_list(&sessions, "sessions");
        }
        Metric::Projects => {
            let project_filter = parsed.filter_value("project");
            let has_time = since_ms > 0 || until_ms < i64::MAX;

            let projects = if has_time {
                // Event scan for time-filtered project list
                let dict = db.load_dict_reverse().map_err(|e| e.to_string())?;
                let mut set = std::collections::HashSet::new();
                db.for_each_event(since_ms, until_ms, |_ts, event| {
                    let project = resolve_project(&dict, &event);
                    if project == "unknown" { return; }
                    if let Some(substr) = project_filter {
                        if !project.contains(substr) { return; }
                    }
                    set.insert(project.to_string());
                }).map_err(|e| e.to_string())?;
                let mut list: Vec<String> = set.into_iter().collect();
                list.sort();
                list
            } else {
                let mut list = db.list_projects().map_err(|e| e.to_string())?;
                if let Some(substr) = project_filter {
                    list.retain(|p| p.contains(substr));
                }
                list
            };
            sink.emit_list(&projects, "projects");
        }
        Metric::Events if parsed.bucket.is_none() && parsed.group_by.is_empty() && parsed.aggregation.is_none() => {
            // Raw event listing (no bucket/group_by)
            let dict = db.load_dict_reverse().map_err(|e| e.to_string())?;
            let unknown = String::new();
            let model_filter = parsed.filter_value("model");
            let session_filter = parsed.filter_value("session");
            let project_filter = parsed.filter_value("project");

            let mut events: Vec<RawEvent> = Vec::new();
            db.for_each_event(since_ms, until_ms, |ts, event| {
                let model = dict.get(&event.model_id).unwrap_or(&unknown);
                if let Some(mf) = model_filter {
                    if model != mf { return; }
                }
                let session = dict.get(&event.session_id).unwrap_or(&unknown);
                if let Some(sf) = session_filter {
                    if !session.starts_with(sf) { return; }
                }
                let project = resolve_project(&dict, &event);
                if let Some(pf) = project_filter {
                    if !project.contains(pf) { return; }
                }

                let dt = ts_to_datetime(ts, tz);
                events.push(RawEvent {
                    timestamp: dt.format("%Y-%m-%dT%H:%M:%S").to_string(),
                    model: model.clone(),
                    session: session.clone(),
                    project: project.to_string(),
                    input_tokens: event.input_tokens,
                    output_tokens: event.output_tokens,
                    cache_creation_input_tokens: event.cache_creation_input_tokens,
                    cache_read_input_tokens: event.cache_read_input_tokens,
                });
            }).map_err(|e| e.to_string())?;

            sink.emit_events_batch(&events, pricing, None);
        }
        Metric::Cost | Metric::Events | Metric::Usage => {
            let since_dt = if since_ms > 0 {
                chrono::DateTime::from_timestamp_millis(since_ms).map(|d| d.naive_utc())
            } else {
                None
            };
            let until_dt = if until_ms < i64::MAX {
                chrono::DateTime::from_timestamp_millis(until_ms).map(|d| d.naive_utc())
            } else {
                None
            };

            let filter = ReportFilter { since: since_dt, until: until_dt, tz };
            let model_filter = parsed.filter_value("model");
            let session_filter = parsed.filter_value("session");
            let type_filter = parsed.get_filter("type");
            let type_mask = type_filter_mask(type_filter);

            match (&parsed.bucket, parsed.group_by.is_empty()) {
                (None, true) => {
                    // Flat summary
                    let mut summaries = report_summary_from_db(db, filter).map_err(|e| e.to_string())?;
                    if let Some(model) = model_filter {
                        summaries.retain(|k, _| k == model);
                    }
                    if type_filter.is_some() {
                        for s in summaries.values_mut() {
                            apply_type_mask(s, &type_mask);
                        }
                    }
                    if let Some(func) = parsed.aggregation {
                        apply_aggregation_flat(&mut summaries, func);
                    }
                    sink.emit_summary(&summaries, pricing, None);
                }
                _ => {
                    // Grouped output (bucket and/or group_by)
                    let (since, until) = filter_range(filter);
                    let mut grouped: GroupedSummaryMap = HashMap::new();

                    // Use event-level scan when:
                    // - session filter or group_by needs per-event data, OR
                    // - bucket is not an exact multiple of the 1-hour rollup granularity
                    let bucket_needs_event_scan = parsed.bucket.as_ref()
                        .map_or(false, |b| b.0 < 3600 || b.0 % 3600 != 0);
                    if session_filter.is_some() || !parsed.group_by.is_empty() || bucket_needs_event_scan {
                        // Need event-level access for session/group_by
                        let dict = db.load_dict_reverse().map_err(|e| e.to_string())?;
                        let unknown = String::new();
                        let step_start_sec = since / 1000;
                        db.for_each_event(since, until, |ts, event| {
                            let model = dict.get(&event.model_id).unwrap_or(&unknown);
                            if let Some(mf) = model_filter {
                                if model != mf { return; }
                            }
                            let session = dict.get(&event.session_id).unwrap_or(&unknown);
                            if let Some(sf) = session_filter {
                                if !session.starts_with(sf) { return; }
                            }

                            let bucket_key = if let Some(ref bucket) = parsed.bucket {
                                // VM query_range compatible bucketing.
                                // Eval points: start, start+step, start+2*step, ...
                                // Each point t covers window (t-step, t].
                                let step_ms = bucket.as_secs() as i64 * 1000;
                                let start_ms = step_start_sec * 1000;
                                let offset_ms = ts - start_ms;
                                if offset_ms < -step_ms { return; } // before first window
                                let idx = if offset_ms <= 0 { 0 } else { (offset_ms + step_ms - 1) / step_ms };
                                let eval_ms = start_ms + idx * step_ms;
                                if eval_ms > until { return; } // past end
                                let eval_sec = eval_ms / 1000;
                                bucket.format_label(eval_sec, tz)
                            } else {
                                String::new()
                            };

                            let group_key = build_group_key(&parsed.group_by, model, session, &dict, &event);
                            let key = if bucket_key.is_empty() && !group_key.is_empty() {
                                group_key
                            } else if !bucket_key.is_empty() && group_key.is_empty() {
                                bucket_key
                            } else if !bucket_key.is_empty() && !group_key.is_empty() {
                                format!("{}|{}", bucket_key, group_key)
                            } else {
                                "total".to_string()
                            };

                            let entry = grouped.entry(key).or_default()
                                .entry(model.clone()).or_insert_with(|| ModelUsageSummary {
                                    model: model.clone(), ..Default::default()
                                });
                            accumulate_event(entry, &event);
                        }).map_err(|e| e.to_string())?;
                    } else {
                        // Rollup-based (faster, no session/group_by needed)
                        db.for_each_rollup(since, until, |hour_ts, model, rollup| {
                            if let Some(mf) = model_filter {
                                if model != mf { return; }
                            }
                            let bucket_key = if let Some(ref bucket) = parsed.bucket {
                                // Pass original UTC timestamp (ms -> s) and let format_label handle tz
                                bucket.format_label(hour_ts / 1000, tz)
                            } else {
                                "total".to_string()
                            };

                            let entry = grouped.entry(bucket_key).or_default()
                                .entry(model.clone()).or_insert_with(|| ModelUsageSummary {
                                    model, ..Default::default()
                                });
                            accumulate_rollup(entry, &rollup);
                        }).map_err(|e| e.to_string())?;
                    }

                    if type_filter.is_some() {
                        for models in grouped.values_mut() {
                            for s in models.values_mut() {
                                apply_type_mask(s, &type_mask);
                            }
                        }
                    }

                    if let Some(func) = parsed.aggregation {
                        apply_aggregation_grouped(&mut grouped, func);
                    }

                    let type_name = if let Some(ref bucket) = parsed.bucket {
                        format!("every {}", bucket)
                    } else {
                        "grouped".to_string()
                    };
                    sink.emit_grouped(&grouped, &type_name, pricing, None);
                }
            }
        }
    }
    Ok(())
}

/// Resolve the project name for a StoredEvent.
/// Prefers project_name_id (dictionary lookup); falls back to source_file path extraction
/// for backward compatibility with events stored before project_name_id was added.
fn resolve_project<'a>(dict: &'a HashMap<u32, String>, event: &crate::common::types::StoredEvent) -> &'a str {
    if event.project_name_id != 0 {
        if let Some(name) = dict.get(&event.project_name_id) {
            return name.as_str();
        }
    }
    // Fallback: extract from source file path (works for Claude Code)
    let source = dict.get(&event.source_file_id).map(|s| s.as_str()).unwrap_or("");
    crate::engine::extract_project_name(source).unwrap_or("unknown")
}

fn build_group_key(
    group_by: &[String],
    model: &str,
    session: &str,
    dict: &HashMap<u32, String>,
    event: &crate::common::types::StoredEvent,
) -> String {
    if group_by.is_empty() {
        return String::new();
    }

    let resolve = |dim: &str| -> &str {
        match dim {
            "model" => model,
            "session" => session,
            "project" => {
                resolve_project(dict, event)
            }
            _ => "",
        }
    };

    // Fast path: single dimension avoids Vec + join
    if group_by.len() == 1 {
        return resolve(&group_by[0]).to_string();
    }

    let mut result = String::new();
    for (i, dim) in group_by.iter().enumerate() {
        if i > 0 { result.push('|'); }
        result.push_str(resolve(dim));
    }
    result
}

/// Parse a time range string into NaiveDateTime (UTC).
///
/// Accepted formats (in order of detection):
///   YYYYMMDD              — date only; since=00:00:00, until=23:59:59 (tz-aware)
///   YYYYMMDDhhmmss        — compact datetime (tz-aware)
///   Unix seconds          — all-digit, 1–10 chars  e.g. "1743465600"
///   Unix milliseconds     — all-digit, 13 chars    e.g. "1743465600123"
///   RFC 3339 / ISO 8601   — e.g. "2026-03-01T12:00:00Z", "2026-03-01T21:00:00+09:00"
///
/// The `tz` parameter applies only to the compact formats (YYYYMMDD, YYYYMMDDhhmmss)
/// where the input has no timezone information. Unix/RFC 3339 inputs are always UTC
/// regardless of `tz`.
pub fn parse_range_time(value: &str, is_until: bool, tz: Option<Tz>) -> Result<NaiveDateTime, String> {
    let all_digits = value.chars().all(|c| c.is_ascii_digit());

    // ── Compact YYYYMMDD (must check before Unix seconds — both are all-digit) ─
    if value.len() == 8 && all_digits {
        let year: i32  = value[0..4].parse().map_err(|_| "invalid year")?;
        let month: u32 = value[4..6].parse().map_err(|_| "invalid month")?;
        let day: u32   = value[6..8].parse().map_err(|_| "invalid day")?;
        let date = chrono::NaiveDate::from_ymd_opt(year, month, day).ok_or("invalid date")?;
        let time = if is_until {
            chrono::NaiveTime::from_hms_opt(23, 59, 59).unwrap()
        } else {
            chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap()
        };
        let naive = NaiveDateTime::new(date, time);
        return match tz {
            Some(tz) => tz.from_local_datetime(&naive)
                .single()
                .map(|d| d.naive_utc())
                .ok_or_else(|| "ambiguous or invalid local time".to_string()),
            None => Ok(naive),
        };
    }

    // ── Compact YYYYMMDDhhmmss (must check before Unix ms — both are 14-digit) ─
    if value.len() == 14 && all_digits {
        let year: i32  = value[0..4].parse().map_err(|_| "invalid year")?;
        let month: u32 = value[4..6].parse().map_err(|_| "invalid month")?;
        let day: u32   = value[6..8].parse().map_err(|_| "invalid day")?;
        let hour: u32  = value[8..10].parse().map_err(|_| "invalid hour")?;
        let min: u32   = value[10..12].parse().map_err(|_| "invalid minute")?;
        let sec: u32   = value[12..14].parse().map_err(|_| "invalid second")?;
        let date = chrono::NaiveDate::from_ymd_opt(year, month, day).ok_or("invalid date")?;
        let time = chrono::NaiveTime::from_hms_opt(hour, min, sec).ok_or("invalid time")?;
        let naive = NaiveDateTime::new(date, time);
        return match tz {
            Some(tz) => tz.from_local_datetime(&naive)
                .single()
                .map(|d| d.naive_utc())
                .ok_or_else(|| "ambiguous or invalid local time".to_string()),
            None => Ok(naive),
        };
    }

    // ── Unix timestamp (seconds or milliseconds) ─────────────────────────────
    // 13 digits = Unix ms; 1–10 digits = Unix seconds.
    // 8 and 14 are already handled above as compact date formats.
    if all_digits && !value.is_empty() && value.len() != 8 && value.len() != 14 && value.len() <= 13 {
        let n: i64 = value.parse().map_err(|_| "invalid unix timestamp")?;
        let ms = if value.len() == 13 { n } else { n * 1000 };
        return chrono::DateTime::from_timestamp_millis(ms)
            .map(|d| d.naive_utc())
            .ok_or_else(|| "unix timestamp out of range".to_string());
    }

    // ── RFC 3339 / ISO 8601 ───────────────────────────────────────────────────
    // Try fixed-offset first (covers "Z" and "+HH:MM")
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
        return Ok(dt.naive_utc());
    }
    // Naive ISO 8601 without timezone (treat as local / tz-aware)
    for fmt in &["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(value, fmt)
            .or_else(|_| {
                chrono::NaiveDate::parse_from_str(value, fmt)
                    .map(|d| NaiveDateTime::new(d, chrono::NaiveTime::from_hms_opt(0,0,0).unwrap()))
            })
        {
            return match tz {
                Some(tz) => tz.from_local_datetime(&naive)
                    .single()
                    .map(|d| d.naive_utc())
                    .ok_or_else(|| "ambiguous or invalid local time".to_string()),
                None => Ok(naive),
            };
        }
    }

    Err(format!(
        "invalid time format '{}' (accepted: YYYYMMDD, YYYYMMDDhhmmss, unix seconds, unix ms, RFC 3339)",
        value
    ))
}

fn filter_to_ms(dt: Option<NaiveDateTime>) -> Option<i64> {
    dt.map(|d| d.and_utc().timestamp_millis())
}

fn ts_to_datetime(ts_ms: i64, tz: Option<Tz>) -> NaiveDateTime {
    let utc = chrono::DateTime::from_timestamp_millis(ts_ms)
        .unwrap_or_default()
        .naive_utc();
    match tz {
        Some(tz) => chrono::Utc.from_utc_datetime(&utc).with_timezone(&tz).naive_local(),
        None => utc,
    }
}

fn bucket_from_datetime(ts: NaiveDateTime, group_by: ReportGroupBy) -> String {
    let date = ts.date();
    match group_by {
        ReportGroupBy::Date => date.format("%Y-%m-%d").to_string(),
        ReportGroupBy::Week { start_of_week } => {
            let (week_year, week) = week_bucket(date, start_of_week);
            format!("{:04}-W{:02}", week_year, week)
        }
        ReportGroupBy::Month => date.format("%Y-%m").to_string(),
        ReportGroupBy::Year => format!("{:04}", date.year()),
        ReportGroupBy::Hour => ts.format("%Y-%m-%dT%H:00").to_string(),
    }
}

fn week_bucket(date: chrono::NaiveDate, start_of_week: Weekday) -> (i32, u32) {
    let date_week_start = week_start(date, start_of_week);
    let mut year = date_week_start.year();
    let first_start = first_week_start(year, start_of_week);
    if date_week_start < first_start {
        year -= 1;
    }
    let first_start = first_week_start(year, start_of_week);
    let days = date_week_start.signed_duration_since(first_start).num_days();
    let week = (days / 7 + 1) as u32;
    (year, week)
}

fn week_start(date: chrono::NaiveDate, start_of_week: Weekday) -> chrono::NaiveDate {
    let date_idx = weekday_index(date.weekday());
    let start_idx = weekday_index(start_of_week);
    let delta = (7 + date_idx - start_idx) % 7;
    date - chrono::Duration::days(delta as i64)
}

fn first_week_start(year: i32, start_of_week: Weekday) -> chrono::NaiveDate {
    let jan1 = chrono::NaiveDate::from_ymd_opt(year, 1, 1).unwrap();
    let delta = (weekday_index(start_of_week) - weekday_index(jan1.weekday()) + 7) % 7;
    jan1 + chrono::Duration::days(delta as i64)
}

fn weekday_index(day: Weekday) -> i32 {
    match day {
        Weekday::Mon => 0,
        Weekday::Tue => 1,
        Weekday::Wed => 2,
        Weekday::Thu => 3,
        Weekday::Fri => 4,
        Weekday::Sat => 5,
        Weekday::Sun => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::types::{RollupValue, StoredEvent};

    #[test]
    fn test_report_summary_from_db() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("test.fjall")).unwrap();

        let hour1 = 3_600_000i64;
        let hour2 = 7_200_000i64;

        let r1 = RollupValue { input: 100, output: 50, cache_create: 10, cache_read: 20, count: 5 };
        let r2 = RollupValue { input: 200, output: 100, cache_create: 20, cache_read: 40, count: 10 };

        let mut batch = db.batch();
        db.upsert_rollup(&mut batch, hour1, "claude-opus-4-6", &r1);
        db.upsert_rollup(&mut batch, hour2, "claude-opus-4-6", &r2);
        batch.commit().unwrap();

        let filter = ReportFilter::default();
        let result = report_summary_from_db(&db, filter).unwrap();

        assert_eq!(result.len(), 1);
        let s = &result["claude-opus-4-6"];
        assert_eq!(s.input_tokens, 300);
        assert_eq!(s.output_tokens, 150);
        assert_eq!(s.event_count, 15);
    }

    #[test]
    fn test_report_grouped_from_db() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("test.fjall")).unwrap();

        // Two different days
        let day1 = 1709251200000i64; // 2024-03-01 00:00:00 UTC
        let day2 = 1709337600000i64; // 2024-03-02 00:00:00 UTC

        let r1 = RollupValue { input: 100, output: 50, cache_create: 0, cache_read: 0, count: 5 };
        let r2 = RollupValue { input: 200, output: 100, cache_create: 0, cache_read: 0, count: 10 };

        let mut batch = db.batch();
        db.upsert_rollup(&mut batch, day1, "claude-opus-4-6", &r1);
        db.upsert_rollup(&mut batch, day2, "claude-opus-4-6", &r2);
        batch.commit().unwrap();

        let filter = ReportFilter::default();
        let result = report_grouped_from_db(&db, ReportGroupBy::Date, filter, None, None).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_report_by_session_from_db() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("test.fjall")).unwrap();

        // Set up dict entries
        let mut batch = db.batch();
        db.dict_put(&mut batch, "claude-opus-4-6", 1);
        db.dict_put(&mut batch, "session-abc", 2);
        db.dict_put(&mut batch, "/path/to/file.jsonl", 3);
        batch.commit().unwrap();

        let event = StoredEvent {
            model_id: 1, session_id: 2, source_file_id: 3, project_name_id: 0,
            input_tokens: 100, output_tokens: 50,
            cache_creation_input_tokens: 10, cache_read_input_tokens: 20,
        };
        db.insert_event(1000, "msg1", &event).unwrap();
        db.insert_event(2000, "msg2", &event).unwrap();

        let filter = ReportFilter::default();
        let result = report_by_session_from_db(&db, filter).unwrap();

        assert_eq!(result.len(), 1);
        let session = &result["session-abc"];
        assert_eq!(session["claude-opus-4-6"].input_tokens, 200);
        assert_eq!(session["claude-opus-4-6"].event_count, 2);
    }

    // ── parse_range_time format coverage ────────────────────────────────────

    #[test]
    fn test_parse_range_time_yyyymmdd() {
        let dt = parse_range_time("20260301", false, None).unwrap();
        assert_eq!(dt.format("%Y-%m-%d %H:%M:%S").to_string(), "2026-03-01 00:00:00");
        let dt_until = parse_range_time("20260301", true, None).unwrap();
        assert_eq!(dt_until.format("%H:%M:%S").to_string(), "23:59:59");
    }

    #[test]
    fn test_parse_range_time_yyyymmddhhmmss() {
        let dt = parse_range_time("20260301120000", false, None).unwrap();
        assert_eq!(dt.format("%Y-%m-%d %H:%M:%S").to_string(), "2026-03-01 12:00:00");
    }

    #[test]
    fn test_parse_range_time_unix_seconds() {
        // 2025-01-01 00:00:00 UTC = 1735689600
        let dt = parse_range_time("1735689600", false, None).unwrap();
        assert_eq!(dt.format("%Y-%m-%d %H:%M:%S").to_string(), "2025-01-01 00:00:00");
    }

    #[test]
    fn test_parse_range_time_unix_ms() {
        // Same moment as above, in milliseconds
        let dt = parse_range_time("1735689600000", false, None).unwrap();
        assert_eq!(dt.format("%Y-%m-%d %H:%M:%S").to_string(), "2025-01-01 00:00:00");
    }

    #[test]
    fn test_parse_range_time_rfc3339_utc() {
        let dt = parse_range_time("2025-01-01T12:00:00Z", false, None).unwrap();
        assert_eq!(dt.format("%Y-%m-%d %H:%M:%S").to_string(), "2025-01-01 12:00:00");
    }

    #[test]
    fn test_parse_range_time_rfc3339_offset() {
        // +09:00 offset — UTC should be 9 hours earlier
        let dt = parse_range_time("2025-01-01T21:00:00+09:00", false, None).unwrap();
        assert_eq!(dt.format("%Y-%m-%d %H:%M:%S").to_string(), "2025-01-01 12:00:00");
    }

    #[test]
    fn test_parse_range_time_all_same_moment() {
        // All four formats representing the same UTC moment
        let secs  = parse_range_time("1735689600",          false, None).unwrap();
        let ms    = parse_range_time("1735689600000",       false, None).unwrap();
        let rfc   = parse_range_time("2025-01-01T00:00:00Z", false, None).unwrap();
        let tz    = parse_range_time("2025-01-01T09:00:00+09:00", false, None).unwrap();
        assert_eq!(secs, ms);
        assert_eq!(secs, rfc);
        assert_eq!(secs, tz);
    }

    #[test]
    fn test_parse_range_time_invalid() {
        assert!(parse_range_time("not-a-date", false, None).is_err());
        assert!(parse_range_time("", false, None).is_err());
    }
}
