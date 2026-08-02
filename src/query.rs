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

/// Match a candidate label value against a filter, honouring `=~`.
///
/// For `=~` the value is treated as simple `|`-separated alternation (the same
/// semantics as the `type` filter): the candidate matches if ANY alternative
/// satisfies `pred`. For `=` the whole value must satisfy `pred`. `pred(candidate,
/// needle)` is the field's base comparison — exact for model, prefix for session,
/// substring for project — so `usage{project=~"foo|bar"}` matches a project that
/// contains either alternative instead of the literal string `foo|bar`.
fn label_matches(filter: &LabelFilter, candidate: &str, pred: impl Fn(&str, &str) -> bool) -> bool {
    if filter.regex {
        filter.value.split('|').any(|alt| pred(candidate, alt))
    } else {
        pred(candidate, &filter.value)
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

/// Accumulate a StoredEvent's token counts into a ModelUsageSummary.
fn accumulate_event(entry: &mut ModelUsageSummary, event: &crate::common::types::StoredEvent) {
    entry.input_tokens += event.input_tokens;
    entry.output_tokens += event.output_tokens;
    entry.cache_creation_input_tokens += event.cache_creation_input_tokens;
    entry.cache_read_input_tokens += event.cache_read_input_tokens;
    entry.event_count += 1;
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
fn apply_aggregation_grouped(grouped: &mut GroupedSummaryMap, func: AggregationFunc, group_by: &[String]) {
    // When group_by is specified, the grouping key already encodes the requested
    // dimension (model, project, session, etc.). The "models" map within each group
    // should not be collapsed — each entry is already the sum for that dimension.
    let has_group_by = !group_by.is_empty();

    if has_group_by {
        // Models are already the grouping dimension — just apply func per model.
        // For Sum this is a no-op (each model entry is already the sum for that model).
        // For Avg/Count, apply per model.
        if func == AggregationFunc::Sum {
            return; // Already summed per model
        }
        for models in grouped.values_mut() {
            for s in models.values_mut() {
                match func {
                    AggregationFunc::Avg => {
                        let count = s.event_count.max(1);
                        s.input_tokens /= count;
                        s.output_tokens /= count;
                        s.cache_creation_input_tokens /= count;
                        s.cache_read_input_tokens /= count;
                        s.event_count = 1;
                    }
                    AggregationFunc::Count => {
                        let count = s.event_count;
                        s.input_tokens = 0;
                        s.output_tokens = 0;
                        s.cache_creation_input_tokens = 0;
                        s.cache_read_input_tokens = 0;
                        s.event_count = count;
                    }
                    _ => {}
                }
            }
        }
    } else {
        // No model in group_by — collapse all models within each group
        for models in grouped.values_mut() {
            let mut flat: SummaryMap = std::mem::take(models);
            apply_aggregation_flat(&mut flat, func);
            *models = flat;
        }
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
    start_of_week: Weekday,
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
        Metric::Windows => {
            // One row per window instance; whole-keyspace scan is a few
            // hundred rows. Range filters apply to the window anchor.
            let mut rows: Vec<crate::windows::WindowRow> = Vec::new();
            db.for_each_window_in(since_ms, until_ms, |key, snap| {
                rows.push(crate::windows::WindowRow::from_stored(key, &snap));
            })
            .map_err(|e| e.to_string())?;
            rows.sort_by_key(|r| r.window_end_ms);
            sink.emit_windows(&rows);
        }
        Metric::Sessions => {
            let session_filter = parsed.get_filter("session");
            let project_filter = parsed.get_filter("project");
            let has_time_or_project = since_ms > 0 || until_ms < i64::MAX || project_filter.is_some();

            let sessions = if has_time_or_project {
                // Need event-level scan to filter by time range and/or project
                let dict = db.load_dict_reverse().map_err(|e| e.to_string())?;
                let mut set = std::collections::HashSet::new();
                db.for_each_event(since_ms, until_ms, |_ts, event| {
                    let session = dict.get(&event.session_id).map(|s| s.as_str()).unwrap_or("");
                    if let Some(f) = session_filter {
                        if !label_matches(f, session, |c, v| c.starts_with(v)) { return; }
                    }
                    if let Some(f) = project_filter {
                        let project = resolve_project(&dict, &event);
                        if !label_matches(f, project, |c, v| c.contains(v)) { return; }
                    }
                    set.insert(session.to_string());
                }).map_err(|e| e.to_string())?;
                let mut list: Vec<String> = set.into_iter().collect();
                list.sort();
                list
            } else {
                // Fast path: index scan only
                let mut list = db.list_sessions().map_err(|e| e.to_string())?;
                if let Some(f) = session_filter {
                    list.retain(|s| label_matches(f, s, |c, v| c.starts_with(v)));
                }
                list
            };
            sink.emit_list(&sessions, "sessions");
        }
        Metric::Projects => {
            let project_filter = parsed.get_filter("project");
            let has_time = since_ms > 0 || until_ms < i64::MAX;

            let projects = if has_time {
                // Event scan for time-filtered project list
                let dict = db.load_dict_reverse().map_err(|e| e.to_string())?;
                let mut set = std::collections::HashSet::new();
                db.for_each_event(since_ms, until_ms, |_ts, event| {
                    let project = resolve_project(&dict, &event);
                    if project == "unknown" { return; }
                    if let Some(f) = project_filter {
                        if !label_matches(f, project, |c, v| c.contains(v)) { return; }
                    }
                    set.insert(project.to_string());
                }).map_err(|e| e.to_string())?;
                let mut list: Vec<String> = set.into_iter().collect();
                list.sort();
                list
            } else {
                let mut list = db.list_projects().map_err(|e| e.to_string())?;
                if let Some(f) = project_filter {
                    list.retain(|p| label_matches(f, p, |c, v| c.contains(v)));
                }
                list
            };
            sink.emit_list(&projects, "projects");
        }
        Metric::Events if parsed.bucket.is_none() && parsed.group_by.is_empty() && parsed.aggregation.is_none() => {
            // Raw event listing (no bucket/group_by)
            let dict = db.load_dict_reverse().map_err(|e| e.to_string())?;
            let unknown = String::new();
            let model_filter = parsed.get_filter("model");
            let session_filter = parsed.get_filter("session");
            let project_filter = parsed.get_filter("project");

            let mut events: Vec<RawEvent> = Vec::new();
            db.for_each_event(since_ms, until_ms, |ts, event| {
                let model = dict.get(&event.model_id).unwrap_or(&unknown);
                if let Some(f) = model_filter {
                    if !label_matches(f, model, |c, v| c == v) { return; }
                }
                let session = dict.get(&event.session_id).unwrap_or(&unknown);
                if let Some(f) = session_filter {
                    if !label_matches(f, session, |c, v| c.starts_with(v)) { return; }
                }
                let project = resolve_project(&dict, &event);
                if let Some(f) = project_filter {
                    if !label_matches(f, project, |c, v| c.contains(v)) { return; }
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
            let is_events_metric = parsed.metric == Metric::Events;
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
            let model_filter = parsed.get_filter("model");
            let session_filter = parsed.get_filter("session");
            let project_filter = parsed.get_filter("project");
            let type_filter = parsed.get_filter("type");
            let type_mask = type_filter_mask(type_filter);

            match (&parsed.bucket, parsed.group_by.is_empty()) {
                (None, true) => {
                    // Flat summary via events keyspace scan.
                    let mut summaries = {
                        let dict = db.load_dict_reverse().map_err(|e| e.to_string())?;
                        let unknown = String::new();
                        let mut sums: SummaryMap = HashMap::new();
                        db.for_each_event(since_ms, until_ms, |_ts, event| {
                            let model = dict.get(&event.model_id).unwrap_or(&unknown);
                            if let Some(f) = model_filter {
                                if !label_matches(f, model, |c, v| c == v) { return; }
                            }
                            if let Some(f) = session_filter {
                                let session = dict.get(&event.session_id).map(|s| s.as_str()).unwrap_or("");
                                if !label_matches(f, session, |c, v| c.starts_with(v)) { return; }
                            }
                            if let Some(f) = project_filter {
                                let project = resolve_project(&dict, &event);
                                if !label_matches(f, project, |c, v| c.contains(v)) { return; }
                            }
                            let entry = sums.entry(model.clone()).or_insert_with(|| ModelUsageSummary {
                                model: model.clone(), ..Default::default()
                            });
                            if is_events_metric {
                                entry.event_count += 1;
                            } else {
                                accumulate_event(entry, &event);
                            }
                        }).map_err(|e| e.to_string())?;
                        sums
                    };
                    if let Some(f) = model_filter {
                        summaries.retain(|k, _| label_matches(f, k, |c, v| c == v));
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

                    // Event-level scan for all grouped queries
                    let dict = db.load_dict_reverse().map_err(|e| e.to_string())?;
                    let unknown = String::new();
                    db.for_each_event(since, until, |ts, event| {
                        let model = dict.get(&event.model_id).unwrap_or(&unknown);
                        if let Some(f) = model_filter {
                            if !label_matches(f, model, |c, v| c == v) { return; }
                        }
                        let session = dict.get(&event.session_id).unwrap_or(&unknown);
                        if let Some(f) = session_filter {
                            if !label_matches(f, session, |c, v| c.starts_with(v)) { return; }
                        }
                        if let Some(f) = project_filter {
                            let project = resolve_project(&dict, &event);
                            if !label_matches(f, project, |c, v| c.contains(v)) { return; }
                        }

                        let bucket_key = if let Some(ref bucket) = parsed.bucket {
                            // Bucket = floor(event_ts) to the period start.
                            // For day-and-larger periods the floor is done in the user's
                            // local timezone (bucket_start_ms) so an event lands in the
                            // wall-clock day/week the user experienced, not the UTC one.
                            // E.g. step=86400, event at 03-23T05:00 → bucket=03-23T00:00.
                            let step_ms = bucket.as_secs() as i64 * 1000;
                            let bucket_ms = bucket_start_ms(ts, step_ms, tz, start_of_week);
                            // No overlap re-check here: the event scan already
                            // enforces since <= ts < until, and this event belongs
                            // to `bucket_ms` by construction. A `bucket_ms + step_ms`
                            // guard would assume every local day/week is exactly
                            // step_ms long and so drop a legitimately in-range event
                            // on a 25h DST fall-back day (bucket start + 24h can fall
                            // before `since` while ts is still ≥ since).
                            let bucket_sec = bucket_ms / 1000;
                            bucket.format_label(bucket_sec, tz)
                        } else {
                            String::new()
                        };

                        let group_key = build_group_key(&parsed.group_by, model, session, &dict, &event);

                        // Determine inner key before group_key is moved into the period key.
                        let inner_key = if !group_key.is_empty() && !parsed.group_by.iter().any(|g| g == "model") {
                            group_key.clone()
                        } else {
                            model.clone()
                        };

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
                            .entry(inner_key.clone()).or_insert_with(|| ModelUsageSummary {
                                model: inner_key, ..Default::default()
                            });
                        if is_events_metric {
                            entry.event_count += 1;
                        } else {
                            accumulate_event(entry, &event);
                        }
                    }).map_err(|e| e.to_string())?;

                    if type_filter.is_some() {
                        for models in grouped.values_mut() {
                            for s in models.values_mut() {
                                apply_type_mask(s, &type_mask);
                            }
                        }
                    }

                    if let Some(func) = parsed.aggregation {
                        apply_aggregation_grouped(&mut grouped, func, &parsed.group_by);
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

/// Compute the start-of-bucket timestamp (ms) that an event at `ts_ms` falls into.
///
/// Canonical rule shared bit-for-bit with the sync server (toki_sync
/// `metrics.rs::bucket_start_sec`) so a local query and the server dashboard put
/// the same event in the same bucket:
///
/// * whole-day-multiple steps (`[1d]`, `[2d]`, `[30d]`, `[365d]`, …) with a tz →
///   floored to local midnight. The day index (days since the 1970-01-01
///   local-midnight anchor) is floored by `div_euclid(step_days)`. `[30d]` and
///   `[365d]` (the monthly/yearly reports) are rolling-window approximations
///   anchored at the epoch, NOT calendar months/years.
/// * week (`[1w]`, 604_800_000ms) with a tz → the one exception: aligned to the
///   `start_of_week` local midnight, so a Monday/Sunday-start week is honoured
///   instead of the epoch weekday.
/// * everything else — sub-day steps, day+ steps that are not a whole-day
///   multiple (`[27h]`), and any step when no tz is given — stays epoch/UTC
///   aligned (`(ts / step) * step`).
///
/// A KST event at 2026-03-10T20:00Z (local 03-11 05:00) therefore lands in the
/// 03-11 day bucket, where naive UTC flooring would have put it under 03-10.
fn bucket_start_ms(ts_ms: i64, step_ms: i64, tz: Option<Tz>, start_of_week: Weekday) -> i64 {
    const DAY_MS: i64 = 86_400_000;
    const WEEK_MS: i64 = 604_800_000;
    let epoch_aligned = || (ts_ms / step_ms) * step_ms;

    // Local-calendar flooring applies to whole-day-multiple steps with a tz.
    let whole_day_multiple = step_ms >= DAY_MS && step_ms % DAY_MS == 0;
    if let (true, Some(tz)) = (whole_day_multiple, tz) {
        if let Some(local) = chrono::DateTime::from_timestamp_millis(ts_ms)
            .map(|dt| dt.with_timezone(&tz))
        {
            let date = local.date_naive();
            let start_date = if step_ms == WEEK_MS {
                // Week: align to the start_of_week local midnight (honours the
                // user's setting; the server matches this).
                let back = (date.weekday().num_days_from_monday() as i64
                    - start_of_week.num_days_from_monday() as i64 + 7) % 7;
                date - chrono::Duration::days(back)
            } else {
                // day / 2d / 30d / 365d: floor the day index by step_days,
                // anchored at the 1970-01-01 local midnight.
                let step_days = step_ms / DAY_MS;
                let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
                let day_index = (date - epoch).num_days();
                epoch + chrono::Duration::days(day_index.div_euclid(step_days) * step_days)
            };
            if let Some(midnight) = start_date.and_hms_opt(0, 0, 0) {
                // Resolve local midnight to its UTC instant. The bucket must always
                // anchor to the start of the local day (never an epoch fallback), so
                // the two DST edge cases are handled explicitly and the server mirrors
                // this bit-for-bit:
                //   * fall-back (local midnight happens twice) → the earlier instant.
                //   * spring-forward gap (local midnight never exists) → advance in
                //     1-minute steps to the first local time that does exist, i.e.
                //     the first valid instant of that local day. Bounded to one day
                //     of steps so a pathological tz can never loop forever.
                let resolved = match tz.from_local_datetime(&midnight) {
                    chrono::LocalResult::Single(dt) => Some(dt),
                    chrono::LocalResult::Ambiguous(earlier, _) => Some(earlier),
                    chrono::LocalResult::None => {
                        let mut candidate = midnight;
                        let mut hit = None;
                        for _ in 0..24 * 60 {
                            candidate += chrono::Duration::minutes(1);
                            if let Some(dt) = tz.from_local_datetime(&candidate).earliest() {
                                hit = Some(dt);
                                break;
                            }
                        }
                        hit
                    }
                };
                if let Some(dt) = resolved {
                    return dt.timestamp_millis();
                }
            }
        }
    }

    // Sub-day, non-whole-day step, or no tz: epoch/UTC-aligned.
    epoch_aligned()
}

#[allow(dead_code)]
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
    use crate::common::types::StoredEvent;

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

    // ── tz-aware bucket flooring (finding #2) ────────────────────────────────

    #[test]
    fn test_bucket_start_ms_day_is_tz_aware() {
        let tz: Tz = "Asia/Seoul".parse().unwrap(); // +09:00
        let day = 86_400_000i64;

        // 2026-03-10T20:00Z is 2026-03-11 05:00 KST → local day 03-11.
        let ts = chrono::DateTime::parse_from_rfc3339("2026-03-10T20:00:00Z").unwrap()
            .timestamp_millis();

        // Local-aware bucket floors to 03-11 00:00 KST.
        let local = bucket_start_ms(ts, day, Some(tz), Weekday::Mon);
        let local_dt = chrono::DateTime::from_timestamp_millis(local).unwrap().with_timezone(&tz);
        assert_eq!(local_dt.format("%Y-%m-%d %H:%M").to_string(), "2026-03-11 00:00");

        // The rendered label (what actually keys the group) matches the local day.
        assert_eq!(
            crate::query_parser::Bucket(86400).format_label(local / 1000, Some(tz)),
            "2026-03-11T00:00:00"
        );

        // UTC flooring (no tz) still lands on 03-10.
        let utc = bucket_start_ms(ts, day, None, Weekday::Mon);
        let utc_dt = chrono::DateTime::from_timestamp_millis(utc).unwrap().naive_utc();
        assert_eq!(utc_dt.format("%Y-%m-%d %H:%M").to_string(), "2026-03-10 00:00");
    }

    #[test]
    fn test_bucket_start_ms_adjacent_local_days_split() {
        let tz: Tz = "Asia/Seoul".parse().unwrap();
        let day = 86_400_000i64;
        // 03-10T20:00Z → local 03-11; 03-11T20:00Z → local 03-12: different buckets.
        let a = chrono::DateTime::parse_from_rfc3339("2026-03-10T20:00:00Z").unwrap().timestamp_millis();
        let b = chrono::DateTime::parse_from_rfc3339("2026-03-11T20:00:00Z").unwrap().timestamp_millis();
        assert_ne!(bucket_start_ms(a, day, Some(tz), Weekday::Mon), bucket_start_ms(b, day, Some(tz), Weekday::Mon));
    }

    #[test]
    fn test_bucket_start_ms_subday_stays_epoch_aligned() {
        let tz: Tz = "Asia/Seoul".parse().unwrap();
        let hour = 3_600_000i64;
        let ts = chrono::DateTime::parse_from_rfc3339("2026-03-10T20:37:00Z").unwrap().timestamp_millis();
        // Sub-day steps ignore tz (epoch-aligned) whether or not a tz is given.
        assert_eq!(bucket_start_ms(ts, hour, Some(tz), Weekday::Mon), (ts / hour) * hour);
        assert_eq!(bucket_start_ms(ts, hour, None, Weekday::Mon), (ts / hour) * hour);
    }

    #[test]
    fn test_bucket_start_ms_non_whole_day_step_epoch_aligned() {
        let tz: Tz = "Asia/Seoul".parse().unwrap();
        let ts = chrono::DateTime::parse_from_rfc3339("2026-03-10T20:37:00Z").unwrap().timestamp_millis();
        // 27h is > a day but not a whole-day multiple: must stay epoch-aligned,
        // NOT be truncated to a 1-day (86400) bucket.
        let step_27h = 27 * 3_600_000i64;
        assert_eq!(bucket_start_ms(ts, step_27h, Some(tz), Weekday::Mon), (ts / step_27h) * step_27h);
        // Guard against the truncation bug: it must differ from a 1-day floor.
        let day = 86_400_000i64;
        assert_ne!(bucket_start_ms(ts, step_27h, Some(tz), Weekday::Mon), bucket_start_ms(ts, day, Some(tz), Weekday::Mon));
    }

    #[test]
    fn test_bucket_start_ms_whole_day_multiples_tz_floor() {
        // 2d/30d/365d are whole-day multiples: with a tz they floor to a LOCAL
        // midnight (1970-anchored div_euclid), not UTC epoch alignment.
        let tz: Tz = "Asia/Seoul".parse().unwrap();
        let ts = chrono::DateTime::parse_from_rfc3339("2026-03-10T20:37:00Z").unwrap().timestamp_millis();
        for step in [2 * 86_400_000i64, 30 * 86_400_000i64, 365 * 86_400_000i64] {
            let b = bucket_start_ms(ts, step, Some(tz), Weekday::Mon);
            // Boundary sits at local midnight...
            let b_dt = chrono::DateTime::from_timestamp_millis(b).unwrap().with_timezone(&tz);
            assert_eq!(b_dt.format("%H:%M:%S").to_string(), "00:00:00", "step {step} not local midnight");
            // ...and differs from a pure UTC epoch floor (KST is +09:00).
            assert_ne!(b, (ts / step) * step, "step {step} should be tz-floored, not epoch");
            // No-tz still epoch-aligned.
            assert_eq!(bucket_start_ms(ts, step, None, Weekday::Mon), (ts / step) * step);
        }
    }

    #[test]
    fn test_bucket_start_ms_week_honors_start_of_week() {
        let tz: Tz = "Asia/Seoul".parse().unwrap();
        let week = 604_800_000i64;
        // 2026-03-11 is a Wednesday (KST). With Monday start → week floors to
        // Mon 2026-03-09 00:00 KST; with Sunday start → Sun 2026-03-08 00:00 KST.
        let ts = chrono::DateTime::parse_from_rfc3339("2026-03-11T05:00:00Z").unwrap().timestamp_millis();

        let mon = bucket_start_ms(ts, week, Some(tz), Weekday::Mon);
        let mon_dt = chrono::DateTime::from_timestamp_millis(mon).unwrap().with_timezone(&tz);
        assert_eq!(mon_dt.format("%Y-%m-%d %H:%M").to_string(), "2026-03-09 00:00");
        assert_eq!(mon_dt.weekday(), Weekday::Mon);

        let sun = bucket_start_ms(ts, week, Some(tz), Weekday::Sun);
        let sun_dt = chrono::DateTime::from_timestamp_millis(sun).unwrap().with_timezone(&tz);
        assert_eq!(sun_dt.format("%Y-%m-%d %H:%M").to_string(), "2026-03-08 00:00");
        assert_eq!(sun_dt.weekday(), Weekday::Sun);
    }

    #[test]
    fn test_bucket_start_ms_kst_cross_parity() {
        // Exact boundaries for the shared "same input → same bucket" check with
        // the sync server (toki_sync metrics.rs). Event 2026-03-11T05:00Z is KST
        // 2026-03-11 14:00 (Wednesday). Values are the canonical ms boundaries.
        let tz: Tz = "Asia/Seoul".parse().unwrap();
        let ts = chrono::DateTime::parse_from_rfc3339("2026-03-11T05:00:00Z").unwrap().timestamp_millis();
        let day = 86_400_000i64;
        // Whole-day-multiple steps floor to local midnight (1970-anchored).
        assert_eq!(bucket_start_ms(ts, day, Some(tz), Weekday::Mon), 1_773_154_800_000);       // 1d  → KST 03-11 00:00
        assert_eq!(bucket_start_ms(ts, 2 * day, Some(tz), Weekday::Mon), 1_773_068_400_000);   // 2d  → KST 03-10 00:00
        assert_eq!(bucket_start_ms(ts, 30 * day, Some(tz), Weekday::Mon), 1_772_895_600_000);  // 30d → KST 03-08 00:00
        // Week is the start_of_week exception (Monday → KST 03-09 00:00).
        assert_eq!(bucket_start_ms(ts, 7 * day, Some(tz), Weekday::Mon), 1_772_982_000_000);
        // 27h is not a whole-day multiple → epoch-aligned regardless of tz.
        let step_27h = 27 * 3_600_000i64;
        assert_eq!(bucket_start_ms(ts, step_27h, Some(tz), Weekday::Mon), 1_773_122_400_000);
        assert_eq!(bucket_start_ms(ts, step_27h, None, Weekday::Mon), 1_773_122_400_000);
        // No tz → epoch-aligned for day and week alike.
        assert_eq!(bucket_start_ms(ts, day, None, Weekday::Mon), 1_773_187_200_000);
        assert_eq!(bucket_start_ms(ts, 7 * day, None, Weekday::Mon), 1_772_668_800_000);
    }

    #[test]
    fn test_bucket_start_ms_server_vector_parity() {
        // Cross-check against the sync server's own published boundary vector
        // (toki_sync bucket_start_sec, HEAD e13f801). Input: 2024-03-15T10:30Z
        // (KST 19:30), Asia/Seoul, start_of_week=Monday. Values are epoch seconds
        // exactly as the server returns them; local must produce the same.
        let tz: Tz = "Asia/Seoul".parse().unwrap();
        let ts = chrono::DateTime::parse_from_rfc3339("2024-03-15T10:30:00Z").unwrap().timestamp_millis();
        let day = 86_400_000i64;
        let sec = |ms: i64| ms / 1000;
        assert_eq!(sec(bucket_start_ms(ts, day, Some(tz), Weekday::Mon)), 1_710_428_400);       // 1d
        assert_eq!(sec(bucket_start_ms(ts, 2 * day, Some(tz), Weekday::Mon)), 1_710_342_000);   // 2d
        assert_eq!(sec(bucket_start_ms(ts, 7 * day, Some(tz), Weekday::Mon)), 1_710_082_800);   // 1w (Mon)
        assert_eq!(sec(bucket_start_ms(ts, 30 * day, Some(tz), Weekday::Mon)), 1_708_095_600);  // 30d
        assert_eq!(sec(bucket_start_ms(ts, 27 * 3_600_000, Some(tz), Weekday::Mon)), 1_710_428_400); // 27h (epoch)
    }

    #[test]
    fn test_bucket_start_ms_week_no_tz_epoch_aligned() {
        // With no tz, weekly (like every other step) is pure epoch alignment —
        // start_of_week does not apply. Matches the server (no-tz → (ts/step)*step).
        let week = 604_800_000i64;
        let ts = chrono::DateTime::parse_from_rfc3339("2026-03-11T05:00:00Z").unwrap().timestamp_millis();
        assert_eq!(bucket_start_ms(ts, week, None, Weekday::Mon), (ts / week) * week);
        assert_eq!(bucket_start_ms(ts, week, None, Weekday::Sun), (ts / week) * week);
    }

    #[test]
    fn test_bucket_start_ms_spring_forward_gap_anchors_to_first_valid_instant() {
        // America/Sao_Paulo entered DST on 2018-11-04 by skipping local midnight
        // (00:00 → 01:00), so 2018-11-04 00:00 never existed. The day bucket must
        // still anchor to the START of that local day — the first instant that does
        // exist, 2018-11-04 01:00 local (-02:00) = 2018-11-04T03:00Z — NOT fall back
        // to epoch alignment. The sync server must mirror this exact value.
        let tz: Tz = "America/Sao_Paulo".parse().unwrap();
        let day = 86_400_000i64;
        // An event during 2018-11-04 (local 10:00 = 13:00Z).
        let ts = chrono::DateTime::parse_from_rfc3339("2018-11-04T13:00:00Z").unwrap().timestamp_millis();
        let b = bucket_start_ms(ts, day, Some(tz), Weekday::Mon);
        assert_eq!(b, 1_541_300_400_000, "gap-day bucket must anchor to first valid local instant (2018-11-04T03:00Z)");
        // Sanity: it is the local wall-clock 01:00 of that day, not epoch alignment.
        let b_dt = chrono::DateTime::from_timestamp_millis(b).unwrap().with_timezone(&tz);
        assert_eq!(b_dt.format("%Y-%m-%d %H:%M").to_string(), "2018-11-04 01:00");
        assert_ne!(b, (ts / day) * day, "must not be epoch-aligned fallback");
    }

    // ── DST-aware range/bucket overlap (finding #1) ──────────────────────────

    fn seed_single_event(db: &Database, ts_rfc3339: &str, input: u64) {
        let mut batch = db.batch();
        db.dict_put(&mut batch, "model-x", 1);
        batch.commit().unwrap();
        let ev = StoredEvent {
            model_id: 1, session_id: 0, source_file_id: 0, project_name_id: 0,
            input_tokens: input, output_tokens: 0,
            cache_creation_input_tokens: 0, cache_read_input_tokens: 0,
        };
        let ts = chrono::DateTime::parse_from_rfc3339(ts_rfc3339).unwrap().timestamp_millis();
        db.insert_event(ts, "m1", &ev).unwrap();
    }

    fn bucketed_usage_total(db: &Database, tz: Tz, since_rfc3339: &str, until_rfc3339: &str) -> u64 {
        let q = crate::query_parser::Query {
            metric: crate::query_parser::Metric::Usage,
            filters: vec![],
            bucket: Some(crate::query_parser::Bucket(86400)),
            group_by: vec![],
            provider: None,
            offset: None,
            aggregation: None,
        };
        let since = chrono::DateTime::parse_from_rfc3339(since_rfc3339).unwrap().timestamp_millis();
        let until = chrono::DateTime::parse_from_rfc3339(until_rfc3339).unwrap().timestamp_millis();
        let sink = CaptureSink::default();
        execute_parsed_query(db, &q, Some(tz), Weekday::Mon, None, &sink, since, until).unwrap();
        let grouped = sink.grouped.lock().unwrap();
        grouped.get(0).map(|g| g.values().flat_map(|m| m.values()).map(|s| s.input_tokens).sum()).unwrap_or(0)
    }

    #[test]
    fn test_bucket_range_dst_fall_back_25h_day_keeps_late_event() {
        // America/New_York 2024-11-03 is a 25h local day (fall-back 02:00 → 01:00).
        // Event 2024-11-04T04:45Z is local 2024-11-03 23:45 — still inside the 11-03
        // local day, whose 1d bucket starts at 2024-11-03T04:00Z. bucket_start + 24h
        // = 2024-11-04T04:00Z, which is BEFORE a since of 2024-11-04T04:30Z, so a
        // fixed-length "bucket_ms + step_ms <= since" guard would wrongly drop this
        // in-range event. It must be counted.
        let tz: Tz = "America/New_York".parse().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("t.fjall")).unwrap();
        seed_single_event(&db, "2024-11-04T04:45:00Z", 42);
        let total = bucketed_usage_total(&db, tz, "2024-11-04T04:30:00Z", "2024-11-05T00:00:00Z");
        assert_eq!(total, 42, "late event in a 25h DST fall-back day must not be dropped");
    }

    #[test]
    fn test_bucket_range_dst_spring_forward_23h_day_counts_event() {
        // America/New_York 2024-03-10 is a 23h local day (spring-forward 02:00 →
        // 03:00). An event mid-day must still land in its bucket and be counted for
        // a range starting earlier that day.
        let tz: Tz = "America/New_York".parse().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("t.fjall")).unwrap();
        seed_single_event(&db, "2024-03-10T18:00:00Z", 9); // local 14:00
        let total = bucketed_usage_total(&db, tz, "2024-03-10T12:00:00Z", "2024-03-11T12:00:00Z");
        assert_eq!(total, 9, "event on a 23h DST spring-forward day must be counted");
    }

    // ── usage query session/project filtering (finding #1) ───────────────────

    use std::sync::Mutex;

    #[derive(Default)]
    struct CaptureSink {
        summaries: Mutex<Vec<SummaryMap>>,
        grouped: Mutex<Vec<GroupedSummaryMap>>,
        windows: Mutex<Vec<Vec<crate::windows::WindowRow>>>,
    }

    impl crate::sink::Sink for CaptureSink {
        fn emit_summary(&self, summaries: &SummaryMap, _p: Option<&crate::pricing::PricingTable>, _s: Option<&dyn crate::common::schema::ProviderSchema>) {
            self.summaries.lock().unwrap().push(summaries.clone());
        }
        fn emit_grouped(&self, grouped: &GroupedSummaryMap, _t: &str, _p: Option<&crate::pricing::PricingTable>, _s: Option<&dyn crate::common::schema::ProviderSchema>) {
            self.grouped.lock().unwrap().push(grouped.clone());
        }
        fn emit_event(&self, _e: &crate::common::types::UsageEventWithTs, _p: Option<&crate::pricing::PricingTable>, _s: Option<&dyn crate::common::schema::ProviderSchema>) {}
        fn emit_list(&self, _items: &[String], _t: &str) {}
        fn emit_events_batch(&self, _e: &[RawEvent], _p: Option<&crate::pricing::PricingTable>, _s: Option<&dyn crate::common::schema::ProviderSchema>) {}
        fn emit_windows(&self, rows: &[crate::windows::WindowRow]) {
            self.windows.lock().unwrap().push(rows.to_vec());
        }
    }

    #[test]
    fn windows_metric_lists_rows_in_range() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("t.fjall")).unwrap();
        use crate::windows::{window_key, WindowKind, WindowSnapshotV1, REACHED_NONE};
        let snap = |peak: u16, reset: i64| WindowSnapshotV1 {
            peak_pct_x100: peak,
            last_pct_x100: peak,
            observed_ts_ms: reset - 1000,
            raw_resets_at_ms: reset,
            first_seen_ms: reset - 3_600_000,
            window_minutes: 300,
            finalized: true,
            maxed_out: peak >= 10_000,
            limit_reached_kind: REACHED_NONE,
            time_to_100_ms: -1,
            active_ms: 60_000,
            last_sample_gap_ms: 1000,
            sampled_active_fraction: 1000,
            n_samples: 5,
            limit_id: "five_hour".into(),
            plan: "max_5x".into(),
            account: "a".into(),
        };
        db.upsert_window_merge(&window_key(WindowKind::Session, 1, 2, 1_000_000_000_000), &snap(4200, 1_000_000_000_000)).unwrap();
        db.upsert_window_merge(&window_key(WindowKind::Session, 1, 2, 2_000_000_000_000), &snap(10_000, 2_000_000_000_000)).unwrap();

        let sink = CaptureSink::default();
        let q = crate::query_parser::parse("windows").unwrap();
        // Range excludes the first row.
        execute_parsed_query(&db, &q, None, chrono::Weekday::Mon, None, &sink, 1_500_000_000_000, i64::MAX).unwrap();
        let captured = sink.windows.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].len(), 1);
        let row = &captured[0][0];
        assert_eq!(row.peak_pct, 100.0);
        assert!(row.maxed_out);
        assert_eq!(row.kind, "session");
        assert_eq!(row.limit_id, "five_hour");
    }

    /// Build a small DB with two sessions across two projects for filter tests.
    fn seed_filter_db(db: &Database) {
        let mut batch = db.batch();
        db.dict_put(&mut batch, "model-x", 1);
        db.dict_put(&mut batch, "session-aaa", 2);
        db.dict_put(&mut batch, "session-bbb", 3);
        db.dict_put(&mut batch, "proj-foo", 4);
        db.dict_put(&mut batch, "proj-bar", 5);
        batch.commit().unwrap();

        // session-aaa / proj-foo : input 100
        let ev_foo = StoredEvent {
            model_id: 1, session_id: 2, source_file_id: 0, project_name_id: 4,
            input_tokens: 100, output_tokens: 0,
            cache_creation_input_tokens: 0, cache_read_input_tokens: 0,
        };
        // session-bbb / proj-bar : input 7
        let ev_bar = StoredEvent {
            model_id: 1, session_id: 3, source_file_id: 0, project_name_id: 5,
            input_tokens: 7, output_tokens: 0,
            cache_creation_input_tokens: 0, cache_read_input_tokens: 0,
        };
        db.insert_event(1000, "m1", &ev_foo).unwrap();
        db.insert_event(2000, "m2", &ev_bar).unwrap();
    }

    fn usage_query(filters: Vec<crate::query_parser::LabelFilter>, group_by: Vec<String>) -> crate::query_parser::Query {
        crate::query_parser::Query {
            metric: crate::query_parser::Metric::Usage,
            filters,
            bucket: None,
            group_by,
            provider: None,
            offset: None,
            aggregation: None,
        }
    }

    fn label(key: &str, value: &str) -> crate::query_parser::LabelFilter {
        crate::query_parser::LabelFilter { key: key.into(), value: value.into(), regex: false }
    }

    #[test]
    fn test_usage_flat_project_filter() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("t.fjall")).unwrap();
        seed_filter_db(&db);

        let q = usage_query(vec![label("project", "proj-foo")], vec![]);
        let sink = CaptureSink::default();
        execute_parsed_query(&db, &q, None, Weekday::Mon, None, &sink, 0, i64::MAX).unwrap();

        let summaries = sink.summaries.lock().unwrap();
        let total: u64 = summaries[0].values().map(|s| s.input_tokens).sum();
        assert_eq!(total, 100, "flat usage must only sum proj-foo events");
    }

    #[test]
    fn test_usage_flat_session_filter() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("t.fjall")).unwrap();
        seed_filter_db(&db);

        let q = usage_query(vec![label("session", "session-bbb")], vec![]);
        let sink = CaptureSink::default();
        execute_parsed_query(&db, &q, None, Weekday::Mon, None, &sink, 0, i64::MAX).unwrap();

        let summaries = sink.summaries.lock().unwrap();
        let total: u64 = summaries[0].values().map(|s| s.input_tokens).sum();
        assert_eq!(total, 7, "flat usage must only sum session-bbb events");
    }

    #[test]
    fn test_usage_grouped_project_filter() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("t.fjall")).unwrap();
        seed_filter_db(&db);

        // group by session, filter to proj-foo → only session-aaa should appear.
        let q = usage_query(vec![label("project", "proj-foo")], vec!["session".into()]);
        let sink = CaptureSink::default();
        execute_parsed_query(&db, &q, None, Weekday::Mon, None, &sink, 0, i64::MAX).unwrap();

        let grouped = sink.grouped.lock().unwrap();
        let g = &grouped[0];
        let total: u64 = g.values().flat_map(|m| m.values()).map(|s| s.input_tokens).sum();
        assert_eq!(total, 100, "grouped usage must only sum proj-foo events");
        assert!(g.contains_key("session-aaa"));
        assert!(!g.contains_key("session-bbb"));
    }

    // ── =~ regex (alternation) filtering (finding #4) ────────────────────────

    fn regex_label(key: &str, value: &str) -> crate::query_parser::LabelFilter {
        crate::query_parser::LabelFilter { key: key.into(), value: value.into(), regex: true }
    }

    #[test]
    fn test_usage_flat_project_regex_matches_alternation() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("t.fjall")).unwrap();
        seed_filter_db(&db);

        // project=~"proj-foo|proj-bar" must match BOTH, not the literal string.
        let q = usage_query(vec![regex_label("project", "proj-foo|proj-bar")], vec![]);
        let sink = CaptureSink::default();
        execute_parsed_query(&db, &q, None, Weekday::Mon, None, &sink, 0, i64::MAX).unwrap();
        let summaries = sink.summaries.lock().unwrap();
        let total: u64 = summaries[0].values().map(|s| s.input_tokens).sum();
        assert_eq!(total, 107, "flat =~ must sum both alternatives (100 + 7)");
    }

    #[test]
    fn test_usage_flat_session_regex_matches_alternation() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("t.fjall")).unwrap();
        seed_filter_db(&db);

        let q = usage_query(vec![regex_label("session", "session-aaa|session-bbb")], vec![]);
        let sink = CaptureSink::default();
        execute_parsed_query(&db, &q, None, Weekday::Mon, None, &sink, 0, i64::MAX).unwrap();
        let summaries = sink.summaries.lock().unwrap();
        let total: u64 = summaries[0].values().map(|s| s.input_tokens).sum();
        assert_eq!(total, 107, "flat =~ session must sum both alternatives");
    }

    #[test]
    fn test_usage_grouped_project_regex_matches_alternation() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("t.fjall")).unwrap();
        seed_filter_db(&db);

        // group by session, project=~"proj-foo|proj-bar" → both sessions appear.
        let q = usage_query(vec![regex_label("project", "proj-foo|proj-bar")], vec!["session".into()]);
        let sink = CaptureSink::default();
        execute_parsed_query(&db, &q, None, Weekday::Mon, None, &sink, 0, i64::MAX).unwrap();
        let grouped = sink.grouped.lock().unwrap();
        let g = &grouped[0];
        let total: u64 = g.values().flat_map(|m| m.values()).map(|s| s.input_tokens).sum();
        assert_eq!(total, 107, "grouped =~ must sum both alternatives");
        assert!(g.contains_key("session-aaa"));
        assert!(g.contains_key("session-bbb"));
    }

    #[test]
    fn test_usage_flat_project_regex_substring_alternatives() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("t.fjall")).unwrap();
        seed_filter_db(&db);

        // project matching is substring, so each alternative is a substring test:
        // "foo" ⊂ "proj-foo", "bar" ⊂ "proj-bar" → both match.
        let q = usage_query(vec![regex_label("project", "foo|bar")], vec![]);
        let sink = CaptureSink::default();
        execute_parsed_query(&db, &q, None, Weekday::Mon, None, &sink, 0, i64::MAX).unwrap();
        let summaries = sink.summaries.lock().unwrap();
        let total: u64 = summaries[0].values().map(|s| s.input_tokens).sum();
        assert_eq!(total, 107, "flat =~ substring alternatives must match both projects");
    }
}
