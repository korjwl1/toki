use std::path::Path;

use cursive::Cursive;
use cursive::event::Key;
use cursive::traits::*;
use cursive::views::{
    Checkbox, Dialog, EditView, LinearLayout, SelectView, TextView,
    DummyView, PaddedView,
};

use crate::db::Database;

struct SettingsState {
    claude_code_root: String,
    daemon_sock: String,
    timezone: String,
    output_format: String,
    start_of_week: String,
    no_cost: bool,
    retention_days: String,
    rollup_retention_days: String,
}

impl SettingsState {
    fn load(db: &Database) -> Self {
        let get = |key: &str, default: &str| -> String {
            db.get_setting(key).ok().flatten().unwrap_or_else(|| default.to_string())
        };
        let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));

        SettingsState {
            claude_code_root: get("claude_code_root", &home.join(".claude").to_string_lossy()),
            daemon_sock: get("daemon_sock", &home.join(".config/clitrace/daemon.sock").to_string_lossy()),
            timezone: get("timezone", ""),
            output_format: get("output_format", "table"),
            start_of_week: get("start_of_week", "mon"),
            no_cost: get("no_cost", "false") == "true",
            retention_days: get("retention_days", "0"),
            rollup_retention_days: get("rollup_retention_days", "0"),
        }
    }
}

pub fn run_settings(db_path: &Path) {
    let db = match Database::open(db_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[clitrace] Failed to open database: {}", e);
            std::process::exit(1);
        }
    };

    let state = SettingsState::load(&db);

    let mut siv = cursive::default();

    // Build form
    let form = LinearLayout::vertical()
        // Paths
        .child(field_label("Claude Code Root"))
        .child(EditView::new().content(&state.claude_code_root).with_name("claude_code_root").full_width())
        .child(DummyView)
        .child(field_label("Daemon Socket"))
        .child(EditView::new().content(&state.daemon_sock).with_name("daemon_sock").full_width())
        .child(DummyView)
        // Timezone
        .child(field_label("Timezone (IANA, empty=UTC)"))
        .child(EditView::new().content(&state.timezone).with_name("timezone").full_width())
        .child(DummyView)
        // Output format
        .child(field_label("Output Format"))
        .child({
            let mut sv = SelectView::new();
            sv.add_item("table", "table".to_string());
            sv.add_item("json", "json".to_string());
            let idx = if state.output_format == "json" { 1 } else { 0 };
            sv.set_selection(idx);
            sv.with_name("output_format")
        })
        .child(DummyView)
        // Start of week
        .child(field_label("Start of Week"))
        .child({
            let days = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
            let mut sv = SelectView::new();
            for d in &days {
                sv.add_item(*d, d.to_string());
            }
            let idx = days.iter().position(|&d| d == state.start_of_week).unwrap_or(0);
            sv.set_selection(idx);
            sv.with_name("start_of_week")
        })
        .child(DummyView)
        // No cost
        .child(
            LinearLayout::horizontal()
                .child(Checkbox::new().with_checked(state.no_cost).with_name("no_cost"))
                .child(TextView::new("  Disable cost calculation"))
        )
        .child(DummyView)
        // Retention
        .child(field_label("Event Retention Days (0=disabled)"))
        .child(EditView::new().content(&state.retention_days).with_name("retention_days").full_width())
        .child(DummyView)
        .child(field_label("Rollup Retention Days (0=disabled)"))
        .child(EditView::new().content(&state.rollup_retention_days).with_name("rollup_retention_days").full_width());

    let padded = PaddedView::lrtb(2, 2, 1, 1, form);

    let db_path_owned = db_path.to_path_buf();

    siv.add_layer(
        Dialog::around(padded)
            .title("clitrace Settings")
            .button("Save", move |s| {
                save_settings(s, &db_path_owned);
            })
            .button("Cancel", |s| s.quit())
            .min_width(60)
    );

    // Esc to quit
    siv.add_global_callback(Key::Esc, |s| s.quit());

    siv.run();
}

fn field_label(text: &str) -> TextView {
    TextView::new(text)
}

fn save_settings(siv: &mut Cursive, db_path: &Path) {
    let claude_root = get_edit(siv, "claude_code_root");
    let daemon_sock = get_edit(siv, "daemon_sock");
    let timezone = get_edit(siv, "timezone");
    let retention = get_edit(siv, "retention_days");
    let rollup_retention = get_edit(siv, "rollup_retention_days");

    let output_format = siv.call_on_name("output_format", |v: &mut SelectView<String>| {
        v.selection().map(|s| (*s).clone()).unwrap_or_else(|| "table".to_string())
    }).unwrap_or_else(|| "table".to_string());

    let start_of_week = siv.call_on_name("start_of_week", |v: &mut SelectView<String>| {
        v.selection().map(|s| (*s).clone()).unwrap_or_else(|| "mon".to_string())
    }).unwrap_or_else(|| "mon".to_string());

    let no_cost = siv.call_on_name("no_cost", |v: &mut Checkbox| {
        v.is_checked()
    }).unwrap_or(false);

    // Validate timezone
    if !timezone.is_empty()
        && timezone.parse::<chrono_tz::Tz>().is_err() {
            siv.add_layer(Dialog::info(format!("Invalid timezone: {}", timezone)));
            return;
        }

    // Validate retention days
    if retention.parse::<u32>().is_err() {
        siv.add_layer(Dialog::info("Retention days must be a number"));
        return;
    }
    if rollup_retention.parse::<u32>().is_err() {
        siv.add_layer(Dialog::info("Rollup retention days must be a number"));
        return;
    }

    // Save to DB
    let db = match Database::open(db_path) {
        Ok(d) => d,
        Err(e) => {
            siv.add_layer(Dialog::info(format!("DB error: {}", e)));
            return;
        }
    };

    let settings = [
        ("claude_code_root", claude_root.as_str()),
        ("daemon_sock", daemon_sock.as_str()),
        ("timezone", timezone.as_str()),
        ("output_format", output_format.as_str()),
        ("start_of_week", start_of_week.as_str()),
        ("no_cost", if no_cost { "true" } else { "false" }),
        ("retention_days", retention.as_str()),
        ("rollup_retention_days", rollup_retention.as_str()),
    ];

    for (key, value) in &settings {
        if let Err(e) = db.set_setting(key, value) {
            siv.add_layer(Dialog::info(format!("Failed to save {}: {}", key, e)));
            return;
        }
    }

    siv.add_layer(Dialog::info("Settings saved.").button("OK", |s| {
        s.pop_layer();
        s.quit();
    }));
}

fn get_edit(siv: &mut Cursive, name: &str) -> String {
    siv.call_on_name(name, |v: &mut EditView| {
        v.get_content().to_string()
    }).unwrap_or_default()
}
