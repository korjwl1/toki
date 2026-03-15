
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
    fn load() -> Self {
        let get = |key: &str, default: &str| -> String {
            crate::config::get_setting(key).unwrap_or_else(|| default.to_string())
        };
        let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));

        SettingsState {
            claude_code_root: get("claude_code_root", &home.join(".claude").to_string_lossy()),
            daemon_sock: get("daemon_sock", &home.join(".config/toki/daemon.sock").to_string_lossy()),
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

    // Inherit terminal colors for everything — don't fight the user's theme
    theme.palette[PaletteColor::Background] = Color::TerminalDefault;
    theme.palette[PaletteColor::View] = Color::TerminalDefault;
    theme.palette[PaletteColor::Primary] = Color::TerminalDefault;
    theme.palette[PaletteColor::Secondary] = Color::TerminalDefault;
    theme.palette[PaletteColor::Tertiary] = Color::TerminalDefault;

    // Only accent: titles in cyan
    theme.palette[PaletteColor::TitlePrimary] = Color::Light(BaseColor::Cyan);
    theme.palette[PaletteColor::TitleSecondary] = Color::Dark(BaseColor::Cyan);

    // Highlight: reverse video (works on any terminal theme)
    theme.palette[PaletteColor::Highlight] = Color::Dark(BaseColor::Cyan);
    theme.palette[PaletteColor::HighlightInactive] = Color::TerminalDefault;
    theme.palette[PaletteColor::HighlightText] = Color::Dark(BaseColor::Black);

    theme
}

/// Field label — uses terminal default color (bold via content)
fn field_label(text: &str) -> TextView {
    TextView::new(text)
}

/// Hint text — dimmed cyan, visible on both light and dark terminals
fn hint_text(text: &str) -> TextView {
    let mut s = StyledString::new();
    s.append_styled(text, ColorStyle::new(
        ColorType::Color(Color::Dark(BaseColor::Cyan)),
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

pub fn run_settings() {
    let state = SettingsState::load();

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
        .child(Panel::new(PaddedView::new(Margins::lrtb(1, 1, 1, 0), paths_section)).title("Paths"))
        .child(DummyView.fixed_height(1))
        .child(Panel::new(PaddedView::new(Margins::lrtb(1, 1, 1, 0), display_section)).title("Display"))
        .child(DummyView.fixed_height(1))
        .child(Panel::new(PaddedView::new(Margins::lrtb(1, 1, 1, 0), data_section)).title("Data"));

    let padded = PaddedView::lrtb(1, 1, 0, 0, form);


    // Footer hint — cyan for keys, terminal default for descriptions
    let cyan = ColorStyle::new(
        ColorType::Color(Color::Light(BaseColor::Cyan)),
        ColorType::InheritParent,
    );
    let footer = {
        let mut s = StyledString::new();
        s.append_styled("Tab", cyan);
        s.append_plain(" navigate  ");
        s.append_styled("Enter", cyan);
        s.append_plain(" select  ");
        s.append_styled("Esc", cyan);
        s.append_plain(" cancel");
        TextView::new(s)
    };

    let main_layout = LinearLayout::vertical()
        .child(padded)
        .child(DummyView.fixed_height(1))
        .child(PaddedView::lrtb(2, 0, 0, 0, footer));

    siv.add_layer(
        Dialog::around(main_layout)
            .title("toki settings")
            .button("Save", move |s| {
                save_settings(s);
            })
            .button("Cancel", |s| s.quit())
            .min_width(70)
    );

    // Esc to quit
    siv.add_global_callback(Key::Esc, |s| s.quit());

    siv.run();
}

fn save_settings(siv: &mut Cursive) {
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

    // Load old values to detect daemon-affecting changes
    let daemon_keys = ["claude_code_root", "daemon_sock", "retention_days", "rollup_retention_days"];
    let old_values: Vec<(String, String)> = daemon_keys.iter()
        .map(|k| (k.to_string(), crate::config::get_setting(k).unwrap_or_default()))
        .collect();

    // Save to settings file
    for (key, value) in &settings {
        if let Err(e) = crate::config::set_setting(key, value) {
            siv.add_layer(Dialog::info(format!("Failed to save {}: {}", key, e)));
            return;
        }
    }

    // Check if any daemon-affecting setting changed
    let daemon_changed = old_values.iter().any(|(k, old_v)| {
        settings.iter().any(|(sk, sv)| sk == k && sv != old_v)
    });

    let pidfile = crate::daemon::default_pidfile_path();
    let daemon_running = crate::daemon::daemon_status(&pidfile).is_some();

    if daemon_changed && daemon_running {
        siv.add_layer(
            Dialog::text("Settings saved.\n\nDaemon-affecting settings changed.\nRestart daemon now?")
                .title("Restart Required")
                .button("Yes", |s| {
                    s.quit();
                    // Restart daemon after TUI exits
                    // Signal via exit code or env var
                    std::env::set_var("TOKI_RESTART_DAEMON", "1");
                })
                .button("No", |s| {
                    s.pop_layer();
                    s.add_layer(Dialog::info("Run `toki daemon restart` to apply.").button("OK", |s| {
                        s.pop_layer();
                        s.quit();
                    }));
                })
        );
    } else {
        siv.add_layer(Dialog::info("Settings saved.").button("OK", |s| {
            s.pop_layer();
            s.quit();
        }));
    }
}

fn get_edit(siv: &mut Cursive, name: &str) -> String {
    siv.call_on_name(name, |v: &mut EditView| {
        v.get_content().to_string()
    }).unwrap_or_default()
}
