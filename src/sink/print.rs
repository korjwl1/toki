use std::collections::HashMap;

use comfy_table::{Table, ContentArrangement, Cell, Attribute, presets::UTF8_FULL};

use crate::common::types::{ModelUsageSummary, UsageEvent};
use crate::pricing::{PricingTable, format_cost};
use super::{Sink, json, format_source_label, shorten_id};

/// Output format for the print sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Table,
    Json,
}

/// Stdout sink: renders to terminal as table or JSON.
pub struct PrintSink {
    format: OutputFormat,
}

impl PrintSink {
    pub fn new(format: OutputFormat) -> Self {
        PrintSink { format }
    }
}

fn format_number(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let commas = if len > 0 { (len - 1) / 3 } else { 0 };
    let mut result = String::with_capacity(len + commas);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            result.push(',');
        }
        result.push(b as char);
    }
    result
}

impl Sink for PrintSink {
    fn emit_summary(&self, summaries: &HashMap<String, ModelUsageSummary>, pricing: Option<&PricingTable>) {
        if summaries.is_empty() {
            if self.format == OutputFormat::Json {
                println!("{}", serde_json::to_string_pretty(&json::summaries_to_json(summaries, pricing)).unwrap_or_default());
            } else {
                println!("[toki] No usage data found.");
            }
            return;
        }

        if self.format == OutputFormat::Json {
            println!("{}", serde_json::to_string_pretty(&json::summaries_to_json(summaries, pricing)).unwrap_or_default());
            return;
        }

        let mut sorted: Vec<_> = summaries.values().collect();
        sorted.sort_by(|a, b| b.event_count.cmp(&a.event_count));
        let has_pricing = pricing.is_some_and(|p| !p.is_empty());
        let has_precalc_cost = sorted.iter().any(|s| s.cost_usd.is_some());
        let show_cost = has_pricing || has_precalc_cost;

        let mut table = Table::new();
        table.load_preset(UTF8_FULL);
        table.set_content_arrangement(ContentArrangement::Dynamic);
        let mut header = vec![
            Cell::new("Model").add_attribute(Attribute::Bold),
            Cell::new("Input").add_attribute(Attribute::Bold),
            Cell::new("Output").add_attribute(Attribute::Bold),
            Cell::new("Cache\nCreate").add_attribute(Attribute::Bold),
            Cell::new("Cache\nRead").add_attribute(Attribute::Bold),
            Cell::new("Total\nTokens").add_attribute(Attribute::Bold),
            Cell::new("Events").add_attribute(Attribute::Bold),
        ];
        if show_cost {
            header.push(Cell::new("Cost\n(USD)").add_attribute(Attribute::Bold));
        }
        table.set_header(header);

        let mut total_input = 0u64;
        let mut total_output = 0u64;
        let mut total_cache_create = 0u64;
        let mut total_cache_read = 0u64;
        let mut total_events = 0u64;
        let mut total_cost = 0.0f64;

        for s in &sorted {
            let total = s.input_tokens + s.output_tokens + s.cache_creation_input_tokens + s.cache_read_input_tokens;
            let cost = pricing.and_then(|p| p.summary_cost(s)).or(s.cost_usd);
            let mut row = vec![
                Cell::new(&s.model),
                Cell::new(format_number(s.input_tokens)),
                Cell::new(format_number(s.output_tokens)),
                Cell::new(format_number(s.cache_creation_input_tokens)),
                Cell::new(format_number(s.cache_read_input_tokens)),
                Cell::new(format_number(total)),
                Cell::new(format_number(s.event_count)),
            ];
            if show_cost {
                row.push(Cell::new(format_cost(cost)));
                if let Some(c) = cost { total_cost += c; }
            }
            table.add_row(row);

            total_input += s.input_tokens;
            total_output += s.output_tokens;
            total_cache_create += s.cache_creation_input_tokens;
            total_cache_read += s.cache_read_input_tokens;
            total_events += s.event_count;
        }

        if sorted.len() > 1 {
            let grand_total = total_input + total_output + total_cache_create + total_cache_read;
            let mut row = vec![
                Cell::new("Total").add_attribute(Attribute::Bold),
                Cell::new(format_number(total_input)).add_attribute(Attribute::Bold),
                Cell::new(format_number(total_output)).add_attribute(Attribute::Bold),
                Cell::new(format_number(total_cache_create)).add_attribute(Attribute::Bold),
                Cell::new(format_number(total_cache_read)).add_attribute(Attribute::Bold),
                Cell::new(format_number(grand_total)).add_attribute(Attribute::Bold),
                Cell::new(format_number(total_events)).add_attribute(Attribute::Bold),
            ];
            if show_cost {
                row.push(Cell::new(format_cost(Some(total_cost))).add_attribute(Attribute::Bold));
            }
            table.add_row(row);
        }

        println!("[toki] Token Usage Summary");
        println!("{table}");
    }

    fn emit_grouped(&self, grouped: &HashMap<String, HashMap<String, ModelUsageSummary>>, type_name: &str, pricing: Option<&PricingTable>) {
        if grouped.is_empty() {
            if self.format == OutputFormat::Json {
                println!("{}", serde_json::to_string_pretty(&json::grouped_to_json(grouped, type_name, pricing)).unwrap_or_default());
            } else {
                println!("[toki] No usage data found.");
            }
            return;
        }

        if self.format == OutputFormat::Json {
            println!("{}", serde_json::to_string_pretty(&json::grouped_to_json(grouped, type_name, pricing)).unwrap_or_default());
            return;
        }

        let is_session = type_name == "session";
        let has_pricing = pricing.is_some_and(|p| !p.is_empty());
        let has_precalc_cost = grouped.values().any(|m| m.values().any(|s| s.cost_usd.is_some()));
        let show_cost = has_pricing || has_precalc_cost;
        let mut buckets: Vec<&String> = grouped.keys().collect();
        buckets.sort();

        let header_label = if is_session { "Session" } else { "Period" };
        let mut table = Table::new();
        table.load_preset(UTF8_FULL);
        table.set_content_arrangement(ContentArrangement::Dynamic);
        let mut header = vec![
            Cell::new(header_label).add_attribute(Attribute::Bold),
            Cell::new("Model").add_attribute(Attribute::Bold),
            Cell::new("Input").add_attribute(Attribute::Bold),
            Cell::new("Output").add_attribute(Attribute::Bold),
            Cell::new("Cache\nCreate").add_attribute(Attribute::Bold),
            Cell::new("Cache\nRead").add_attribute(Attribute::Bold),
            Cell::new("Total\nTokens").add_attribute(Attribute::Bold),
            Cell::new("Events").add_attribute(Attribute::Bold),
        ];
        if show_cost {
            header.push(Cell::new("Cost\n(USD)").add_attribute(Attribute::Bold));
        }
        table.set_header(header);

        let mut grand_input = 0u64;
        let mut grand_output = 0u64;
        let mut grand_cache_create = 0u64;
        let mut grand_cache_read = 0u64;
        let mut grand_events = 0u64;
        let mut grand_cost = 0.0f64;

        for bucket in &buckets {
            if let Some(models) = grouped.get(bucket.as_str()) {
                let mut sorted: Vec<_> = models.values().collect();
                sorted.sort_by(|a, b| b.event_count.cmp(&a.event_count));

                for (i, s) in sorted.iter().enumerate() {
                    let total = s.input_tokens + s.output_tokens + s.cache_creation_input_tokens + s.cache_read_input_tokens;
                    let cost = pricing.and_then(|p| p.summary_cost(s)).or(s.cost_usd);
                    let display_key = if is_session { shorten_id(bucket).to_string() } else { bucket.to_string() };
                    let period_cell = if i == 0 {
                        Cell::new(&display_key)
                    } else {
                        Cell::new("")
                    };
                    let mut row = vec![
                        period_cell,
                        Cell::new(&s.model),
                        Cell::new(format_number(s.input_tokens)),
                        Cell::new(format_number(s.output_tokens)),
                        Cell::new(format_number(s.cache_creation_input_tokens)),
                        Cell::new(format_number(s.cache_read_input_tokens)),
                        Cell::new(format_number(total)),
                        Cell::new(format_number(s.event_count)),
                    ];
                    if show_cost {
                        row.push(Cell::new(format_cost(cost)));
                        if let Some(c) = cost { grand_cost += c; }
                    }
                    table.add_row(row);

                    grand_input += s.input_tokens;
                    grand_output += s.output_tokens;
                    grand_cache_create += s.cache_creation_input_tokens;
                    grand_cache_read += s.cache_read_input_tokens;
                    grand_events += s.event_count;
                }
            }
        }

        let grand_total = grand_input + grand_output + grand_cache_create + grand_cache_read;
        let mut total_row = vec![
            Cell::new("Total").add_attribute(Attribute::Bold),
            Cell::new(""),
            Cell::new(format_number(grand_input)).add_attribute(Attribute::Bold),
            Cell::new(format_number(grand_output)).add_attribute(Attribute::Bold),
            Cell::new(format_number(grand_cache_create)).add_attribute(Attribute::Bold),
            Cell::new(format_number(grand_cache_read)).add_attribute(Attribute::Bold),
            Cell::new(format_number(grand_total)).add_attribute(Attribute::Bold),
            Cell::new(format_number(grand_events)).add_attribute(Attribute::Bold),
        ];
        if show_cost {
            total_row.push(Cell::new(format_cost(Some(grand_cost))).add_attribute(Attribute::Bold));
        }
        table.add_row(total_row);

        println!("[toki] Token Usage Summary");
        println!("{table}");
    }

    fn emit_list(&self, items: &[String], type_name: &str) {
        if self.format == OutputFormat::Json {
            let json = serde_json::json!({
                "type": type_name,
                "items": items,
            });
            println!("{}", serde_json::to_string_pretty(&json).unwrap_or_default());
            return;
        }

        if items.is_empty() {
            println!("[toki] No {} found.", type_name);
            return;
        }

        let col_name = if type_name == "sessions" { "Session ID" } else { "Project" };
        let mut table = Table::new();
        table.load_preset(UTF8_FULL);
        table.set_content_arrangement(ContentArrangement::Dynamic);
        table.set_header(vec![Cell::new(col_name).add_attribute(Attribute::Bold)]);

        for item in items {
            table.add_row(vec![Cell::new(item)]);
        }

        println!("[toki] {} ({})", type_name, items.len());
        println!("{table}");
    }

    fn emit_event(&self, event: &UsageEvent, pricing: Option<&PricingTable>) {
        if self.format == OutputFormat::Json {
            let json = json::event_to_json(event, pricing);
            println!("{}", serde_json::to_string(&json).unwrap_or_default());
            return;
        }

        let cost = pricing.and_then(|p| p.event_cost(event));
        let label = format_source_label(&event.source_file);
        match cost {
            Some(c) => println!(
                "[toki] {} | {} | in:{} cc:{} cr:{} out:{} | {}",
                event.model, label,
                event.input_tokens, event.cache_creation_input_tokens,
                event.cache_read_input_tokens, event.output_tokens,
                format_cost(Some(c)),
            ),
            None => println!(
                "[toki] {} | {} | in:{} cc:{} cr:{} out:{}",
                event.model, label,
                event.input_tokens, event.cache_creation_input_tokens,
                event.cache_read_input_tokens, event.output_tokens,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_number() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(123), "123");
        assert_eq!(format_number(1234), "1,234");
        assert_eq!(format_number(1234567), "1,234,567");
    }
}
