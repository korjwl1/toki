/// Minimal PromQL-inspired query parser for toki.
///
/// Grammar:
///   query   = aggregation_start? metric filters? bucket? offset? aggregation_end? group_by?
///   aggregation_start = ("sum" | "avg" | "count") "("
///   aggregation_end = ")"
///   metric  = "usage" | "sessions" | "projects" | "events"
///   filters = "{" (filter ("," filter)*)? "}"
///   filter  = key "=" quoted_string
///   bucket  = "[" duration "]"
///   offset  = "offset" duration
///   group_by = "by" "(" key ("," key)* ")"
///
/// Examples:
///   usage{model="claude-opus-4-6"}[5m] by (model)
///   usage{project="myapp", since="20260301"}[1h]
///   events{session="abc123", since="20260301"}
///   usage[1d] offset 7d
///   sum(usage[1d])
///   avg(usage[1d]) by (project)
///   count(usage{since="20260301"}[1d])
///   sessions{project="myapp"}
///   projects

/// Aggregation function for collapsing model dimension.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AggregationFunc {
    /// Sum all token fields and event counts across models.
    Sum,
    /// Average per event (sum / event_count).
    Avg,
    /// Count of events (token fields zeroed).
    Count,
}

/// Query metric type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Metric {
    /// Token usage aggregation.
    Usage,
    /// List sessions.
    Sessions,
    /// List projects.
    Projects,
    /// Raw event data (individual API calls).
    Events,
}

/// Parsed label filter.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelFilter {
    pub key: String,
    pub value: String,
}

/// Parsed time bucket duration in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket(pub u64);

impl Bucket {
    pub fn as_secs(&self) -> u64 {
        self.0
    }

    /// Format a bucket label from an epoch timestamp (seconds) in the given timezone.
    /// Produces ISO-like labels: "2026-03-10T14:05:00" for sub-day,
    /// "2026-03-10" for day+.
    ///
    /// For day+ buckets, flooring is done in the user's local timezone so that
    /// date boundaries align with local midnight rather than UTC midnight.
    pub fn format_label(&self, epoch_secs: i64, tz: Option<chrono_tz::Tz>) -> String {
        let bucket_secs = self.0 as i64;

        if bucket_secs >= 86400 {
            if let Some(tz) = tz {
                // Day+ buckets: floor in local timezone
                let dt = chrono::DateTime::from_timestamp(epoch_secs, 0)
                    .unwrap_or_default()
                    .with_timezone(&tz);
                let local_date = dt.date_naive();
                format!("{}", local_date.format("%Y-%m-%d"))
            } else {
                let floored = (epoch_secs / bucket_secs) * bucket_secs;
                let dt = chrono::DateTime::from_timestamp(floored, 0).unwrap_or_default();
                dt.naive_utc().format("%Y-%m-%d").to_string()
            }
        } else {
            // Sub-day buckets: floor in UTC, then convert
            let floored = (epoch_secs / bucket_secs) * bucket_secs;
            let dt = chrono::DateTime::from_timestamp(floored, 0).unwrap_or_default();
            let naive = match tz {
                Some(tz) => dt.with_timezone(&tz).naive_local(),
                None => dt.naive_utc(),
            };
            naive.format("%Y-%m-%dT%H:%M:%S").to_string()
        }
    }
}

impl std::fmt::Display for Bucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = self.0;
        let mut wrote = false;
        for &(unit, label) in &[(604800, "w"), (86400, "d"), (3600, "h"), (60, "m")] {
            if s >= unit {
                let n = s / unit;
                s %= unit;
                write!(f, "{}{}", n, label)?;
                wrote = true;
            }
        }
        if s > 0 || !wrote {
            write!(f, "{}s", s)?;
        }
        Ok(())
    }
}

/// Parsed query.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub metric: Metric,
    pub filters: Vec<LabelFilter>,
    pub bucket: Option<Bucket>,
    pub group_by: Vec<String>,
    /// Time range filter: since (inclusive). Format: YYYYMMDD or YYYYMMDDhhmmss.
    pub since: Option<String>,
    /// Time range filter: until (inclusive). Format: YYYYMMDD or YYYYMMDDhhmmss.
    pub until: Option<String>,
    /// Provider filter: if set, only query this provider's DB.
    pub provider: Option<String>,
    /// Offset modifier: shifts the time window back by this duration.
    pub offset: Option<Bucket>,
    /// Aggregation function: collapses model dimension.
    pub aggregation: Option<AggregationFunc>,
}

impl Query {
    /// Get filter value for a given key, if present.
    pub fn filter_value(&self, key: &str) -> Option<&str> {
        self.filters.iter()
            .find(|f| f.key == key)
            .map(|f| f.value.as_str())
    }

    /// Serialize back to PromQL-style query string.
    pub fn to_query_string(&self) -> String {
        let agg_prefix = match self.aggregation {
            Some(AggregationFunc::Sum) => "sum(",
            Some(AggregationFunc::Avg) => "avg(",
            Some(AggregationFunc::Count) => "count(",
            None => "",
        };

        let mut s = match self.metric {
            Metric::Usage => "usage".to_string(),
            Metric::Sessions => "sessions".to_string(),
            Metric::Projects => "projects".to_string(),
            Metric::Events => "events".to_string(),
        };

        // Collect all filters (including since/until)
        let mut filters: Vec<(&str, &str)> = self.filters.iter()
            .map(|f| (f.key.as_str(), f.value.as_str()))
            .collect();
        if let Some(ref provider) = self.provider {
            filters.push(("provider", provider));
        }
        if let Some(ref since) = self.since {
            filters.push(("since", since));
        }
        if let Some(ref until) = self.until {
            filters.push(("until", until));
        }
        if !filters.is_empty() {
            s.push('{');
            for (i, (k, v)) in filters.iter().enumerate() {
                if i > 0 { s.push_str(", "); }
                let escaped = v.replace('\\', "\\\\").replace('"', "\\\"");
                s.push_str(&format!("{}=\"{}\"", k, escaped));
            }
            s.push('}');
        }

        if let Some(ref bucket) = self.bucket {
            s.push_str(&format!("[{}]", bucket));
        }

        if let Some(ref offset) = self.offset {
            s.push_str(&format!(" offset {}", offset));
        }

        let agg_suffix = if self.aggregation.is_some() { ")" } else { "" };

        if !self.group_by.is_empty() {
            format!("{}{}{} by ({})", agg_prefix, s, agg_suffix, self.group_by.join(", "))
        } else {
            format!("{}{}{}", agg_prefix, s, agg_suffix)
        }
    }

    /// Serialize with a ReportGroupBy converted to a bucket.
    pub fn to_query_string_with_bucket(&self, group_by: crate::engine::ReportGroupBy) -> String {
        let bucket_str = match group_by {
            crate::engine::ReportGroupBy::Hour => "[1h]",
            crate::engine::ReportGroupBy::Date => "[1d]",
            crate::engine::ReportGroupBy::Week { .. } => "[1w]",
            crate::engine::ReportGroupBy::Month => "[30d]",
            crate::engine::ReportGroupBy::Year => "[365d]",
        };

        let base = self.to_query_string();
        // Insert bucket before " offset" or " by" or at end
        let insert_pos = base.find(" offset ")
            .or_else(|| base.find(" by "))
            .unwrap_or(base.len());
        format!("{}{}{}", &base[..insert_pos], bucket_str, &base[insert_pos..])
    }
}

const VALID_FILTER_KEYS: &[&str] = &["model", "session", "project", "since", "until", "provider"];
const VALID_GROUP_KEYS: &[&str] = &["model", "session", "project"];

/// Parse a PromQL-like query string.
pub fn parse(input: &str) -> Result<Query, String> {
    let mut p = Parser::new(input);

    // Optional aggregation function: sum( / avg( / count(
    p.skip_ws();
    let aggregation = if p.consume_literal("sum") {
        p.expect_char('(')?;
        Some(AggregationFunc::Sum)
    } else if p.consume_literal("avg") {
        p.expect_char('(')?;
        Some(AggregationFunc::Avg)
    } else if p.consume_literal("count") {
        p.expect_char('(')?;
        Some(AggregationFunc::Count)
    } else {
        None
    };

    // Parse metric name
    p.skip_ws();
    let metric = if p.consume_literal("usage") {
        Metric::Usage
    } else if p.consume_literal("events") {
        Metric::Events
    } else if p.consume_literal("sessions") {
        Metric::Sessions
    } else if p.consume_literal("projects") {
        Metric::Projects
    } else {
        return Err("query must start with 'usage', 'events', 'sessions', or 'projects'".into());
    };

    // Optional filters: { ... }
    let mut raw_filters = if p.peek() == Some('{') {
        p.parse_filters()?
    } else {
        Vec::new()
    };

    // Extract since/until/provider from filters into dedicated fields
    let since = extract_filter(&mut raw_filters, "since");
    let until = extract_filter(&mut raw_filters, "until");
    let provider = extract_filter(&mut raw_filters, "provider");

    // Optional bucket: [ ... ]
    let bucket = if p.peek() == Some('[') {
        Some(p.parse_bucket()?)
    } else {
        None
    };

    // Optional offset: offset <duration>
    p.skip_ws();
    let offset = if p.consume_literal("offset") {
        Some(p.parse_duration()?)
    } else {
        None
    };

    // Close aggregation: )
    if aggregation.is_some() {
        p.expect_char(')')?;
    }

    // Optional group_by: by ( ... )
    p.skip_ws();
    let group_by = if p.consume_literal("by") {
        p.parse_group_by()?
    } else {
        Vec::new()
    };

    p.skip_ws();
    if !p.is_eof() {
        return Err(format!("unexpected trailing input: '{}'", p.remaining()));
    }

    // Validate: sessions/projects/events don't support bucket or group_by
    if metric != Metric::Usage {
        if bucket.is_some() {
            return Err(format!("{:?} does not support time buckets", metric));
        }
        if !group_by.is_empty() {
            return Err(format!("{:?} does not support group by", metric));
        }
        if aggregation.is_some() {
            return Err(format!("{:?} does not support aggregation functions", metric));
        }
    }

    Ok(Query { metric, filters: raw_filters, bucket, group_by, since, until, provider, offset, aggregation })
}

/// Extract and remove a filter by key, returning its value if present.
fn extract_filter(filters: &mut Vec<LabelFilter>, key: &str) -> Option<String> {
    if let Some(pos) = filters.iter().position(|f| f.key == key) {
        Some(filters.remove(pos).value)
    } else {
        None
    }
}

struct Parser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Parser { input, pos: 0 }
    }

    fn remaining(&self) -> &str {
        &self.input[self.pos..]
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.input.len()
    }

    fn peek(&mut self) -> Option<char> {
        self.skip_ws();
        self.input[self.pos..].chars().next()
    }

    fn skip_ws(&mut self) {
        while self.pos < self.input.len() {
            let c = self.input.as_bytes()[self.pos];
            if c == b' ' || c == b'\t' || c == b'\n' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn consume_literal(&mut self, lit: &str) -> bool {
        self.skip_ws();
        if self.remaining().starts_with(lit) {
            // Make sure it's not a prefix of a longer identifier
            let after = self.pos + lit.len();
            if after < self.input.len() {
                let next = self.input.as_bytes()[after];
                if next.is_ascii_alphanumeric() || next == b'_' {
                    return false;
                }
            }
            self.pos += lit.len();
            true
        } else {
            false
        }
    }

    fn expect_char(&mut self, ch: char) -> Result<(), String> {
        self.skip_ws();
        match self.input[self.pos..].chars().next() {
            Some(c) if c == ch => {
                self.pos += c.len_utf8();
                Ok(())
            }
            Some(c) => Err(format!("expected '{}', found '{}'", ch, c)),
            None => Err(format!("expected '{}', found end of input", ch)),
        }
    }

    fn parse_ident(&mut self) -> Result<String, String> {
        self.skip_ws();
        let start = self.pos;
        while self.pos < self.input.len() {
            let c = self.input.as_bytes()[self.pos];
            if c.is_ascii_alphanumeric() || c == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err("expected identifier".into());
        }
        Ok(self.input[start..self.pos].to_string())
    }

    fn parse_quoted_string(&mut self) -> Result<String, String> {
        self.skip_ws();
        self.expect_char('"')?;
        let mut value = String::new();
        while self.pos < self.input.len() {
            let c = self.input.as_bytes()[self.pos];
            if c == b'\\' && self.pos + 1 < self.input.len() {
                let next = self.input.as_bytes()[self.pos + 1];
                match next {
                    b'"' | b'\\' => {
                        value.push(next as char);
                        self.pos += 2;
                        continue;
                    }
                    _ => {}
                }
            }
            if c == b'"' {
                self.pos += 1; // consume closing quote
                return Ok(value);
            }
            value.push(c as char);
            self.pos += 1;
        }
        Err("unterminated string".into())
    }

    fn parse_filters(&mut self) -> Result<Vec<LabelFilter>, String> {
        self.expect_char('{')?;
        let mut filters = Vec::new();

        // Handle empty {}
        if self.peek() == Some('}') {
            self.pos += 1;
            return Ok(filters);
        }

        loop {
            let key = self.parse_ident()?;
            if !VALID_FILTER_KEYS.contains(&key.as_str()) {
                return Err(format!("unknown filter key '{}' (valid: {})", key, VALID_FILTER_KEYS.join(", ")));
            }
            self.expect_char('=')?;
            let value = self.parse_quoted_string()?;
            if filters.iter().any(|f| f.key == key) {
                return Err(format!("duplicate filter key '{}'", key));
            }
            filters.push(LabelFilter { key, value });

            match self.peek() {
                Some(',') => { self.pos += 1; }
                Some('}') => { self.pos += 1; break; }
                Some(c) => return Err(format!("expected ',' or '}}', found '{}'", c)),
                None => return Err("unterminated filter block".into()),
            }
        }
        Ok(filters)
    }

    /// Parse a duration: single (e.g. "5m", "1d") or compound (e.g. "2h30m", "1d12h").
    /// Units must appear in descending order (w > d > h > m > s) with no duplicates.
    fn parse_duration(&mut self) -> Result<Bucket, String> {
        self.skip_ws();

        fn unit_rank(c: char) -> Option<(u64, u8)> {
            match c {
                'w' => Some((604800, 5)),
                'd' => Some((86400, 4)),
                'h' => Some((3600, 3)),
                'm' => Some((60, 2)),
                's' => Some((1, 1)),
                _ => None,
            }
        }

        let mut total_secs: u64 = 0;
        let mut last_rank: u8 = u8::MAX;
        let mut parsed_any = false;

        loop {
            // Try to parse a number
            let start = self.pos;
            while self.pos < self.input.len() && self.input.as_bytes()[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
            if self.pos == start {
                if !parsed_any {
                    return Err("expected number in duration".into());
                }
                break;
            }
            let num: u64 = self.input[start..self.pos].parse()
                .map_err(|_| "invalid duration number")?;
            if num == 0 && !parsed_any {
                return Err("duration must be > 0".into());
            }

            // Parse unit
            let c = self.input[self.pos..].chars().next()
                .ok_or("expected duration unit (w/d/h/m/s)")?;
            let (multiplier, rank) = unit_rank(c)
                .ok_or_else(|| format!("unknown duration unit '{}' (use w/d/h/m/s)", c))?;
            if rank >= last_rank {
                return Err(format!("duration units must be in descending order, got '{}' after smaller/equal unit", c));
            }
            self.pos += 1;
            last_rank = rank;

            let secs = num.checked_mul(multiplier).ok_or("duration too large")?;
            total_secs = total_secs.checked_add(secs).ok_or("duration too large")?;
            parsed_any = true;
        }

        if total_secs == 0 {
            return Err("duration must be > 0".into());
        }
        Ok(Bucket(total_secs))
    }

    fn parse_bucket(&mut self) -> Result<Bucket, String> {
        self.expect_char('[')?;
        let result = self.parse_duration()?;
        self.expect_char(']')?;
        Ok(result)
    }

    fn parse_group_by(&mut self) -> Result<Vec<String>, String> {
        self.expect_char('(')?;
        let mut keys = Vec::new();

        if self.peek() == Some(')') {
            self.pos += 1;
            return Ok(keys);
        }

        loop {
            let key = self.parse_ident()?;
            if !VALID_GROUP_KEYS.contains(&key.as_str()) {
                return Err(format!("unknown group key '{}' (valid: {})", key, VALID_GROUP_KEYS.join(", ")));
            }
            keys.push(key);

            match self.peek() {
                Some(',') => { self.pos += 1; }
                Some(')') => { self.pos += 1; break; }
                Some(c) => return Err(format!("expected ',' or ')', found '{}'", c)),
                None => return Err("unterminated group by".into()),
            }
        }
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bare_usage() {
        let q = parse("usage").unwrap();
        assert!(q.filters.is_empty());
        assert!(q.bucket.is_none());
        assert!(q.group_by.is_empty());
        assert!(q.since.is_none());
        assert!(q.until.is_none());
    }

    #[test]
    fn test_empty_filters() {
        let q = parse("usage{}").unwrap();
        assert!(q.filters.is_empty());
    }

    #[test]
    fn test_single_filter() {
        let q = parse(r#"usage{model="claude-opus-4-6"}"#).unwrap();
        assert_eq!(q.filters.len(), 1);
        assert_eq!(q.filters[0].key, "model");
        assert_eq!(q.filters[0].value, "claude-opus-4-6");
    }

    #[test]
    fn test_multiple_filters() {
        let q = parse(r#"usage{model="opus", project="myapp"}"#).unwrap();
        assert_eq!(q.filters.len(), 2);
        assert_eq!(q.filters[0].key, "model");
        assert_eq!(q.filters[1].key, "project");
        assert_eq!(q.filters[1].value, "myapp");
    }

    #[test]
    fn test_bucket() {
        let q = parse("usage[5m]").unwrap();
        assert_eq!(q.bucket, Some(Bucket(300)));
    }

    #[test]
    fn test_bucket_units() {
        assert_eq!(parse("usage[30s]").unwrap().bucket, Some(Bucket(30)));
        assert_eq!(parse("usage[1h]").unwrap().bucket, Some(Bucket(3600)));
        assert_eq!(parse("usage[1d]").unwrap().bucket, Some(Bucket(86400)));
        assert_eq!(parse("usage[2w]").unwrap().bucket, Some(Bucket(1209600)));
    }

    #[test]
    fn test_group_by() {
        let q = parse("usage by (model)").unwrap();
        assert_eq!(q.group_by, vec!["model"]);
    }

    #[test]
    fn test_group_by_multiple() {
        let q = parse("usage by (model, project)").unwrap();
        assert_eq!(q.group_by, vec!["model", "project"]);
    }

    #[test]
    fn test_full_query() {
        let q = parse(r#"usage{model="claude-opus-4-6", project="myapp"}[5m] by (model)"#).unwrap();
        assert_eq!(q.filters.len(), 2);
        assert_eq!(q.bucket, Some(Bucket(300)));
        assert_eq!(q.group_by, vec!["model"]);
    }

    #[test]
    fn test_whitespace_tolerance() {
        let q = parse(r#"  usage  { model = "opus" }  [ 1h ]  by  ( model , project )  "#).unwrap();
        assert_eq!(q.filters[0].value, "opus");
        assert_eq!(q.bucket, Some(Bucket(3600)));
        assert_eq!(q.group_by, vec!["model", "project"]);
    }

    #[test]
    fn test_invalid_filter_key() {
        assert!(parse(r#"usage{unknown="value"}"#).is_err());
    }

    #[test]
    fn test_invalid_group_key() {
        assert!(parse("usage by (unknown)").is_err());
    }

    #[test]
    fn test_invalid_bucket_unit() {
        assert!(parse("usage[5x]").is_err());
    }

    #[test]
    fn test_zero_bucket() {
        assert!(parse("usage[0m]").is_err());
    }

    #[test]
    fn test_missing_metric() {
        assert!(parse("{model=\"opus\"}").is_err());
    }

    #[test]
    fn test_trailing_input() {
        assert!(parse("usage extra").is_err());
    }

    #[test]
    fn test_unterminated_string() {
        assert!(parse(r#"usage{model="unterminated}"#).is_err());
    }

    #[test]
    fn test_bucket_display() {
        assert_eq!(Bucket(300).to_string(), "5m");
        assert_eq!(Bucket(3600).to_string(), "1h");
        assert_eq!(Bucket(86400).to_string(), "1d");
        assert_eq!(Bucket(604800).to_string(), "1w");
        assert_eq!(Bucket(90).to_string(), "1m30s");
        assert_eq!(Bucket(7200).to_string(), "2h");
    }

    #[test]
    fn test_filter_value() {
        let q = parse(r#"usage{model="opus", project="myapp"}"#).unwrap();
        assert_eq!(q.filter_value("model"), Some("opus"));
        assert_eq!(q.filter_value("project"), Some("myapp"));
        assert_eq!(q.filter_value("session"), None);
    }

    #[test]
    fn test_session_filter() {
        let q = parse(r#"usage{session="4de929"}"#).unwrap();
        assert_eq!(q.filter_value("session"), Some("4de929"));
    }

    #[test]
    fn test_since_until() {
        let q = parse(r#"usage{since="20260301", until="20260310"}"#).unwrap();
        assert_eq!(q.since, Some("20260301".to_string()));
        assert_eq!(q.until, Some("20260310".to_string()));
        // since/until should be extracted from filters, not remain as label filters
        assert!(q.filters.is_empty());
    }

    #[test]
    fn test_since_with_other_filters() {
        let q = parse(r#"usage{model="opus", since="20260301"}[1h] by (model)"#).unwrap();
        assert_eq!(q.since, Some("20260301".to_string()));
        assert_eq!(q.until, None);
        assert_eq!(q.filters.len(), 1);
        assert_eq!(q.filters[0].key, "model");
        assert_eq!(q.bucket, Some(Bucket(3600)));
    }

    #[test]
    fn test_since_precise() {
        let q = parse(r#"usage{since="20260301120000"}"#).unwrap();
        assert_eq!(q.since, Some("20260301120000".to_string()));
    }

    #[test]
    fn test_sessions_metric() {
        let q = parse("sessions").unwrap();
        assert_eq!(q.metric, Metric::Sessions);
        assert!(q.filters.is_empty());
    }

    #[test]
    fn test_sessions_with_filter() {
        let q = parse(r#"sessions{project="myapp"}"#).unwrap();
        assert_eq!(q.metric, Metric::Sessions);
        assert_eq!(q.filter_value("project"), Some("myapp"));
    }

    #[test]
    fn test_projects_metric() {
        let q = parse("projects").unwrap();
        assert_eq!(q.metric, Metric::Projects);
    }

    #[test]
    fn test_sessions_rejects_bucket() {
        assert!(parse("sessions[5m]").is_err());
    }

    #[test]
    fn test_projects_rejects_group_by() {
        assert!(parse("projects by (model)").is_err());
    }

    #[test]
    fn test_escaped_quote_in_filter() {
        let q = parse(r#"usage{model="test\"value"}"#).unwrap();
        assert_eq!(q.filter_value("model"), Some("test\"value"));
    }

    #[test]
    fn test_escaped_backslash_in_filter() {
        let q = parse(r#"usage{project="path\\dir"}"#).unwrap();
        assert_eq!(q.filter_value("project"), Some("path\\dir"));
    }

    #[test]
    fn test_duplicate_filter_key() {
        let err = parse(r#"usage{model="a", model="b"}"#).unwrap_err();
        assert!(err.contains("duplicate filter key"));
    }

    #[test]
    fn test_provider_filter() {
        let q = parse(r#"usage{provider="claude_code"}"#).unwrap();
        assert_eq!(q.provider, Some("claude_code".to_string()));
        // provider should be extracted from filters, not remain as a label filter
        assert!(q.filters.is_empty());
    }

    #[test]
    fn test_provider_filter_with_other_filters() {
        let q = parse(r#"usage{provider="codex", model="gpt-5.4"}[1h] by (model)"#).unwrap();
        assert_eq!(q.provider, Some("codex".to_string()));
        assert_eq!(q.filters.len(), 1);
        assert_eq!(q.filters[0].key, "model");
        assert_eq!(q.bucket, Some(Bucket(3600)));
        assert_eq!(q.group_by, vec!["model"]);
    }

    #[test]
    fn test_sessions_with_provider_filter() {
        let q = parse(r#"sessions{provider="claude_code"}"#).unwrap();
        assert_eq!(q.metric, Metric::Sessions);
        assert_eq!(q.provider, Some("claude_code".to_string()));
    }

    #[test]
    fn test_group_by_provider_rejected() {
        // "provider" is not a valid group key — per-provider results are implicit
        assert!(parse("usage by (provider)").is_err());
    }

    #[test]
    fn test_group_by_model_and_provider_rejected() {
        // "provider" is not a valid group key — per-provider results are implicit
        assert!(parse("usage by (model, provider)").is_err());
    }

    #[test]
    fn test_provider_filter_serialization() {
        let q = parse(r#"usage{provider="claude_code", model="opus"}"#).unwrap();
        let s = q.to_query_string();
        assert!(s.contains(r#"provider="claude_code""#));
        assert!(s.contains(r#"model="opus""#));
    }

    // ── events metric ──

    #[test]
    fn test_events_metric() {
        let q = parse("events").unwrap();
        assert_eq!(q.metric, Metric::Events);
        assert!(q.filters.is_empty());
    }

    #[test]
    fn test_events_with_filters() {
        let q = parse(r#"events{session="abc123", since="20260301"}"#).unwrap();
        assert_eq!(q.metric, Metric::Events);
        assert_eq!(q.filter_value("session"), Some("abc123"));
        assert_eq!(q.since, Some("20260301".to_string()));
    }

    #[test]
    fn test_events_rejects_bucket() {
        assert!(parse("events[5m]").is_err());
    }

    #[test]
    fn test_events_rejects_group_by() {
        assert!(parse("events by (model)").is_err());
    }

    #[test]
    fn test_events_with_offset() {
        let q = parse("events offset 1d").unwrap();
        assert_eq!(q.metric, Metric::Events);
        assert_eq!(q.offset, Some(Bucket(86400)));
    }

    #[test]
    fn test_events_serialization() {
        let q = parse(r#"events{model="opus", since="20260301"}"#).unwrap();
        let s = q.to_query_string();
        assert!(s.starts_with("events{"));
        assert!(s.contains(r#"model="opus""#));
    }

    // ── offset modifier ──

    #[test]
    fn test_offset_with_bucket() {
        let q = parse("usage[1h] offset 1d").unwrap();
        assert_eq!(q.bucket, Some(Bucket(3600)));
        assert_eq!(q.offset, Some(Bucket(86400)));
    }

    #[test]
    fn test_offset_without_bucket() {
        let q = parse("usage offset 7d").unwrap();
        assert!(q.bucket.is_none());
        assert_eq!(q.offset, Some(Bucket(604800)));
    }

    #[test]
    fn test_offset_with_group_by() {
        let q = parse("usage[1d] offset 7d by (model)").unwrap();
        assert_eq!(q.bucket, Some(Bucket(86400)));
        assert_eq!(q.offset, Some(Bucket(604800)));
        assert_eq!(q.group_by, vec!["model"]);
    }

    #[test]
    fn test_offset_serialization() {
        let q = parse("usage[1h] offset 1d").unwrap();
        let s = q.to_query_string();
        assert_eq!(s, "usage[1h] offset 1d");
    }

    #[test]
    fn test_offset_serialization_with_group_by() {
        let q = parse("usage[1d] offset 7d by (model)").unwrap();
        let s = q.to_query_string();
        assert_eq!(s, "usage[1d] offset 1w by (model)");
    }

    #[test]
    fn test_to_query_string_with_bucket_preserves_offset() {
        let q = parse(r#"usage{since="20260301"} offset 7d"#).unwrap();
        let s = q.to_query_string_with_bucket(crate::engine::ReportGroupBy::Date);
        // bucket should be inserted before offset; 7d displays as 1w
        assert!(s.contains("[1d] offset 1w"));
    }

    // ── aggregation functions ──

    #[test]
    fn test_sum_aggregation() {
        let q = parse("sum(usage[1d])").unwrap();
        assert_eq!(q.metric, Metric::Usage);
        assert_eq!(q.aggregation, Some(super::AggregationFunc::Sum));
        assert_eq!(q.bucket, Some(Bucket(86400)));
    }

    #[test]
    fn test_avg_aggregation() {
        let q = parse("avg(usage[1d])").unwrap();
        assert_eq!(q.aggregation, Some(super::AggregationFunc::Avg));
    }

    #[test]
    fn test_count_aggregation() {
        let q = parse("count(usage[1d])").unwrap();
        assert_eq!(q.aggregation, Some(super::AggregationFunc::Count));
    }

    #[test]
    fn test_aggregation_with_filters() {
        let q = parse(r#"sum(usage{since="20260301"}[1d])"#).unwrap();
        assert_eq!(q.aggregation, Some(super::AggregationFunc::Sum));
        assert_eq!(q.since, Some("20260301".to_string()));
        assert_eq!(q.bucket, Some(Bucket(86400)));
    }

    #[test]
    fn test_aggregation_with_group_by() {
        let q = parse("sum(usage[1d]) by (project)").unwrap();
        assert_eq!(q.aggregation, Some(super::AggregationFunc::Sum));
        assert_eq!(q.group_by, vec!["project"]);
    }

    #[test]
    fn test_aggregation_with_offset() {
        let q = parse("sum(usage[1d] offset 7d)").unwrap();
        assert_eq!(q.aggregation, Some(super::AggregationFunc::Sum));
        assert_eq!(q.offset, Some(Bucket(604800)));
    }

    #[test]
    fn test_aggregation_bare_usage() {
        let q = parse("sum(usage)").unwrap();
        assert_eq!(q.aggregation, Some(super::AggregationFunc::Sum));
        assert!(q.bucket.is_none());
    }

    #[test]
    fn test_aggregation_rejects_sessions() {
        assert!(parse("sum(sessions)").is_err());
    }

    #[test]
    fn test_aggregation_rejects_events() {
        assert!(parse("count(events)").is_err());
    }

    #[test]
    fn test_no_aggregation() {
        let q = parse("usage[1d]").unwrap();
        assert_eq!(q.aggregation, None);
    }

    #[test]
    fn test_sum_serialization() {
        let q = parse("sum(usage[1d])").unwrap();
        assert_eq!(q.to_query_string(), "sum(usage[1d])");
    }

    #[test]
    fn test_avg_serialization_with_group_by() {
        let q = parse("avg(usage[1d]) by (project)").unwrap();
        assert_eq!(q.to_query_string(), "avg(usage[1d]) by (project)");
    }

    #[test]
    fn test_count_serialization_with_offset() {
        let q = parse("count(usage[1d] offset 7d)").unwrap();
        let s = q.to_query_string();
        assert_eq!(s, "count(usage[1d] offset 1w)");
    }

    // ── compound duration ──

    #[test]
    fn test_compound_duration() {
        assert_eq!(parse("usage[2h30m]").unwrap().bucket, Some(Bucket(9000)));
        assert_eq!(parse("usage[1d12h]").unwrap().bucket, Some(Bucket(129600)));
        assert_eq!(parse("usage[1h30s]").unwrap().bucket, Some(Bucket(3630)));
        assert_eq!(parse("usage[1w2d]").unwrap().bucket, Some(Bucket(777600)));
        assert_eq!(parse("usage[1d6h30m]").unwrap().bucket, Some(Bucket(109800)));
    }

    #[test]
    fn test_compound_duration_display() {
        assert_eq!(Bucket(9000).to_string(), "2h30m");
        assert_eq!(Bucket(129600).to_string(), "1d12h");
        assert_eq!(Bucket(3630).to_string(), "1h30s");
        assert_eq!(Bucket(109800).to_string(), "1d6h30m");
    }

    #[test]
    fn test_compound_duration_wrong_order() {
        assert!(parse("usage[30m2h]").is_err());
        assert!(parse("usage[1s30m]").is_err());
        assert!(parse("usage[1h2d]").is_err());
    }

    #[test]
    fn test_compound_duration_duplicate_unit() {
        assert!(parse("usage[1h2h]").is_err());
    }

    #[test]
    fn test_compound_duration_roundtrip() {
        let q = parse("usage[2h30m]").unwrap();
        assert_eq!(q.to_query_string(), "usage[2h30m]");
    }
}
