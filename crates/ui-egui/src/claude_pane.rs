//! The Claude task pane: a chat that runs the local `claude` CLI against this window.
//!
//! Each turn is one `claude -p` process reading the prompt on stdin and streaming JSON lines. It
//! reaches the workbook only through `gridcraft-cli mcp --connect <port>`, so edits go through the
//! same commands (and undo) as any other agent. Desktop only: the web build has no processes.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};

use egui::Ui;
use serde_json::{Value as Json, json};

use crate::SheetApp;
use crate::theme::Tokens;

/// The MCP server name in the generated config; its tools are the only ones Claude may call.
const SERVER: &str = "gridcraft";
const SYSTEM_PROMPT: &str = "You are inside the GridCraft spreadsheet app. Use the gridcraft tools to read and change the open workbook. \
Keep replies short: the chat pane is narrow.";

const HELP: &str = "Commands: /new or /clear starts a new chat, /help shows this list.";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    Claude,
    Tool,
    Error,
}

#[derive(Debug, PartialEq)]
enum Event {
    Session(String),
    Text(String),
    Tool(String),
    Error(String),
    Done,
}

/// The chat of this window. `control_port` is set when the app runs with `--control`.
#[derive(Default)]
pub struct ClaudePane {
    pub control_port: Option<u16>,
    messages: Vec<(Role, String)>,
    draft: String,
    session: Option<String>,
    child: Option<Child>,
    rx: Option<Receiver<Event>>,
}

impl Drop for ClaudePane {
    fn drop(&mut self) {
        self.stop();
    }
}

impl ClaudePane {
    fn busy(&self) -> bool {
        self.rx.is_some()
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn finish(&mut self) {
        self.rx = None;
        self.stop();
    }

    fn drain(&mut self) {
        let Some(rx) = self.rx.take() else { return };
        let mut done = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                Event::Session(id) => self.session = Some(id),
                Event::Text(s) => self.messages.push((Role::Claude, s)),
                Event::Tool(s) => self.messages.push((Role::Tool, s)),
                Event::Error(s) => self.messages.push((Role::Error, s)),
                Event::Done => done = true,
            }
        }
        if done {
            self.finish();
        } else {
            self.rx = Some(rx);
        }
    }

    fn new_chat(&mut self) {
        self.messages.clear();
        self.session = None;
    }

    /// Handled here, not by Claude: `claude -p` treats a slash command as plain text.
    fn slash_command(&mut self, input: &str) {
        match input.split_whitespace().next().unwrap_or_default() {
            "/new" | "/clear" => self.new_chat(),
            "/help" => self.messages.push((Role::Tool, HELP.into())),
            other => self.messages.push((Role::Error, format!("Unknown command {other}. {HELP}"))),
        }
    }

    fn send(&mut self, prompt: String, context: String, port: u16, ctx: &egui::Context) {
        self.messages.push((Role::User, prompt.clone()));
        let mut command = claude_command(port, self.session.as_deref());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                self.messages.push((Role::Error, format!("Could not start claude: {e}. Install Claude Code and make sure it is on PATH.")));
                return;
            }
        };
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            self.messages.push((Role::Error, "Could not connect to claude.".into()));
            return;
        };
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        // A prompt larger than the pipe buffer must not stall the UI thread.
        std::thread::spawn(move || {
            let _ = stdin.write_all(format!("{context}\n\n{prompt}").as_bytes());
            drop(stdin);
            read_stream(stdout, &tx, &ctx);
        });
        self.child = Some(child);
        self.rx = Some(rx);
    }
}

fn read_stream(stdout: impl std::io::Read, tx: &Sender<Event>, ctx: &egui::Context) {
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        for event in parse_line(&line) {
            let _ = tx.send(event);
        }
        ctx.request_repaint();
    }
    let _ = tx.send(Event::Done);
    ctx.request_repaint();
}

/// One line of `--output-format stream-json`. Unknown lines yield nothing, so a newer CLI can add types.
fn parse_line(line: &str) -> Vec<Event> {
    let Ok(v) = serde_json::from_str::<Json>(line) else { return Vec::new() };
    let mut out = Vec::new();
    match v["type"].as_str() {
        Some("system") => {
            if let Some(id) = v["session_id"].as_str() {
                out.push(Event::Session(id.to_string()));
            }
        }
        Some("assistant") => {
            for block in v["message"]["content"].as_array().map(Vec::as_slice).unwrap_or_default() {
                match block["type"].as_str() {
                    Some("text") => out.extend(block["text"].as_str().map(|s| Event::Text(s.to_string()))),
                    Some("tool_use") => {
                        let name = block["name"].as_str().unwrap_or_default();
                        out.push(Event::Tool(name.rsplit("__").next().unwrap_or(name).to_string()));
                    }
                    _ => {}
                }
            }
        }
        Some("result") if v["is_error"].as_bool() == Some(true) => {
            out.push(Event::Error(v["result"].as_str().unwrap_or("Claude stopped with an error.").to_string()));
        }
        _ => {}
    }
    out
}

/// `gridcraft-cli` beside this executable, or the one on PATH.
fn cli_path() -> String {
    let name = if cfg!(windows) { "gridcraft-cli.exe" } else { "gridcraft-cli" };
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join(name)))
        .filter(|p| p.exists())
        .map_or_else(|| name.to_string(), |p| p.display().to_string())
}

fn claude_command(port: u16, session: Option<&str>) -> Command {
    let config = json!({"mcpServers": {SERVER: {"command": cli_path(), "args": ["mcp", "--connect", port.to_string()]}}});
    let mut command = Command::new("claude");
    command
        .args(["-p", "--output-format", "stream-json", "--verbose", "--strict-mcp-config", "--mcp-config"])
        .arg(config.to_string())
        .args(["--tools", ""])
        .args(["--allowedTools", &format!("mcp__{SERVER}")])
        .args(["--append-system-prompt", SYSTEM_PROMPT]);
    if let Some(id) = session {
        command.args(["--resume", id]);
    }
    // Resumed sessions are looked up by folder, and a fixed one keeps project files out of the chat.
    command.current_dir(std::env::temp_dir()).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    command
}

fn workbook_context(app: &SheetApp) -> String {
    let Some(d) = app.session.active() else { return String::new() };
    let sheet = d.wb.active().map(|s| s.name.to_string()).unwrap_or_default();
    format!("[Active sheet: {sheet}. Active cell: {}.]", d.selection.active.a1())
}

pub fn show(app: &mut SheetApp, ui: &mut Ui) {
    app.claude.drain();
    let t = Tokens::get(ui.ctx());
    let Some(port) = app.claude.control_port else {
        ui.label("Claude needs the control channel. Start GridCraft with --control 7979.");
        return;
    };
    let busy = app.claude.busy();
    egui::Panel::bottom("claude_input").frame(egui::Frame::NONE).show(ui, |ui| {
        ui.add_space(6.0);
        let edit = egui::TextEdit::multiline(&mut app.claude.draft).hint_text("Ask Claude…").desired_rows(3).desired_width(f32::INFINITY);
        let r = ui.add(edit);
        let enter = r.has_focus() && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
        ui.horizontal(|ui| {
            if busy {
                if ui.button("Stop").clicked() {
                    app.claude.finish();
                    app.claude.messages.push((Role::Error, "Stopped.".into()));
                }
                ui.spinner();
            } else {
                let click = ui.button("Send").clicked();
                let prompt = app.claude.draft.trim().to_string();
                if (click || enter) && !prompt.is_empty() {
                    app.claude.draft.clear();
                    if prompt.starts_with('/') {
                        app.claude.slash_command(&prompt);
                    } else {
                        let context = workbook_context(app);
                        app.claude.send(prompt, context, port, ui.ctx());
                    }
                }
            }
            if !busy && ui.small_button("New chat").clicked() {
                app.claude.new_chat();
            }
        });
    });
    egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
        ui.set_width(ui.available_width());
        if app.claude.messages.is_empty() {
            ui.label(egui::RichText::new("Ask Claude about this workbook or to change it. Use Undo to revert edits.").italics().color(t.text_dim));
        }
        for (role, text) in &app.claude.messages {
            let rich = egui::RichText::new(text.as_str());
            match role {
                Role::User => ui.label(rich.strong()),
                Role::Claude => ui.label(rich),
                Role::Tool => ui.label(rich.small().color(t.text_dim)),
                Role::Error => ui.colored_label(egui::Color32::from_rgb(0xd0, 0x40, 0x40), text.as_str()),
            };
            ui.add_space(6.0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{ClaudePane, Event, Role, parse_line};

    fn chat() -> ClaudePane {
        let mut pane = ClaudePane::default();
        pane.session = Some("s1".into());
        pane
    }

    #[test]
    fn slash_commands_stay_local() {
        for command in ["/new", "/clear", "/clear now"] {
            let mut pane = chat();
            pane.messages.push((Role::User, "hi".into()));
            pane.slash_command(command);
            assert!(pane.messages.is_empty() && pane.session.is_none());
        }
        let mut pane = chat();
        pane.slash_command("/nope");
        assert!(pane.session.is_some());
        assert!(pane.messages[0].0 == Role::Error && pane.messages[0].1.contains("/nope"));
    }

    #[test]
    fn stream_lines_become_events() {
        assert_eq!(parse_line(r#"{"type":"system","session_id":"s1"}"#), vec![Event::Session("s1".into())]);
        let line =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hi"},{"type":"tool_use","name":"mcp__gridcraft__set_cell"}]}}"#;
        assert_eq!(parse_line(line), vec![Event::Text("Hi".into()), Event::Tool("set_cell".into())]);
        assert_eq!(parse_line(r#"{"type":"result","is_error":true,"result":"Nope"}"#), vec![Event::Error("Nope".into())]);
        assert!(parse_line(r#"{"type":"result","is_error":false,"result":"Hi"}"#).is_empty());
    }

    #[test]
    fn hostile_lines_are_ignored() {
        assert!(parse_line("not json").is_empty());
        assert!(parse_line(r#"{"type":"assistant","message":{"content":7}}"#).is_empty());
    }
}
