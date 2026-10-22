//! Extensions / Plugins system (`Window ▸ Extensions`, `PanelKind::Extensions`, and
//! `Edit ▸ Preferences ▸ Plugins…`).
//!
//! Discovers declarative plugin manifests (`<data_dir>/plugins/<plugin_id>/plugin.json` or
//! `FILMCRAFT_PLUGINS_DIR`) and renders their actions and parameter controls natively in egui.
//! Plugin commands run asynchronously on a worker thread against FilmCraft's loopback control
//! bridge (`--bridge 127.0.0.1:<port>`).

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use egui::{Color32, CornerRadius, Frame, Margin, Rect, RichText, ScrollArea, Stroke, Ui, vec2};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::FilmcraftApp;
use crate::control::ControlRequest;
use crate::dock::PanelKind;
use crate::menus::MenuItem;

/// Subfolder inside the FilmCraft application data directory where plugins live.
pub const PLUGINS_SUBDIR: &str = "plugins";
/// Manifest filename inside each plugin directory.
pub const MANIFEST_FILE: &str = "plugin.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub actions: Vec<PluginAction>,
    #[serde(skip)]
    pub dir: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginAction {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub description: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub params: Vec<PluginParam>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginParam {
    pub id: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: ParamKind,
    #[serde(default)]
    pub default: Value,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub choices: Vec<ParamChoice>,
    #[serde(default)]
    pub extensions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamKind {
    Int,
    Float,
    Bool,
    Text,
    Choice,
    File,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParamChoice {
    pub value: String,
    pub label: String,
}

/// Result of a finished plugin action run on a background thread.
#[derive(Clone, Debug)]
pub struct ActionOutcome {
    pub plugin_id: String,
    pub action_id: String,
    pub action_label: String,
    pub ok: bool,
    pub output: String,
}

/// Live state for the Extensions panel and installed plugins.
#[derive(Default)]
pub struct ExtensionsState {
    pub plugins: Vec<PluginManifest>,
    pub active_plugin: usize,
    /// Parameter values keyed by `("<plugin_id>.<action_id>", "<param_id>")`.
    pub values: HashMap<(String, String), Value>,
    /// Currently running background job: `(label, receiver, undo_len_before)`.
    pub running: Option<(String, Receiver<ActionOutcome>, usize)>,
    /// Undo step stacks from completed plugin actions: `(label, steps_count)`.
    pub undo_stack: Vec<(String, usize)>,
    /// Last output message from a plugin action: `(ok, text)`.
    pub last_output: Option<(bool, String)>,
    /// Port of the loopback control server bound for plugins, if started.
    pub bridge_port: Option<u16>,
    pub loaded: bool,
}

impl ExtensionsState {
    pub fn ensure_loaded(&mut self) {
        if !self.loaded {
            self.reload();
        }
    }

    pub fn reload(&mut self) {
        self.plugins = discover_plugins();
        if self.active_plugin >= self.plugins.len() {
            self.active_plugin = 0;
        }
        for plugin in &self.plugins {
            for action in &plugin.actions {
                let key_prefix = format!("{}.{}", plugin.id, action.id);
                for param in &action.params {
                    self.values
                        .entry((key_prefix.clone(), param.id.clone()))
                        .or_insert_with(|| param.default.clone());
                }
            }
        }
        self.loaded = true;
    }
}

/// Directory where FilmCraft looks for plugins: `FILMCRAFT_PLUGINS_DIR` if set, otherwise
/// `<default_data_dir>/plugins`.
pub fn plugins_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("FILMCRAFT_PLUGINS_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(d));
    }
    filmcraft_engine::autosave::default_data_dir().map(|d| d.join(PLUGINS_SUBDIR))
}

/// Scan [`plugins_dir`] for subdirectories containing `plugin.json`.
pub fn discover_plugins() -> Vec<PluginManifest> {
    if cfg!(test) && std::env::var_os("FILMCRAFT_PLUGINS_DIR").filter(|v| !v.is_empty()).is_none() {
        return Vec::new();
    }
    let Some(root) = plugins_dir() else {
        return Vec::new();
    };
    discover_plugins_in(&root)
}

/// Scan a specific directory for plugin subdirectories containing `plugin.json`.
pub fn discover_plugins_in(root: &Path) -> Vec<PluginManifest> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest_path = path.join(MANIFEST_FILE);
        if !manifest_path.is_file() {
            continue;
        }
        let Ok(bytes) = fs::read(&manifest_path) else {
            continue;
        };
        let Ok(mut manifest) = serde_json::from_slice::<PluginManifest>(&bytes) else {
            continue;
        };
        manifest.dir = path;
        out.push(manifest);
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

/// Ensure FilmCraft has a localhost control bridge listening for plugin commands, returning its port.
pub fn ensure_bridge(app: &mut FilmcraftApp, ctx: &egui::Context) -> Option<u16> {
    if let Some(port) = app.control_port.or(app.extensions.bridge_port) {
        return Some(port);
    }
    // Try default control port 9876 first, falling back to an OS-assigned ephemeral port.
    let listener = TcpListener::bind(("127.0.0.1", 9876))
        .or_else(|_| TcpListener::bind(("127.0.0.1", 0)))
        .ok()?;
    let port = listener.local_addr().ok()?.port();
    let (tx, rx) = channel::<ControlRequest>();
    let ctx_clone = ctx.clone();
    std::thread::Builder::new()
        .name("plugin-bridge".into())
        .spawn(move || accept_bridge_loop(listener, tx, ctx_clone))
        .ok()?;
    app.control_rx = Some(rx);
    app.control_port = Some(port);
    app.extensions.bridge_port = Some(port);
    Some(port)
}

fn accept_bridge_loop(listener: TcpListener, tx: Sender<ControlRequest>, ctx: egui::Context) {
    for stream in listener.incoming().flatten() {
        let tx = tx.clone();
        let ctx = ctx.clone();
        let _ = std::thread::Builder::new()
            .name("plugin-bridge-conn".into())
            .spawn(move || serve_bridge_conn(stream, &tx, &ctx));
    }
}

fn serve_bridge_conn(mut stream: TcpStream, tx: &Sender<ControlRequest>, ctx: &egui::Context) {
    let Ok(read) = stream.try_clone() else { return };
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    loop {
        line.clear();
        let Ok(n) = reader.read_line(&mut line) else { return };
        if n == 0 {
            return;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
            let _ = writeln!(stream, "{}", json!({"ok": false, "error": "invalid JSON"}));
            return;
        };
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            let _ = writeln!(stream, "{}", json!({"ok": false, "error": "missing method"}));
            return;
        };
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
        let (req, rrx) = ControlRequest::new(method.to_string(), params);
        if tx.send(req).is_err() {
            return;
        }
        ctx.request_repaint();
        let mut reply = rrx
            .recv_timeout(Duration::from_secs(60))
            .unwrap_or_else(|_| json!({"ok": false, "error": "timeout"}));
        if let Some(obj) = reply.as_object_mut() {
            obj.insert("id".into(), id);
        }
        if writeln!(stream, "{reply}").and_then(|()| stream.flush()).is_err() {
            return;
        }
    }
}

/// Poll any running plugin job and update status when it finishes.
pub fn poll(app: &mut FilmcraftApp) {
    let Some((label, rx, undo_before)) = app.extensions.running.take() else {
        return;
    };
    match rx.try_recv() {
        Ok(outcome) => {
            let undo_after = app.session.history.undo.len();
            let steps = undo_after.saturating_sub(undo_before);
            if outcome.ok && steps > 0 {
                app.extensions.undo_stack.push((outcome.action_label.clone(), steps));
            }
            let text = if outcome.output.trim().is_empty() {
                if outcome.ok {
                    format!("{}: done", outcome.action_label)
                } else {
                    format!("{}: failed", outcome.action_label)
                }
            } else {
                outcome.output.trim().to_string()
            };
            app.ui.status = text.clone();
            app.extensions.last_output = Some((outcome.ok, text));
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => {
            app.extensions.running = Some((label, rx, undo_before));
        }
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            let msg = format!("{label}: plugin process exited unexpectedly");
            app.ui.status = msg.clone();
            app.extensions.last_output = Some((false, msg));
        }
    }
}

/// Add `Window ▸ Extensions` items to the live menu bar when plugins are installed (or to manage them).
pub fn menu(app: &FilmcraftApp, items: &mut Vec<MenuItem>) {
    let tr = |s: &str| app.ui.language.tr(s).to_string();
    let path = vec!["Window".to_string(), tr("Extensions")];
    let mut sub = Vec::new();
    for plugin in &app.extensions.plugins {
        let active = (app.ui.dock.is_visible(PanelKind::Extensions)
            || app.ui.floating.iter().any(|g| g.panels.contains(&PanelKind::Extensions)))
            && app
                .extensions
                .plugins
                .get(app.extensions.active_plugin)
                .is_some_and(|p| p.id == plugin.id);
        sub.push(MenuItem {
            id: format!("window.extension.open.{}", plugin.id),
            label: plugin.name.clone(),
            path: path.clone(),
            shortcut: None,
            enabled: true,
            checked: Some(active),
        });
    }
    sub.push(MenuItem {
        id: "window.extension.reload".into(),
        label: tr("Reload Extensions"),
        path: path.clone(),
        shortcut: None,
        enabled: true,
        checked: None,
    });
    sub.push(MenuItem {
        id: "window.extension.openFolder".into(),
        label: tr("Open Extensions Folder…"),
        path,
        shortcut: None,
        enabled: plugins_dir().is_some(),
        checked: None,
    });
    // Insert right after Window ▸ Workspaces (or at the start of Window).
    let pos = items
        .iter()
        .rposition(|it| it.path.len() == 2 && it.path[0] == "Window" && it.path[1] == "Workspaces")
        .map_or_else(
            || items.iter().position(|it| it.path.first().map(String::as_str) == Some("Window")).unwrap_or(items.len()),
            |i| i + 1,
        );
    items.splice(pos..pos, sub);
}

/// Handle `window.extension.*` commands from menus or control protocol.
pub fn route(app: &mut FilmcraftApp, ctx: &egui::Context, id: &str, params: &Value) -> Option<Result<Value, String>> {
    let rest = id.strip_prefix("window.extension.")?;
    app.extensions.ensure_loaded();
    match rest {
        "reload" => {
            app.extensions.reload();
            let count = app.extensions.plugins.len();
            app.ui.status = format!("Loaded {count} extension(s)");
            Some(Ok(json!({"extensions": count})))
        }
        "openFolder" => {
            let Some(dir) = plugins_dir() else {
                return Some(Err("no plugins directory available".into()));
            };
            let _ = fs::create_dir_all(&dir);
            let dir_str = dir.to_string_lossy().to_string();
            if let Some(open) = app.hooks.open_path.as_mut() {
                if let Err(e) = open(&dir_str, false) {
                    return Some(Err(e));
                }
            }
            Some(Ok(json!({"path": dir_str})))
        }
        "run" => {
            let plugin_id = params.get("plugin").and_then(Value::as_str).unwrap_or_default();
            let action_id = params.get("action").and_then(Value::as_str).unwrap_or_default();
            Some(start_action_by_id(app, ctx, plugin_id, action_id, params.get("params")))
        }
        _ if rest.starts_with("open.") => {
            let plugin_id = rest.strip_prefix("open.").unwrap_or_default();
            if let Some(idx) = app.extensions.plugins.iter().position(|p| p.id == plugin_id) {
                app.extensions.active_plugin = idx;
            }
            app.show_panel(PanelKind::Extensions);
            Some(Ok(json!({"panel": "Extensions", "plugin": plugin_id})))
        }
        _ => Some(Err(format!("unknown extension command `{id}`"))),
    }
}

fn find_filmcraft_cli() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FILMCRAFT_CLI").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let exe_name = if cfg!(windows) { "filmcraft-cli.exe" } else { "filmcraft-cli" };
    if let Ok(cur) = std::env::current_exe()
        && let Some(parent) = cur.parent()
    {
        let sibling = parent.join(exe_name);
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let local_bin = PathBuf::from(home).join(".local/bin").join(exe_name);
        if local_bin.is_file() {
            return Some(local_bin);
        }
    }
    None
}

fn start_action_by_id(
    app: &mut FilmcraftApp,
    ctx: &egui::Context,
    plugin_id: &str,
    action_id: &str,
    override_params: Option<&Value>,
) -> Result<Value, String> {
    if app.extensions.running.is_some() {
        return Err("an extension action is already running".into());
    }
    let Some(plugin) = app.extensions.plugins.iter().find(|p| p.id == plugin_id).cloned() else {
        return Err(format!("unknown plugin `{plugin_id}`"));
    };
    let Some(action) = plugin.actions.iter().find(|a| a.id == action_id).cloned() else {
        return Err(format!("unknown action `{action_id}` in plugin `{plugin_id}`"));
    };
    let mut param_map = serde_json::Map::new();
    let key_prefix = format!("{}.{}", plugin.id, action.id);
    for p in &action.params {
        let val = override_params
            .and_then(|o| o.get(&p.id))
            .cloned()
            .or_else(|| app.extensions.values.get(&(key_prefix.clone(), p.id.clone())).cloned())
            .unwrap_or_else(|| p.default.clone());
        param_map.insert(p.id.clone(), val);
    }
    spawn_plugin_action(app, ctx, &plugin, &action, Value::Object(param_map))?;
    Ok(json!({"running": action.id, "plugin": plugin.id}))
}

fn spawn_plugin_action(
    app: &mut FilmcraftApp,
    ctx: &egui::Context,
    plugin: &PluginManifest,
    action: &PluginAction,
    params_val: Value,
) -> Result<(), String> {
    if action.command.is_empty() {
        return Err("plugin action has an empty command".into());
    }
    let port = ensure_bridge(app, ctx).ok_or("could not start local control bridge for plugin")?;
    let bridge = format!("127.0.0.1:{port}");
    let plugin_dir = plugin.dir.to_string_lossy().to_string();
    let params_json = serde_json::to_string(&params_val).unwrap_or_else(|_| "{}".into());

    let mut expanded: Vec<String> = action
        .command
        .iter()
        .map(|arg| {
            arg.replace("{plugin_dir}", &plugin_dir)
                .replace("{bridge}", &bridge)
                .replace("{params_json}", &params_json)
        })
        .collect();

    // On Windows, if the first argument is `python3` and only `python` exists in PATH, normalize it.
    if cfg!(windows) && expanded.first().map(String::as_str) == Some("python3") {
        expanded[0] = "python".into();
    }

    let program = expanded.remove(0);
    let work_dir = plugin.dir.clone();
    let cli_path = find_filmcraft_cli();
    let plugin_id = plugin.id.clone();
    let action_id = action.id.clone();
    let action_label = action.label.clone();
    let undo_before = app.session.history.undo.len();
    let (tx, rx) = channel::<ActionOutcome>();
    let ctx_clone = ctx.clone();

    app.ui.status = format!("Running {}…", action.label);
    app.extensions.running = Some((action.label.clone(), rx, undo_before));

    std::thread::Builder::new()
        .name(format!("plugin-{}-{}", plugin.id, action.id))
        .spawn(move || {
            let mut cmd = Command::new(&program);
            cmd.args(&expanded)
                .current_dir(&work_dir)
                .env("FILMCRAFT_CONTROL_PORT", port.to_string())
                .env("FILMCRAFT_BRIDGE", &bridge)
                .env("AUTOEDIT_PARAMS_JSON", &params_json)
                .env("PYTHONIOENCODING", "utf-8");
            if let Some(cli) = cli_path {
                cmd.env("FILMCRAFT_CLI", cli);
            }
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                cmd.creation_flags(CREATE_NO_WINDOW);
            }
            let outcome = match cmd.output() {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    let ok = out.status.success();
                    let output = if !stdout.is_empty() && !stderr.is_empty() {
                        format!("{stdout}\n{stderr}")
                    } else if !stdout.is_empty() {
                        stdout
                    } else if !stderr.is_empty() {
                        stderr
                    } else if ok {
                        format!("{action_label}: completed")
                    } else {
                        format!("{action_label}: exited with {}", out.status)
                    };
                    ActionOutcome { plugin_id, action_id, action_label, ok, output }
                }
                Err(e) => ActionOutcome {
                    plugin_id,
                    action_id,
                    action_label,
                    ok: false,
                    output: format!("Failed to start `{program}`: {e}"),
                },
            };
            let _ = tx.send(outcome);
            ctx_clone.request_repaint();
        })
        .map_err(|e| format!("could not spawn plugin worker: {e}"))?;
    Ok(())
}

/// Render the dockable `PanelKind::Extensions` panel.
pub fn show(app: &mut FilmcraftApp, ui: &mut Ui, rect: Rect) {
    app.extensions.ensure_loaded();
    poll(app);

    let t = app.tokens;
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(10.0, 8.0))));
    let ui = &mut child;
    let tr = |s: &'static str| crate::i18n::t(s);

    // Top toolbar: Plugin selector, Undo button, Reload, Open Folder
    ui.horizontal(|ui| {
        if app.extensions.plugins.len() > 1 {
            let current_name = app
                .extensions
                .plugins
                .get(app.extensions.active_plugin)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| tr("Extensions").to_string());
            egui::ComboBox::from_id_salt("extensions-plugin-select")
                .selected_text(RichText::new(&current_name).strong().size(13.0))
                .show_ui(ui, |ui| {
                    for (i, p) in app.extensions.plugins.iter().enumerate() {
                        if ui.selectable_label(i == app.extensions.active_plugin, &p.name).clicked() {
                            app.extensions.active_plugin = i;
                        }
                    }
                });
        } else if let Some(p) = app.extensions.plugins.first() {
            ui.label(RichText::new(&p.name).strong().size(14.0).color(t.text));
            if !p.version.is_empty() {
                ui.label(RichText::new(format!("v{}", p.version)).size(11.0).color(t.text_faint));
            }
        } else {
            ui.label(RichText::new(tr("Extensions")).strong().size(13.5).color(t.text));
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let r_reload = ui.button(RichText::new("↻").size(12.5)).on_hover_text(tr("Reload Extensions"));
            app.auto.add("extensions.reload", r_reload.rect, "Reload Extensions");
            if r_reload.clicked() {
                app.extensions.reload();
            }

            if let Some((last_label, steps)) = app.extensions.undo_stack.last().cloned() {
                let undo_btn = ui
                    .add_enabled(
                        app.extensions.running.is_none(),
                        egui::Button::new(RichText::new(format!("↶ {} ({last_label})", tr("Undo"))).size(11.5)),
                    )
                    .on_hover_text(format!("Undo {steps} step(s) from {last_label}"));
                app.auto.add("extensions.undo", undo_btn.rect, "Undo");
                if undo_btn.clicked() {
                    app.extensions.undo_stack.pop();
                    for _ in 0..steps {
                        let _ = app.session.execute("edit.undo", json!({}));
                    }
                    let msg = format!("Undid {last_label}");
                    app.ui.status = msg.clone();
                    app.extensions.last_output = Some((true, msg));
                }
            }
        });
    });

    ui.add_space(4.0);

    // Status banner if running or last output exists
    if let Some((running_label, _, _)) = &app.extensions.running {
        Frame::new()
            .fill(t.accent.gamma_multiply(0.18))
            .stroke(Stroke::new(1.0, t.accent))
            .corner_radius(CornerRadius::same(5))
            .inner_margin(Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(format!("Running {running_label}…")).size(12.0).color(t.text));
                });
            });
        ui.add_space(4.0);
    } else if let Some((ok, msg)) = app.extensions.last_output.clone() {
        let border = if ok { t.accent.gamma_multiply(0.6) } else { Color32::from_rgb(210, 75, 75) };
        let bg = if ok { t.field_bg } else { Color32::from_rgba_unmultiplied(180, 40, 40, 35) };
        Frame::new()
            .fill(bg)
            .stroke(Stroke::new(1.0, border))
            .corner_radius(CornerRadius::same(5))
            .inner_margin(Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&msg).size(11.5).color(t.text));
                });
            });
        ui.add_space(4.0);
    }

    if app.extensions.plugins.is_empty() {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(tr("No plugins are installed.")).size(13.0).color(t.text_dim));
            ui.add_space(6.0);
            if let Some(dir) = plugins_dir() {
                ui.label(
                    RichText::new(format!("Place plugin folders with a plugin.json inside:\n{}", dir.display()))
                        .size(11.5)
                        .color(t.text_faint),
                );
                ui.add_space(8.0);
                if ui.button(tr("Open Extensions Folder…")).clicked() {
                    let _ = fs::create_dir_all(&dir);
                    if let Some(open) = app.hooks.open_path.as_mut() {
                        let _ = open(&dir.to_string_lossy(), false);
                    }
                }
            }
        });
        return;
    }

    let Some(plugin) = app.extensions.plugins.get(app.extensions.active_plugin).cloned() else {
        return;
    };

    if !plugin.description.is_empty() {
        ui.label(RichText::new(&plugin.description).size(11.5).color(t.text_dim));
        ui.add_space(4.0);
    }

    ui.separator();
    ui.add_space(4.0);

    let ctx = ui.ctx().clone();
    let is_busy = app.extensions.running.is_some();
    let mut trigger_action: Option<PluginAction> = None;

    ScrollArea::vertical()
        .id_salt("extensions-actions-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let mut current_category = String::new();
            for action in &plugin.actions {
                if !action.category.is_empty() && action.category != current_category {
                    current_category = action.category.clone();
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(current_category.to_uppercase())
                            .size(10.5)
                            .strong()
                            .color(t.text_faint),
                    );
                    ui.add_space(2.0);
                }

                Frame::new()
                    .fill(t.field_bg)
                    .stroke(Stroke::new(1.0, t.separator))
                    .corner_radius(CornerRadius::same(6))
                    .inner_margin(Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label(RichText::new(&action.label).strong().size(12.5).color(t.text));
                                if !action.description.is_empty() {
                                    ui.label(RichText::new(&action.description).size(11.0).color(t.text_dim));
                                }
                            });
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let btn = egui::Button::new(
                                    RichText::new(tr("Apply")).size(12.0).strong().color(Color32::WHITE),
                                )
                                .fill(t.accent)
                                .corner_radius(CornerRadius::same(4))
                                .min_size(vec2(64.0, 24.0));
                                let r = ui.add_enabled(!is_busy, btn);
                                app.auto.add(
                                    &format!("extensions.{}.{}.run", plugin.id, action.id),
                                    r.rect,
                                    &action.label,
                                );
                                if r.clicked() {
                                    trigger_action = Some(action.clone());
                                }
                            });
                        });

                        if !action.params.is_empty() {
                            ui.add_space(6.0);
                            let key_prefix = format!("{}.{}", plugin.id, action.id);
                            for param in &action.params {
                                draw_param_row(app, ui, &key_prefix, param);
                            }
                        }
                    });
                ui.add_space(6.0);
            }
        });

    if let Some(action) = trigger_action {
        let mut param_map = serde_json::Map::new();
        let key_prefix = format!("{}.{}", plugin.id, action.id);
        for p in &action.params {
            let val = app
                .extensions
                .values
                .get(&(key_prefix.clone(), p.id.clone()))
                .cloned()
                .unwrap_or_else(|| p.default.clone());
            param_map.insert(p.id.clone(), val);
        }
        if let Err(e) = spawn_plugin_action(app, &ctx, &plugin, &action, Value::Object(param_map)) {
            app.ui.status = e.clone();
            app.extensions.last_output = Some((false, e));
        }
    }
}

fn draw_param_row(app: &mut FilmcraftApp, ui: &mut Ui, key_prefix: &str, param: &PluginParam) {
    let t = app.tokens;
    let map_key = (key_prefix.to_string(), param.id.clone());
    let current = app
        .extensions
        .values
        .get(&map_key)
        .cloned()
        .unwrap_or_else(|| param.default.clone());

    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{}:", param.label)).size(11.5).color(t.text_dim));
        match param.kind {
            ParamKind::Int => {
                let mut v = current.as_i64().unwrap_or(0);
                let mut dv = egui::DragValue::new(&mut v).speed(1.0);
                if let (Some(min), Some(max)) = (param.min, param.max) {
                    dv = dv.range((min as i64)..=(max as i64));
                }
                if ui.add(dv).changed() {
                    app.extensions.values.insert(map_key, json!(v));
                }
            }
            ParamKind::Float => {
                let mut v = current.as_f64().unwrap_or(0.0);
                let mut dv = egui::DragValue::new(&mut v).speed(0.25).max_decimals(2);
                if let (Some(min), Some(max)) = (param.min, param.max) {
                    dv = dv.range(min..=max);
                }
                if ui.add(dv).changed() {
                    app.extensions.values.insert(map_key, json!(v));
                }
            }
            ParamKind::Bool => {
                let mut v = current.as_bool().unwrap_or(false);
                if ui.checkbox(&mut v, "").changed() {
                    app.extensions.values.insert(map_key, json!(v));
                }
            }
            ParamKind::Text => {
                let mut v = current.as_str().unwrap_or_default().to_string();
                let r = ui.add(egui::TextEdit::singleline(&mut v).desired_width(140.0));
                if r.changed() {
                    app.extensions.values.insert(map_key, json!(v));
                }
            }
            ParamKind::Choice => {
                let cur_str = current.as_str().unwrap_or_default().to_string();
                let shown = param
                    .choices
                    .iter()
                    .find(|c| c.value == cur_str)
                    .map(|c| c.label.clone())
                    .unwrap_or_else(|| cur_str.clone());
                egui::ComboBox::from_id_salt(("ext-param", key_prefix, &param.id))
                    .selected_text(RichText::new(shown).size(11.5))
                    .width(130.0)
                    .show_ui(ui, |ui| {
                        for c in &param.choices {
                            if ui.selectable_label(c.value == cur_str, &c.label).clicked() {
                                app.extensions.values.insert(map_key.clone(), json!(c.value));
                            }
                        }
                    });
            }
            ParamKind::File => {
                let mut v = current.as_str().unwrap_or_default().to_string();
                let r = ui.add(egui::TextEdit::singleline(&mut v).desired_width(120.0).hint_text("Optional file…"));
                if r.changed() {
                    app.extensions.values.insert(map_key.clone(), json!(v));
                }
                if ui.small_button(crate::i18n::t("Browse…")).clicked() {
                    let exts: Vec<&str> = if param.extensions.is_empty() {
                        vec!["mp3", "wav", "m4a", "flac", "aac", "ogg"]
                    } else {
                        param.extensions.iter().map(String::as_str).collect()
                    };
                    if let Some(pick) = app.hooks.pick_open_file.as_mut()
                        && let Some(chosen) = pick(&param.label, &exts)
                    {
                        app.extensions.values.insert(map_key, json!(chosen));
                    }
                }
            }
        }
    });
}

/// Render the installed plugins list inside `Edit ▸ Preferences ▸ Plugins…`.
pub fn settings_page(app: &mut FilmcraftApp, ui: &mut Ui) {
    app.extensions.ensure_loaded();
    let t = app.tokens;
    let tr = |s: &'static str| crate::i18n::t(s);

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if let Some(dir) = plugins_dir() {
            let b_open = ui.button(tr("Open Extensions Folder…"));
            app.auto.add("settings.plugins.openFolder", b_open.rect, "Open Extensions Folder");
            if b_open.clicked() {
                let _ = fs::create_dir_all(&dir);
                if let Some(open) = app.hooks.open_path.as_mut() {
                    let _ = open(&dir.to_string_lossy(), false);
                }
            }
        }
        let b_reload = ui.button(tr("Reload Extensions"));
        app.auto.add("settings.plugins.reload", b_reload.rect, "Reload Extensions");
        if b_reload.clicked() {
            app.extensions.reload();
        }
    });

    if !app.extensions.plugins.is_empty() {
        ui.add_space(8.0);
        for plugin in app.extensions.plugins.clone() {
            Frame::new()
                .fill(t.field_bg)
                .stroke(Stroke::new(1.0, t.separator))
                .corner_radius(CornerRadius::same(4))
                .inner_margin(Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&plugin.name).strong().size(13.0).color(t.text));
                        if !plugin.version.is_empty() {
                            ui.label(RichText::new(format!("v{}", plugin.version)).size(11.5).color(t.text_faint));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button(tr("Open Panel")).clicked() {
                                if let Some(idx) = app.extensions.plugins.iter().position(|p| p.id == plugin.id) {
                                    app.extensions.active_plugin = idx;
                                }
                                app.dialog = None;
                                app.show_panel(PanelKind::Extensions);
                            }
                        });
                    });
                    if !plugin.description.is_empty() {
                        ui.label(RichText::new(&plugin.description).size(11.5).color(t.text_dim));
                    }
                    ui.label(
                        RichText::new(format!("{} actions · {}", plugin.actions.len(), plugin.dir.display()))
                            .size(11.0)
                            .color(t.text_faint),
                    );
                });
            ui.add_space(4.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_plugin_manifests_in_directory() {
        let tmp = std::env::temp_dir().join(format!("filmcraft-plugin-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let plugin_dir = tmp.join("autoedit");
        fs::create_dir_all(&plugin_dir).unwrap();
        let manifest = json!({
            "id": "autoedit",
            "name": "AutoEdit",
            "version": "1.0.0",
            "description": "One-click edits for FilmCraft",
            "actions": [
                {
                    "id": "zoom",
                    "label": "Slow zoom",
                    "category": "Finish",
                    "command": ["python3", "{plugin_dir}/plugin_runner.py", "zoom"],
                    "params": [
                        { "id": "amount", "label": "Zoom (%)", "type": "float", "default": 8.0, "min": 1.0, "max": 30.0 }
                    ]
                }
            ]
        });
        fs::write(plugin_dir.join("plugin.json"), serde_json::to_vec(&manifest).unwrap()).unwrap();

        let found = discover_plugins_in(&tmp);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "autoedit");
        assert_eq!(found[0].name, "AutoEdit");
        assert_eq!(found[0].actions.len(), 1);
        assert_eq!(found[0].actions[0].id, "zoom");
        assert_eq!(found[0].dir, plugin_dir);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn route_opens_extension_panel_and_bridge_serves_requests() {
        let mut app = FilmcraftApp::new(filmcraft_engine::Session::default());
        app.extensions.plugins = vec![PluginManifest {
            id: "autoedit".into(),
            name: "AutoEdit".into(),
            version: "1.0.0".into(),
            description: "Test".into(),
            author: "AutoEdit".into(),
            actions: vec![],
            dir: PathBuf::from("/tmp/autoedit"),
        }];
        app.extensions.loaded = true;
        let ctx = egui::Context::default();
        let res = route(&mut app, &ctx, "window.extension.open.autoedit", &json!({})).unwrap().unwrap();
        assert_eq!(res["panel"], "Extensions");
        assert!(app.ui.dock.is_visible(PanelKind::Extensions));

        let port = ensure_bridge(&mut app, &ctx).expect("bridge port bound");
        assert!(port > 0);
        assert_eq!(app.control_port, Some(port));
    }
}
