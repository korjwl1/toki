
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
    codex_root: String,
    daemon_sock: String,
    timezone: String,
    output_format: String,
    start_of_week: String,
    no_cost: bool,
    retention_days: String,
    rollup_retention_days: String,
    sync_enabled: bool,
    sync_server: String,
    sync_device_name: String,
}

impl SettingsState {
    fn load() -> Self {
        let get = |key: &str, default: &str| -> String {
            crate::config::get_setting(key).unwrap_or_else(|| default.to_string())
        };
        let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));

        SettingsState {
            claude_code_root: get("claude_code_root", &home.join(".claude").to_string_lossy()),
            codex_root: get("codex_root", &home.join(".codex").to_string_lossy()),
            daemon_sock: get("daemon_sock", &home.join(".config/toki/daemon.sock").to_string_lossy()),
            timezone: get("timezone", ""),
            output_format: get("output_format", "table"),
            start_of_week: get("start_of_week", "mon"),
            no_cost: get("no_cost", "false") == "true",
            retention_days: get("retention_days", "0"),
            rollup_retention_days: get("rollup_retention_days", "0"),
            sync_enabled: get("sync_enabled", "false") == "true",
            sync_server: get("sync_server", ""),
            sync_device_name: get("sync_device_name", &crate::sync::thread::SyncConfig::default_device_name()),
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

/// Run the settings TUI. Returns `true` if daemon restart was requested.
pub fn run_settings() -> bool {
    let state = SettingsState::load();

    let mut siv = cursive::default();
    siv.set_theme(build_theme());

    // Shared flag to signal daemon restart request
    let restart_requested = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // -- Paths section --
    let paths_section = LinearLayout::vertical()
        .child(labeled_edit("Claude Code Root", &state.claude_code_root, "claude_code_root"))
        .child(DummyView.fixed_height(1))
        .child(labeled_edit("Codex CLI Root", &state.codex_root, "codex_root"))
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

    // -- Sync section (read-only except device name) --
    let sync_status = if state.sync_enabled { "Enabled" } else { "Disabled" };
    let sync_server_display = if state.sync_server.is_empty() { "(not configured)" } else { &state.sync_server };

    let sync_section = LinearLayout::vertical()
        .child(LinearLayout::horizontal()
            .child(field_label(&format!("{:<22}", "Status")))
            .child(TextView::new(sync_status)))
        .child(DummyView.fixed_height(1))
        .child(LinearLayout::horizontal()
            .child(field_label(&format!("{:<22}", "Server")))
            .child(TextView::new(sync_server_display)))
        .child(DummyView.fixed_height(1))
        .child(labeled_edit("Device Name", &state.sync_device_name, "sync_device_name"))
        .child(hint_text("                        1-64 chars, no control characters"))
        .child(DummyView.fixed_height(1))
        .child(hint_text("  Manage sync via CLI:"))
        .child(hint_text("    toki settings sync enable --server <host>"))
        .child(hint_text("    toki settings sync disable [--delete | --keep]"));

    // -- Providers section (popup multi-select) --
    let enabled_providers = crate::config::get_providers();
    let providers_display = format_providers_display(&enabled_providers);

    let providers_section = LinearLayout::vertical()
        .child(LinearLayout::horizontal()
            .child(field_label(&format!("{:<22}", "Selected")))
            .child(TextView::new(providers_display).with_name("providers_display"))
        )
        .child(DummyView.fixed_height(1))
        .child(hint_text("                        Press Enter on [Change] to select providers"))
        .child(
            cursive::views::Button::new("Change Providers", move |s| {
                show_providers_popup(s);
            })
        );

    // Hidden storage for provider selection (comma-separated internal names)
    let providers_data = EditView::new()
        .content(enabled_providers.join(","))
        .with_name("providers_data")
        .fixed_width(0)
        .fixed_height(0);

    // Assemble form with section panels
    let form = LinearLayout::vertical()
        .child(providers_data)
        .child(Panel::new(PaddedView::new(Margins::lrtb(1, 1, 1, 0), providers_section)).title("Providers"))
        .child(DummyView.fixed_height(1))
        .child(Panel::new(PaddedView::new(Margins::lrtb(1, 1, 1, 0), paths_section)).title("Paths"))
        .child(DummyView.fixed_height(1))
        .child(Panel::new(PaddedView::new(Margins::lrtb(1, 1, 1, 0), display_section)).title("Display"))
        .child(DummyView.fixed_height(1))
        .child(Panel::new(PaddedView::new(Margins::lrtb(1, 1, 1, 0), data_section)).title("Data"))
        .child(DummyView.fixed_height(1))
        .child(Panel::new(PaddedView::new(Margins::lrtb(1, 1, 1, 0), sync_section)).title("Sync"));

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

    let restart_flag = restart_requested.clone();
    siv.add_layer(
        Dialog::around(main_layout)
            .title("toki settings")
            .button("Save", move |s| {
                let flag = restart_flag.clone();
                s.add_layer(
                    Dialog::text("Save settings?")
                        .button("Yes", move |s| {
                            s.pop_layer(); // close confirm dialog
                            save_settings(s, flag.clone());
                        })
                        .button("No", |s| {
                            s.pop_layer(); // close confirm dialog only
                        })
                );
            })
            .button("Cancel", |s| s.quit())
            .min_width(70)
    );

    // Esc to quit
    siv.add_global_callback(Key::Esc, |s| s.quit());

    siv.run();

    restart_requested.load(std::sync::atomic::Ordering::SeqCst)
}

fn save_settings(siv: &mut Cursive, restart_flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    let claude_root = get_edit(siv, "claude_code_root");
    let codex_root = get_edit(siv, "codex_root");
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

    // Sync: only device name is editable in TUI
    let sync_device_name = get_edit(siv, "sync_device_name");

    // Validate device name
    if sync_device_name.len() > 64 {
        siv.add_layer(Dialog::info("Device name must be 64 characters or less"));
        return;
    }
    if sync_device_name.contains(|c: char| c.is_control()) {
        siv.add_layer(Dialog::info("Device name must not contain control characters"));
        return;
    }

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
        ("codex_root", codex_root.as_str()),
        ("daemon_sock", daemon_sock.as_str()),
        ("timezone", timezone.as_str()),
        ("output_format", output_format.as_str()),
        ("start_of_week", start_of_week.as_str()),
        ("no_cost", if no_cost { "true" } else { "false" }),
        ("retention_days", retention.as_str()),
        ("rollup_retention_days", rollup_retention.as_str()),
        ("sync_device_name", sync_device_name.as_str()),
    ];

    // Read providers from hidden storage (set by popup)
    let new_providers: Vec<String> = siv.call_on_name("providers_data", |v: &mut EditView| {
        let content = v.get_content().to_string();
        if content.is_empty() {
            Vec::new()
        } else {
            content.split(',').map(|s| s.to_string()).collect()
        }
    }).unwrap_or_default();

    // Load old values to detect restart-requiring changes
    // Only claude_code_root, codex_root, daemon_sock, providers require restart.
    // Other settings (including sync, retention) are hot-reloaded.
    let old_providers = crate::config::get_providers();
    let restart_keys = ["claude_code_root", "codex_root", "daemon_sock"];
    let old_values: Vec<(String, String)> = restart_keys.iter()
        .map(|k| (k.to_string(), crate::config::get_setting(k).unwrap_or_default()))
        .collect();

    // Save scalar settings
    for (key, value) in &settings {
        if let Err(e) = crate::config::set_setting(key, value) {
            siv.add_layer(Dialog::info(format!("Failed to save {}: {}", key, e)));
            return;
        }
    }

    // Save providers
    if let Err(e) = crate::config::set_setting_array("providers", &new_providers) {
        siv.add_layer(Dialog::info(format!("Failed to save providers: {}", e)));
        return;
    }

    // Check if any daemon-affecting setting changed
    let providers_changed = old_providers != new_providers;
    let scalar_changed = old_values.iter().any(|(k, old_v)| {
        settings.iter().any(|(sk, sv)| sk == k && sv != old_v)
    });
    let daemon_changed = providers_changed || scalar_changed;

    let pidfile = crate::daemon::default_pidfile_path();
    let daemon_running = crate::daemon::daemon_status(&pidfile).is_some();

    if daemon_changed && daemon_running {
        siv.add_layer(
            Dialog::text("Settings saved.\n\nDaemon-affecting settings changed.\nRestart daemon now?")
                .title("Restart Required")
                .button("Yes", {
                    let flag = restart_flag.clone();
                    move |s| {
                        s.quit();
                        // Signal restart via atomic flag (avoids deprecated set_var)
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
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
        siv.quit();
    }
}

fn get_edit(siv: &mut Cursive, name: &str) -> String {
    siv.call_on_name(name, |v: &mut EditView| {
        v.get_content().to_string()
    }).unwrap_or_default()
}

/// Format enabled providers for display in the main TUI.
fn format_providers_display(providers: &[String]) -> String {
    if providers.is_empty() {
        "(none)".to_string()
    } else {
        providers.iter().map(|p| match p.as_str() {
            "claude_code" => "Claude Code",
            "codex" => "Codex CLI",
            _ => p.as_str(),
        }).collect::<Vec<_>>().join(", ")
    }
}

/// Show a popup dialog with checkboxes for provider selection.
fn show_providers_popup(siv: &mut Cursive) {
    // Read current selection from hidden storage
    let current: Vec<String> = siv.call_on_name("providers_data", |v: &mut EditView| {
        let content = v.get_content().to_string();
        if content.is_empty() { Vec::new() }
        else { content.split(',').map(|s| s.to_string()).collect() }
    }).unwrap_or_default();

    let mut layout = LinearLayout::vertical();
    for &pname in crate::providers::KNOWN_PROVIDERS {
        let checked = current.contains(&pname.to_string());
        let display = match pname {
            "claude_code" => "Claude Code",
            "codex" => "Codex CLI",
            _ => pname,
        };
        layout.add_child(
            LinearLayout::horizontal()
                .child(Checkbox::new().with_checked(checked).with_name(format!("popup_provider_{}", pname)))
                .child(TextView::new(format!("  {}", display)))
        );
    }

    siv.add_layer(
        Dialog::around(PaddedView::lrtb(1, 1, 1, 1, layout))
            .title("Select Providers")
            .button("OK", |s| {
                // Read checkboxes and update hidden storage + display
                let mut selected: Vec<String> = Vec::new();
                for &pname in crate::providers::KNOWN_PROVIDERS {
                    let cb_name = format!("popup_provider_{}", pname);
                    let checked = s.call_on_name(&cb_name, |v: &mut Checkbox| v.is_checked()).unwrap_or(false);
                    if checked {
                        selected.push(pname.to_string());
                    }
                }
                let display = format_providers_display(&selected);
                s.call_on_name("providers_data", |v: &mut EditView| {
                    v.set_content(selected.join(","));
                });
                s.call_on_name("providers_display", |v: &mut TextView| {
                    v.set_content(display);
                });
                s.pop_layer();
            })
            .button("Cancel", |s| { s.pop_layer(); })
    );
}

/// Collapse home directory prefix to ~ for display.
fn tilde_path(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if path.starts_with(home_str.as_ref()) {
            return format!("~{}", &path[home_str.len()..]);
        }
    }
    path.to_string()
}
