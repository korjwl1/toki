/// Minimal PromQL-inspired query parser for toki.
///
/// Grammar:
///   query   = metric filters? bucket? group_by?
///   metric  = "usage" | "sessions" | "projects"
///   filters = "{" (filter ("," filter)*)? "}"
///   filter  = key "=" quoted_string
///   bucket  = "[" duration "]"
///   group_by = "by" "(" key ("," key)* ")"
///
/// Examples:
///   usage{model="claude-opus-4-6"}[5m] by (model)
///   usage{project="myapp", since="20260301"}[1h]
///   sessions{project="myapp"}
///   projects

/// Query metric type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Metric {
    /// Token usage aggregation.
    Usage,
    /// List sessions.
    Sessions,
    /// List projects.
    Projects,
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

    /// Format a bucket label from an epoch timestamp.
    /// Produces ISO-like labels: "2026-03-10T14:05:00" for sub-day,
    /// "2026-03-10" for day+.
    pub fn format_label(&self, epoch_secs: i64) -> String {
        let floored = (epoch_secs / self.0 as i64) * self.0 as i64;
        let dt = chrono::DateTime::from_timestamp(floored, 0).unwrap_or_default();
        let naive = dt.naive_utc();
        if self.0 >= 86400 {
            naive.format("%Y-%m-%d").to_string()
        } else {
            naive.format("%Y-%m-%dT%H:%M:%S").to_string()
        }
    }
}

impl std::fmt::Display for Bucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.0;
        if s >= 604800 && s % 604800 == 0 {
            write!(f, "{}w", s / 604800)
        } else if s >= 86400 && s % 86400 == 0 {
            write!(f, "{}d", s / 86400)
        } else if s >= 3600 && s % 3600 == 0 {
            write!(f, "{}h", s / 3600)
        } else if s >= 60 && s % 60 == 0 {
            write!(f, "{}m", s / 60)
        } else {
            write!(f, "{}s", s)
        }
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
}

impl Query {
    /// Get filter value for a given key, if present.
    pub fn filter_value(&self, key: &str) -> Option<&str> {
        self.filters.iter()
            .find(|f| f.key == key)
            .map(|f| f.value.as_str())
    }
}

const VALID_FILTER_KEYS: &[&str] = &["model", "session", "project", "since", "until"];
const VALID_GROUP_KEYS: &[&str] = &["model", "session", "project"];

/// Parse a PromQL-like query string.
pub fn parse(input: &str) -> Result<Query, String> {
    let mut p = Parser::new(input);

    // Parse metric name
    p.skip_ws();
    let metric = if p.consume_literal("usage") {
        Metric::Usage
    } else if p.consume_literal("sessions") {
        Metric::Sessions
    } else if p.consume_literal("projects") {
        Metric::Projects
    } else {
        return Err("query must start with 'usage', 'sessions', or 'projects'".into());
    };

    // Optional filters: { ... }
    let mut raw_filters = if p.peek() == Some('{') {
        p.parse_filters()?
    } else {
        Vec::new()
    };

    // Extract since/until from filters into dedicated fields
    let since = extract_filter(&mut raw_filters, "since");
    let until = extract_filter(&mut raw_filters, "until");

    // Optional bucket: [ ... ]
    let bucket = if p.peek() == Some('[') {
        Some(p.parse_bucket()?)
    } else {
        None
    };

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

    // Validate: sessions/projects don't support bucket or group_by
    if metric != Metric::Usage {
        if bucket.is_some() {
            return Err(format!("{:?} does not support time buckets", metric));
        }
        if !group_by.is_empty() {
            return Err(format!("{:?} does not support group by", metric));
        }
    }

    Ok(Query { metric, filters: raw_filters, bucket, group_by, since, until })
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
        let start = self.pos;
        while self.pos < self.input.len() {
            let c = self.input.as_bytes()[self.pos];
            if c == b'"' {
                let value = self.input[start..self.pos].to_string();
                self.pos += 1; // consume closing quote
                return Ok(value);
            }
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

    fn parse_bucket(&mut self) -> Result<Bucket, String> {
        self.expect_char('[')?;
        self.skip_ws();

        let start = self.pos;
        while self.pos < self.input.len() && self.input.as_bytes()[self.pos].is_ascii_digit() {
            self.pos += 1;
        }
        if self.pos == start {
            return Err("expected number in bucket".into());
        }
        let num: u64 = self.input[start..self.pos].parse()
            .map_err(|_| "invalid bucket number")?;
        if num == 0 {
            return Err("bucket duration must be > 0".into());
        }

        let unit = match self.input[self.pos..].chars().next() {
            Some('s') => { self.pos += 1; 1u64 }
            Some('m') => { self.pos += 1; 60 }
            Some('h') => { self.pos += 1; 3600 }
            Some('d') => { self.pos += 1; 86400 }
            Some('w') => { self.pos += 1; 604800 }
            Some(c) => return Err(format!("unknown duration unit '{}' (use s/m/h/d/w)", c)),
            None => return Err("expected duration unit".into()),
        };

        self.expect_char(']')?;
        Ok(Bucket(num * unit))
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
        assert_eq!(Bucket(90).to_string(), "90s");
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
}
