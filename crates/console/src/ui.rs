//! Drawing the console: a tab bar, the tab, and a line of keys.

use crate::app::{role_name, App, Row, Tab, TABS};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row as TRow, Table, Tabs, Wrap};
use ratatui::Frame;
use serde_json::Value;

const GOOD: Color = Color::Green;
const WARN: Color = Color::Yellow;
const BAD: Color = Color::Red;
const DIM: Color = Color::DarkGray;

pub fn draw(f: &mut Frame, app: &App) {
    let [top, body, msg, keys] = Layout::vertical([Constraint::Length(2), Constraint::Min(5), Constraint::Length(1), Constraint::Length(1)])
        .areas(f.area());
    let st = app.status.as_ref();
    let s = |k: &str| st.and_then(|v| v.get(k)).and_then(Value::as_str).unwrap_or("").to_string();
    let label = app.saved.as_ref().and_then(|v| v.get("label")).and_then(Value::as_str).unwrap_or("");
    let mut title = vec![Span::raw("RailSpotting receiver").bold()];
    if !label.is_empty() {
        title.push(Span::raw(format!("  {label}")));
    }
    if !s("device_id").is_empty() {
        title.push(Span::raw(format!("  {}", s("device_id"))).fg(DIM));
    }
    if !s("agent_version").is_empty() {
        title.push(Span::raw(format!("  v{}", s("agent_version"))).fg(DIM));
    }
    if st.and_then(|v| v.get("simulate")).and_then(Value::as_bool) == Some(true) {
        title.push(Span::raw("  simulated").fg(WARN));
    }
    let [t1, t2] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(top);
    f.render_widget(Line::from(title), t1);
    let tabs = Tabs::new(TABS.iter().enumerate().map(|(i, (_, n))| format!("{} {n}", i + 1)))
        .select(TABS.iter().position(|(t, _)| *t == app.tab).unwrap_or(0))
        .highlight_style(Style::new().bold().reversed());
    f.render_widget(tabs, t2);

    if let Some(e) = &app.offline {
        let p = Paragraph::new(vec![Line::from("Can't reach the receiver.").fg(BAD).bold(), Line::raw(""), Line::raw(e.clone()),
                                    Line::raw(""), Line::from("Trying again every few seconds.").fg(DIM)])
            .wrap(Wrap { trim: true }).block(Block::bordered());
        f.render_widget(p, body);
    } else {
        match app.tab {
            Tab::Status => status(f, body, app),
            Tab::Events => events(f, body, app),
            Tab::Settings => settings(f, body, app),
        }
    }

    let line = match (&app.busy, &app.message) {
        (Some(b), _) => Line::from(b.clone()).fg(WARN),
        (None, Some((m, ok))) => Line::from(m.clone()).fg(if *ok { GOOD } else { BAD }),
        _ => Line::raw(""),
    };
    f.render_widget(line, msg);
    let help = match (app.tab, app.typing.is_some()) {
        (_, true) => "Enter keep  Esc cancel",
        (Tab::Settings, _) => "↑↓ move  Enter/Space change  ←→ dongle job  [ ] move role  s save  u undo  e edit receiver.json  Tab next  q quit",
        (Tab::Events, _) => "↑↓ scroll  r refresh  e edit receiver.json  l link  Tab next  q quit",
        (Tab::Status, _) => "r refresh  e edit receiver.json  l link  Tab next  q quit",
    };
    f.render_widget(Line::from(help).fg(DIM), keys);
}

fn reg(st: &Value) -> (Color, &'static str, &'static str) {
    match st.get("registration").and_then(Value::as_str).unwrap_or("") {
        "approved" => (GOOD, "Linked", "Signed uploads are going to the headend."),
        "checking" => (WARN, "Checking", "Checking the stored device ID with the headend. Events are queued locally."),
        "waiting" => (WARN, "Waiting for the owner", "Open the address below, sign in, and enter the code."),
        "denied" => (BAD, "Link denied", "The owner denied this receiver. Press l to start linking again."),
        _ => (WARN, "Not linked", "Asking the headend for a link code. Events are queued locally. Press l to start linking again."),
    }
}

/// "12 s", "4 min", "3 h", "2 d".
pub fn age(sec: f64) -> String {
    let sec = sec.max(0.0).round();
    if sec < 90.0 {
        format!("{sec} s")
    } else if sec < 5400.0 {
        format!("{} min", (sec / 60.0).round())
    } else if sec < 172_800.0 {
        format!("{} h", (sec / 3600.0).round())
    } else {
        format!("{} d", (sec / 86400.0).round())
    }
}

fn num(v: Option<&Value>, places: usize) -> String {
    v.and_then(Value::as_f64).map(|x| format!("{x:.places$}")).unwrap_or_else(|| "-".into())
}

fn status(f: &mut Frame, area: Rect, app: &App) {
    let Some(st) = &app.status else {
        f.render_widget(Paragraph::new("Reading the receiver's status...").block(Block::bordered()), area);
        return;
    };
    let now = st.get("ts").and_then(Value::as_f64).unwrap_or(0.0);
    let sdrs: Vec<&Value> = st.get("sdrs").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
    let [head, dongles, scanner, _] = Layout::vertical([Constraint::Length(7), Constraint::Length(sdrs.len() as u16 + 3),
                                                        Constraint::Length(3), Constraint::Min(0)]).areas(area);

    // the headend
    let (color, name, text) = reg(st);
    let mut lines = vec![Line::from(vec![Span::raw(name).fg(color).bold(), Span::raw(format!("  {text}"))])];
    if let Some(lk) = st.get("link").filter(|l| l.is_object()) {
        let code = lk.get("user_code").and_then(Value::as_str).unwrap_or("");
        let uri = lk.get("verification_uri").and_then(Value::as_str).unwrap_or("");
        lines.push(Line::from(vec![Span::raw("Code ").fg(DIM), Span::raw(code).bold().fg(Color::Cyan), Span::raw("  at ").fg(DIM),
                                   Span::raw(uri.to_string())]));
    }
    let hb = st.get("last_heartbeat").and_then(|h| h.get("ts")).and_then(Value::as_f64)
        .map(|t| format!("{} ago", age(now - t))).unwrap_or_else(|| "none yet".into());
    let up = st.get("uploaded").cloned().unwrap_or(Value::Null);
    let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    lines.push(Line::from(vec![
        Span::raw("Last heartbeat ").fg(DIM), Span::raw(hb),
        Span::raw("   Clock ").fg(DIM),
        if st.get("clock_synced").and_then(Value::as_bool) == Some(false) { Span::raw("not synced").fg(BAD) } else { Span::raw("synced") },
    ]));
    let queued = n(st, "outbox_depth");
    lines.push(Line::from(vec![
        Span::raw("Uploaded ").fg(DIM),
        Span::raw(format!("{} events, {} transcripts, {} telemetry", n(&up, "events"), n(&up, "transcripts"), n(&up, "telemetry"))),
        Span::raw("   Queued ").fg(DIM),
        if queued > 0 { Span::raw(queued.to_string()).fg(WARN) } else { Span::raw("0") },
        if n(st, "dropped_events") > 0 { Span::raw(format!("   Dropped {}", n(st, "dropped_events"))).fg(BAD) } else { Span::raw("") },
    ]));
    let failing: Vec<String> = st.get("api").and_then(Value::as_object).into_iter().flatten()
        .filter_map(|(k, v)| v.get("error").and_then(Value::as_str).map(|e| format!("{k}: {e}"))).collect();
    if let Some(e) = failing.first() {
        lines.push(Line::from(format!("Headend error {e}")).fg(BAD));
    }
    let server = st.get("server_url").and_then(Value::as_str).unwrap_or("");
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(Block::bordered().title(format!(" Headend {server} "))), head);

    // the dongles
    let rows = sdrs.iter().map(|s| {
        let g = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("");
        let serial = g("serial");
        let (state, color) = if serial.is_empty() {
            ("no dongle", DIM)
        } else if s.get("plugged_in").and_then(Value::as_bool) == Some(false) {
            ("unplugged", BAD)
        } else if s.get("connected").and_then(Value::as_bool) == Some(true) {
            if s.get("corrupt").and_then(Value::as_bool) == Some(true) { ("corrupt data", BAD) } else { ("connected", GOOD) }
        } else {
            ("not connected", BAD)
        };
        let chans = s.get("channels").and_then(Value::as_array).cloned().unwrap_or_default();
        let rx = chans.iter().filter(|c| c.get("receiving").and_then(Value::as_bool) == Some(true)).count();
        let detail = match s.get("itc").filter(|i| i.is_object()) {
            Some(i) => format!("ITC: {} messages, {} bases", i.get("messages").and_then(Value::as_u64).unwrap_or(0),
                               i.get("bases").and_then(Value::as_array).map_or(0, Vec::len)),
            None if chans.is_empty() => String::new(),
            None => format!("{rx} of {} receiving", chans.len()),
        };
        let how = match g("assigned") {
            _ if serial.is_empty() => "",
            "website" => "website",
            "receiver.json" => "receiver.json",
            "automatic" => "automatic",
            "" => "",
            _ => "receiver.json",
        };
        TRow::new(vec![
            Cell::from(g("name").to_string()),
            Cell::from(if serial.is_empty() { "-".to_string() } else { serial.to_string() }),
            Cell::from(state).fg(color),
            Cell::from(how),
            Cell::from(num(s.get("center_mhz"), 4)),
            Cell::from(num(s.get("gain"), 1)),
            Cell::from(detail),
        ])
    });
    let mut title = " Dongles ".to_string();
    let sa = st.get("sdr_assign").cloned().unwrap_or(Value::Null);
    let list = |k: &str| sa.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
        .filter(|s| !s.is_empty());
    if let Some(s) = list("spare") {
        title.push_str(&format!("(spare: {s}) "));
    }
    if let Some(s) = list("off") {
        title.push_str(&format!("(off: {s}) "));
    }
    let table = Table::new(rows, [Constraint::Length(26), Constraint::Length(12), Constraint::Length(14), Constraint::Length(14),
                                  Constraint::Length(10), Constraint::Length(6), Constraint::Min(10)])
        .header(TRow::new(["Role", "Serial", "State", "Job from", "Center MHz", "Gain", ""]).style(Style::new().fg(DIM)))
        .block(Block::bordered().title(title));
    f.render_widget(table, dongles);

    // the scanner
    let sc = st.get("scanner").cloned().unwrap_or(Value::Null);
    let g = |k: &str| sc.get(k).map(|v| match v { Value::String(s) => s.clone(), Value::Null => String::new(), v => v.to_string() })
        .unwrap_or_default();
    let line = if sc.get("connected").and_then(Value::as_bool) == Some(true) {
        let mut parts = vec![Span::raw("connected").fg(GOOD), Span::raw(format!("  {} {}", g("model"), g("state")))];
        if !g("freq").is_empty() {
            parts.push(Span::raw(format!("  {} MHz", g("freq"))));
        }
        let role = st.get("scanner_role").and_then(Value::as_str).unwrap_or("");
        if !role.is_empty() {
            parts.push(Span::raw(format!("  job {role}")).fg(DIM));
        }
        Line::from(parts)
    } else if sc.is_null() || g("port") == "none" {
        Line::from("none").fg(DIM)
    } else {
        Line::from(format!("not connected ({})", g("port"))).fg(WARN)
    };
    f.render_widget(Paragraph::new(line).block(Block::bordered().title(" Scanner ")), scanner);
}

/// Local time of day for an epoch second.
pub fn clock(t: f64) -> String {
    let secs = t as libc::time_t;
    // SAFETY: localtime_r only writes into the struct we hand it
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return "-".into();
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// The short description of an event, as the dashboard gives it.
pub fn event_detail(e: &Value) -> String {
    let mut out = vec![];
    if let Some(t) = e.get("telemetry").filter(|t| t.is_object()) {
        let g = |k: &str| t.get(k).map(|v| v.to_string()).unwrap_or_default();
        let proto = t.get("protocol").and_then(Value::as_str).unwrap_or("").to_uppercase();
        out.push(format!("{proto} unit {}", g("unit_id")));
        if t.get("brake_pipe_psi").is_some() {
            out.push(format!("{} psi", g("brake_pipe_psi")));
        }
    }
    if let Some(tr) = e.get("transcript").filter(|t| t.is_object()) {
        out.push(format!("\"{}\"", tr.get("text").and_then(Value::as_str).unwrap_or("")));
        if tr.get("defect").and_then(Value::as_bool) == Some(true) {
            out.push("defect report".into());
        }
    }
    if let Some(d) = e.get("detector").filter(|d| d.is_object()) {
        let name = d.get("name").and_then(Value::as_str).or_else(|| d.get("id").and_then(Value::as_str)).unwrap_or("detector");
        out.push(name.to_string());
    }
    out.join("  ")
}

fn events(f: &mut Frame, area: Rect, app: &App) {
    if app.events.is_empty() {
        f.render_widget(Paragraph::new("No events yet.").fg(DIM).block(Block::bordered().title(" Events ")), area);
        return;
    }
    let rows = app.events.iter().skip(app.event_scroll).map(|e| {
        let g = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let start = e.get("start_ts").and_then(Value::as_f64).unwrap_or(0.0);
        let len = e.get("end_ts").and_then(Value::as_f64).map(|end| format!("{:.1} s", end - start)).unwrap_or_else(|| "open".into());
        let ch = e.get("ch").map(|c| c.to_string()).filter(|c| c != "null").unwrap_or_default();
        TRow::new(vec![
            Cell::from(clock(start)),
            Cell::from(g("kind")),
            Cell::from(format!("{ch} {}", g("label")).trim().to_string()),
            Cell::from(num(e.get("freq"), 4)),
            Cell::from(len),
            Cell::from(e.get("peak_rssi").map(|v| v.to_string()).unwrap_or_default()),
            Cell::from(g("source")),
            Cell::from(event_detail(e)),
        ])
    });
    let table = Table::new(rows, [Constraint::Length(8), Constraint::Length(10), Constraint::Length(24), Constraint::Length(9),
                                  Constraint::Length(7), Constraint::Length(5), Constraint::Length(8), Constraint::Min(10)])
        .header(TRow::new(["Time", "Kind", "Channel", "MHz", "Length", "Peak", "From", ""]).style(Style::new().fg(DIM)))
        .block(Block::bordered().title(format!(" Events ({} newest first) ", app.events.len())));
    f.render_widget(table, area);
}

fn settings(f: &mut Frame, area: Rect, app: &App) {
    let Some(form) = &app.form else {
        f.render_widget(Paragraph::new("Reading the settings...").block(Block::bordered()), area);
        return;
    };
    let mut lines: Vec<Line> = vec![];
    let add_heading = |lines: &mut Vec<Line>, t: &str, hint: &str| {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(vec![Span::raw(t.to_string()).bold(), Span::raw(format!("  {hint}")).fg(DIM)]));
    };
    let mut cursor_line = 0;
    let rows = app.rows();
    for (i, row) in rows.iter().enumerate() {
        match row {
            Row::Label => add_heading(&mut lines, "Receiver", "saving restarts the receiver; it takes a few seconds"),
            Row::Order(0) => add_heading(&mut lines, "Role order", "dongles without a job take roles in this order; Space switches one off"),
            Row::Job(0) => add_heading(&mut lines, "Dongle jobs", "a job chosen on the website wins over this"),
            _ => {}
        }
        let here = i == app.row;
        if here {
            cursor_line = lines.len();
        }
        let typed = |v: &str| match (&app.typing, here) {
            (Some(t), true) => format!("{t}▏"),
            _ if v.is_empty() => "none".into(),
            _ => v.to_string(),
        };
        let (name, value, dim) = match row {
            Row::Label => ("Name on the headend".to_string(), typed(&form.label), form.label.is_empty()),
            Row::OtherCalls => ("Transcribe other calls".to_string(), if form.other_calls { "[x] yes".into() } else { "[ ] no".into() }, false),
            Row::Watch => ("Keep every recording on (MHz)".to_string(), typed(&form.watch), form.watch.is_empty()),
            Row::Order(k) => {
                let (r, on) = &form.order[*k];
                let place = form.order[..=*k].iter().filter(|(_, o)| *o).count();
                (if *on { format!("{place}. {}", role_name(r)) } else { format!("   {}", role_name(r)) },
                 if *on { "[x] on".into() } else { "[ ] off".into() }, !*on)
            }
            Row::Job(k) => {
                let (serial, job) = &form.jobs[*k];
                let here_now = app.connected().contains(serial);
                (format!("{serial}{}", if here_now { "" } else { " (not connected)" }), format!("‹ {} ›", role_name(job)), job.is_empty())
            }
        };
        let style = if here { Style::new().reversed() } else { Style::new() };
        let value_style = if dim && !here { Style::new().fg(DIM) } else { style };
        lines.push(Line::from(vec![Span::styled(format!("  {name:<34}"), style), Span::styled(value, value_style)]));
    }
    if form.jobs.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from("Dongle jobs: no dongles connected.").fg(DIM));
    }
    lines.push(Line::raw(""));
    if form.devices.is_empty() {
        lines.push(Line::from("Fixed dongles: none (roles follow the order and jobs above).").fg(DIM));
    } else {
        lines.push(Line::from("Fixed dongles (sdr.devices; with these, the order and jobs above are not used; change them with e):").fg(WARN));
        for d in &form.devices {
            let g = |k: &str| d.get(k).map(|v| v.to_string().trim_matches('"').to_string()).unwrap_or_else(|| "-".into());
            lines.push(Line::raw(format!("  {:<28} serial {:<12} gain {}", role_name(&g("role")), g("serial"), g("gain"))));
        }
    }
    // keep the cursor on screen
    let height = area.height.saturating_sub(2) as usize;
    let scroll = cursor_line.saturating_sub(height.saturating_sub(3));
    let title = if app.dirty() { " Settings (not saved) " } else { " Settings " };
    let block = Block::bordered().title(title);
    let block = if app.dirty() { block.border_style(Style::new().fg(WARN)) } else { block };
    f.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)).block(block), area);
}

/// What the screen shows, as text (for tests).
#[cfg(test)]
pub fn render_text(app: &App, w: u16, h: u16) -> String {
    let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
    t.draw(|f| draw(f, app)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string())
        .collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn app() -> App {
        let mut a = App::new();
        a.set_status(Ok(json!({"ts": 1000.0, "registration": "waiting", "device_id": null, "agent_version": "2.0.0",
            "link": {"user_code": "ABCD-1234", "verification_uri": "https://example.com/link"},
            "server_url": "https://example.com", "last_heartbeat": {"ts": 990.0}, "uploaded": {"events": 3},
            "outbox_depth": 2, "clock_synced": true,
            "sdrs": [{"name": "UHF SDR (EOT)", "role": "eot", "serial": "30001111", "connected": true, "assigned": "automatic",
                      "center_mhz": 458.1385, "gain": 40.2, "channels": [{"receiving": true}, {"receiving": false}]},
                     {"name": "220 MHz SDR (ITC)", "role": "itc", "serial": "30002222", "connected": true, "assigned": "receiver.json",
                      "itc": {"messages": 12, "bases": [{}]}, "channels": []},
                     {"name": "VHF SDR (voice)", "role": "vhf", "serial": "", "channels": []}],
            "sdr_assign": {"spare": [], "off": []},
            "scanner": {"connected": true, "model": "SR30C", "state": "scanning"}})));
        a.set_settings(json!({"label": "Shed", "transcribe_other_calls": true, "watch_clips": [],
                              "sdr": {"devices": [], "role_order": ["eot", "itc", "vhf", "hot"], "assign": {"30002222": "itc"}}}));
        a
    }

    #[test]
    fn status_shows_the_link_code_and_the_dongles() {
        let text = render_text(&app(), 120, 30);
        assert!(text.contains("Waiting for the owner"), "{text}");
        assert!(text.contains("ABCD-1234") && text.contains("https://example.com/link"));
        assert!(text.contains("10 s ago") && text.contains("Queued 2"));
        assert!(text.contains("30001111") && text.contains("1 of 2 receiving") && text.contains("458.1385"));
        assert!(text.contains("ITC: 12 messages, 1 bases") && text.contains("receiver.json"));
        assert!(text.contains("no dongle"));
        assert!(text.contains("SR30C"));
    }

    #[test]
    fn settings_show_order_jobs_and_the_unsaved_mark() {
        let mut a = app();
        a.tab = Tab::Settings;
        let text = render_text(&a, 120, 30);
        assert!(text.contains("1. UHF (EOT)") && text.contains("4. UHF (HOT)"), "{text}");
        assert!(text.contains("30002222") && text.contains("‹ 220 MHz (ITC) ›") && text.contains("‹ Automatic ›"));
        assert!(!text.contains("not saved"));
        a.form.as_mut().unwrap().other_calls = false;
        assert!(render_text(&a, 120, 30).contains("Settings (not saved)"));
    }

    #[test]
    fn events_and_an_unreachable_receiver() {
        let mut a = app();
        a.tab = Tab::Events;
        a.events = vec![json!({"start_ts": 1000.0, "end_ts": 1002.5, "kind": "telemetry", "ch": 9, "label": "EOT", "freq": 457.9375,
                               "source": "sdr", "telemetry": {"protocol": "eot", "unit_id": 75618, "brake_pipe_psi": 89}})];
        let text = render_text(&a, 140, 20);
        assert!(text.contains("2.5 s") && text.contains("EOT unit 75618  89 psi"), "{text}");
        a.set_status(Err("the receiver is not running".into()));
        assert!(render_text(&a, 100, 20).contains("Can't reach the receiver"));
        assert_eq!(age(4000.0), "67 min");
    }
}
