use std::path::Path;

use cursive::Cursive;
use cursive::event::Key;
use cursive::theme::{BaseColor, BorderStyle, Color, ColorStyle, ColorType, PaletteColor, Theme};
use cursive::traits::*;
use cursive::utils::markup::StyledString;
use cursive::view::Margins;
use cursive::views::{
    Checkbox, Dialog, EditView, LinearLayout, Panel, SelectView, TextView,
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

fn build_theme() -> Theme {
    let mut theme = Theme::terminal_default();
    theme.shadow = false;
    theme.borders = BorderStyle::Simple;

    // Dark background, clean foreground
    theme.palette[PaletteColor::Background] = Color::TerminalDefault;
    theme.palette[PaletteColor::View] = Color::TerminalDefault;
    theme.palette[PaletteColor::Primary] = Color::Light(BaseColor::White);
    theme.palette[PaletteColor::Secondary] = Color::Dark(BaseColor::White);
    theme.palette[PaletteColor::Tertiary] = Color::Dark(BaseColor::White);

    // Titles: cyan accent
    theme.palette[PaletteColor::TitlePrimary] = Color::Light(BaseColor::Cyan);
    theme.palette[PaletteColor::TitleSecondary] = Color::Dark(BaseColor::Cyan);

    // Highlight: cyan bg, black text
    theme.palette[PaletteColor::Highlight] = Color::Dark(BaseColor::Cyan);
    theme.palette[PaletteColor::HighlightInactive] = Color::Dark(BaseColor::White);
    theme.palette[PaletteColor::HighlightText] = Color::Dark(BaseColor::Black);

    theme
}

/// Styled field label (bright)
fn field_label(text: &str) -> TextView {
    let mut s = StyledString::new();
    s.append_styled(text, ColorStyle::new(
        ColorType::Color(Color::Light(BaseColor::White)),
        ColorType::InheritParent,
    ));
    TextView::new(s)
}

/// Styled hint text (dim)
fn hint_text(text: &str) -> TextView {
    let mut s = StyledString::new();
    s.append_styled(text, ColorStyle::new(
        ColorType::Color(Color::Dark(BaseColor::White)),
        ColorType::InheritParent,
    ));
    TextView::new(s)
}

/// Build a labeled row: "Label:  [EditView]"
fn labeled_edit(label: &str, value: &str, name: &str) -> LinearLayout {
    LinearLayout::horizontal()
        .child(field_label(&format!("{:<22}", label)))
        .child(EditView::new().content(value).with_name(name).full_width())
}

/// Build a labeled row with popup SelectView
fn labeled_select(label: &str, items: &[&str], current: &str, name: &str) -> LinearLayout {
    let mut sv = SelectView::new().popup();
    for &item in items {
        sv.add_item(item, item.to_string());
    }
    let idx = items.iter().position(|&d| d == current).unwrap_or(0);
    sv.set_selection(idx);

    LinearLayout::horizontal()
        .child(field_label(&format!("{:<22}", label)))
        .child(sv.with_name(name))
}

/// Build a labeled row with checkbox
fn labeled_checkbox(label: &str, checked: bool, name: &str) -> LinearLayout {
    LinearLayout::horizontal()
        .child(field_label(&format!("{:<22}", label)))
        .child(Checkbox::new().with_checked(checked).with_name(name))
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
    siv.set_theme(build_theme());

    // -- Paths section --
    let paths_section = LinearLayout::vertical()
        .child(labeled_edit("Claude Code Root", &state.claude_code_root, "claude_code_root"))
        .child(DummyView.fixed_height(1))
        .child(labeled_edit("Daemon Socket", &state.daemon_sock, "daemon_sock"));

    // -- Display section --
    let display_section = LinearLayout::vertical()
        .child(labeled_edit("Timezone", &state.timezone, "timezone"))
        .child(hint_text("                        IANA format (e.g. Asia/Seoul), empty = UTC"))
        .child(DummyView.fixed_height(1))
        .child(labeled_select("Output Format", &["table", "json"], &state.output_format, "output_format"))
        .child(DummyView.fixed_height(1))
        .child(labeled_select("Start of Week", &["mon", "tue", "wed", "thu", "fri", "sat", "sun"], &state.start_of_week, "start_of_week"))
        .child(DummyView.fixed_height(1))
        .child(labeled_checkbox("Disable Cost", state.no_cost, "no_cost"));

    // -- Data section --
    let data_section = LinearLayout::vertical()
        .child(labeled_edit("Event Retention", &state.retention_days, "retention_days"))
        .child(hint_text("                        Days to keep events (0 = forever)"))
        .child(DummyView.fixed_height(1))
        .child(labeled_edit("Rollup Retention", &state.rollup_retention_days, "rollup_retention_days"))
        .child(hint_text("                        Days to keep rollups (0 = forever)"));

    // Assemble form with section panels
    let form = LinearLayout::vertical()
        .child(Panel::new(PaddedView::new(Margins::lrtb(1, 1, 0, 0), paths_section)).title("Paths"))
        .child(DummyView.fixed_height(1))
        .child(Panel::new(PaddedView::new(Margins::lrtb(1, 1, 0, 0), display_section)).title("Display"))
        .child(DummyView.fixed_height(1))
        .child(Panel::new(PaddedView::new(Margins::lrtb(1, 1, 0, 0), data_section)).title("Data"));

    let padded = PaddedView::lrtb(1, 1, 0, 0, form);

    let db_path_owned = db_path.to_path_buf();

    // Footer hint
    let footer = {
        let mut s = StyledString::new();
        s.append_styled("Tab", ColorStyle::new(
            ColorType::Color(Color::Light(BaseColor::Cyan)),
            ColorType::InheritParent,
        ));
        s.append_styled(" navigate  ", ColorStyle::new(
            ColorType::Color(Color::Dark(BaseColor::White)),
            ColorType::InheritParent,
        ));
        s.append_styled("Enter", ColorStyle::new(
            ColorType::Color(Color::Light(BaseColor::Cyan)),
            ColorType::InheritParent,
        ));
        s.append_styled(" select  ", ColorStyle::new(
            ColorType::Color(Color::Dark(BaseColor::White)),
            ColorType::InheritParent,
        ));
        s.append_styled("Esc", ColorStyle::new(
            ColorType::Color(Color::Light(BaseColor::Cyan)),
            ColorType::InheritParent,
        ));
        s.append_styled(" cancel", ColorStyle::new(
            ColorType::Color(Color::Dark(BaseColor::White)),
            ColorType::InheritParent,
        ));
        TextView::new(s)
    };

    let main_layout = LinearLayout::vertical()
        .child(padded)
        .child(DummyView.fixed_height(1))
        .child(PaddedView::lrtb(2, 0, 0, 0, footer));

    siv.add_layer(
        Dialog::around(main_layout)
            .title("clitrace settings")
            .button("Save", move |s| {
                save_settings(s, &db_path_owned);
            })
            .button("Cancel", |s| s.quit())
            .min_width(70)
    );

    // Esc to quit
    siv.add_global_callback(Key::Esc, |s| s.quit());

    siv.run();
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
