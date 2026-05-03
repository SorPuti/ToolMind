use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

mod gui_automation;
mod vision_context;

use colored::*;
use dialoguer::{theme::ColorfulTheme, Input, Select};
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use wait_timeout::ChildExt;

type ToolFn = fn(Value) -> Value;

struct Tool {
    name: String,
    description: String,
    handler: ToolFn,
}

struct ToolRegistry {
    tools: HashMap<String, Tool>,
    /// Ordem estável para o prompt do sistema
    order: Vec<String>,
}

impl ToolRegistry {
    fn new() -> Self {
        Self {
            tools: HashMap::new(),
            order: Vec::new(),
        }
    }

    fn register(&mut self, tool: Tool) {
        println!("{} {}", "🔧 Registrando tool:".yellow(), tool.name);
        self.order.push(tool.name.clone());
        self.tools.insert(tool.name.clone(), tool);
    }

    fn execute(&self, name: &str, input: Value) -> Option<Value> {
        self.tools.get(name).map(|t| (t.handler)(input))
    }

    fn system_tools_block(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        for name in &self.order {
            if let Some(t) = self.tools.get(name) {
                lines.push(format!("- {} — {}", t.name, t.description));
            }
        }
        lines.join("\n")
    }
}

const TOOLMIND_DIR: &str = ".toolmind";
const PLAN_PROGRESS_FILE: &str = "plan_progress.json";
/// Estado do turno (mensagens + cadeia de tools) quando a API entra em limite / sobrecarga.
const FROZEN_API_TURN_FILE: &str = "frozen_api_turn.json";
/// Trilho de auditoria para automação / checkpoints (append JSONL).
const AUTOMATION_AUDIT_FILE: &str = "automation_audit.jsonl";
/// Evita loop infinito de tools no mesmo turno do usuário (provas longas precisam de várias tools).
const MAX_TOOL_CHAIN_PER_TURN: u32 = 48;

fn ensure_toolmind_dir() -> io::Result<PathBuf> {
    let p = PathBuf::from(TOOLMIND_DIR);
    fs::create_dir_all(&p)?;
    Ok(p)
}

fn safe_relative_path(raw: &str) -> Option<PathBuf> {
    if raw.is_empty() || raw.contains("..") {
        return None;
    }
    let p = Path::new(raw);
    if p.is_absolute() {
        return None;
    }
    Some(p.to_path_buf())
}

fn plan_current_path() -> PathBuf {
    PathBuf::from(TOOLMIND_DIR).join("current_plan.json")
}

fn plan_progress_path() -> PathBuf {
    PathBuf::from(TOOLMIND_DIR).join(PLAN_PROGRESS_FILE)
}

/// Opcional em **qualquer** tool: se `tool_input` inclui `plan_mark_done_through_step` (inteiro ≥ 0)
/// e o resultado da tool tem `status: "ok"`, marca em `plan_progress.json` todas as etapas
/// de índice `0..=through` como `done`. O significado do índice vem do modelo (mesma escala que `update_plan_step::step_index`).
fn plan_progress_apply_optional_mark(tool_input: &Value, tool_result: &Value) -> Option<Value> {
    if tool_result.get("status").and_then(|s| s.as_str()) != Some("ok") {
        return None;
    }
    let mark = tool_input.get("plan_mark_done_through_step")?;
    if mark.is_null() {
        return None;
    }
    let through_inclusive = mark.as_u64()? as usize;

    let path = plan_progress_path();
    let raw = fs::read_to_string(&path).ok()?;
    let mut prog: Value = serde_json::from_str(&raw).ok()?;
    let arr = prog.get_mut("step_statuses")?.as_array_mut()?;
    if arr.is_empty() {
        return None;
    }
    let last = through_inclusive.min(arr.len().saturating_sub(1));
    for j in 0..=last {
        arr[j] = json!("done");
    }
    fs::write(
        &path,
        serde_json::to_string_pretty(&prog).unwrap_or_default(),
    )
    .ok()?;
    Some(json!({
        "marked_done_through_step_index": last,
        "requested_through": through_inclusive,
        "note": "plan_progress.json atualizado via plan_mark_done_through_step no input da tool."
    }))
}

/// Inicializa progresso das etapas (tudo `pending`) alinhado ao `save_plan`.
fn init_plan_progress(dir: &Path, title: &str, plan_doc: &Value) -> io::Result<()> {
    let n = plan_doc["steps"].as_array().map(|a| a.len()).unwrap_or(0);
    let statuses: Value = Value::Array((0..n).map(|_| json!("pending")).collect());
    let prog = json!({
        "title": title,
        "plan_sync_key": plan_doc["saved_at_ms"],
        "step_statuses": statuses,
        "halt_requested": false,
        "halt_reason": ""
    });
    fs::write(
        dir.join(PLAN_PROGRESS_FILE),
        serde_json::to_string_pretty(&prog).unwrap_or_default(),
    )
}

fn step_line_preview(step: &Value, max_chars: usize) -> String {
    let raw = match step {
        Value::String(s) => s.clone(),
        _ => step.to_string(),
    };
    let t: String = raw.chars().filter(|c| !c.is_control()).take(max_chars).collect();
    if raw.chars().filter(|c| !c.is_control()).count() > max_chars {
        format!("{t}…")
    } else {
        t
    }
}

/// Painel estilo “modal” no console: etapas do plano, feitas e pendentes.
fn render_plan_execution_panel() {
    let plan_path = plan_current_path();
    let Ok(plan_raw) = fs::read_to_string(&plan_path) else {
        return;
    };
    let Ok(plan) = serde_json::from_str::<Value>(&plan_raw) else {
        return;
    };
    let Some(steps) = plan["steps"].as_array() else {
        return;
    };
    if steps.is_empty() {
        return;
    }

    let title = plan["title"]
        .as_str()
        .unwrap_or("Plano")
        .chars()
        .take(56)
        .collect::<String>();

    let progress_path = plan_progress_path();
    let (statuses, halted, halt_reason, prog_title) =
        match fs::read_to_string(&progress_path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        {
            Some(p) => {
                let st: Vec<String> = p["step_statuses"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let h = p["halt_requested"].as_bool().unwrap_or(false);
                let hr = p["halt_reason"].as_str().unwrap_or("").to_string();
                let pt = p["title"].as_str().unwrap_or("").to_string();
                (st, h, hr, pt)
            }
            None => {
                let n = steps.len();
                (vec!["?".into(); n], false, String::new(), String::new())
            }
        };

    let display_title = if prog_title.is_empty() {
        title
    } else {
        prog_title.chars().take(56).collect()
    };

    const W: usize = 76;
    let inner = W - 2;
    println!("\n{}", format!("╔{}╗", "═".repeat(inner)).bright_cyan());
    let head = format!(" PLANO · {} ", display_title)
        .chars()
        .take(inner)
        .collect::<String>();
    println!(
        "{}",
        format!("║{:<iw$}║", head, iw = inner).bright_cyan()
    );
    println!("{}", format!("╠{}╣", "═".repeat(inner)).bright_cyan());

    for (i, step) in steps.iter().enumerate() {
        let st = statuses
            .get(i)
            .map(|s| s.as_str())
            .unwrap_or("pending");
        let icon = match st {
            "done" => "✓",
            "in_progress" => "⋯",
            _ => "·",
        };
        let body = format!(" {:>2}. {} {}", i + 1, icon, step_line_preview(step, inner.saturating_sub(8)));
        let row: String = body.chars().take(inner).collect();
        println!("{}", format!("║{:<iw$}║", row, iw = inner).white());
    }

    println!("{}", format!("╠{}╣", "═".repeat(inner)).bright_cyan());
    let foot: String = if halted {
        format!(
            " PARADO: {}",
            halt_reason.chars().take(inner.saturating_sub(12)).collect::<String>()
        )
    } else if statuses.iter().all(|s| s == "done") && !statuses.is_empty() {
        " Todas as etapas concluídas. ".into()
    } else {
        " Em cadeia: conclua cada passo; pare só com update_plan_step (halt_execution:true). "
            .into()
    };
    let ftxt: String = foot.chars().take(inner).collect();
    let pad = inner.saturating_sub(ftxt.chars().count());
    let row = format!("║{}{}║", ftxt, " ".repeat(pad));
    if halted {
        println!("{}", row.yellow());
    } else if statuses.iter().all(|s| s == "done") && !statuses.is_empty() {
        println!("{}", row.green());
    } else {
        println!("{}", row.bright_black());
    }
    println!("{}", format!("╚{}╝", "═".repeat(inner)).bright_cyan());
    let _ = io::stdout().flush();
}

fn plan_execution_active_for_spinner() -> bool {
    let pp = plan_progress_path();
    if !pp.exists() {
        return false;
    }
    fs::read_to_string(&pp)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .map(|v| !v["halt_requested"].as_bool().unwrap_or(false))
        .unwrap_or(false)
}

// =========================
// TOOLS
// =========================
fn write_file(input: Value) -> Value {
    let filename = input["filename"].as_str().unwrap_or("file.txt");
    let content = input["content"].as_str().unwrap_or("");
    let Some(path) = safe_relative_path(filename) else {
        return json!({"status": "error", "error": "caminho inválido (use relativo, sem ..)"});
    };

    let existed_before = path.exists();
    match fs::write(&path, content) {
        Ok(_) => json!({
            "status": "ok",
            "file": path.to_string_lossy(),
            "existed_before": existed_before,
            "bytes_written": content.len(),
            "hint": "Se o objetivo era só criar/atualizar este arquivo uma vez, NÃO chame write_file de novo no mesmo filename neste turno; confirme ao usuário em texto ou use read_file/path_info se precisar validar."
        }),
        Err(e) => json!({"status": "error", "error": e.to_string()}),
    }
}

fn append_file(input: Value) -> Value {
    let filename = input["filename"].as_str().unwrap_or("file.txt");
    let content = input["content"].as_str().unwrap_or("");
    let Some(path) = safe_relative_path(filename) else {
        return json!({"status": "error", "error": "caminho inválido"});
    };

    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => match write!(f, "{}", content) {
            Ok(_) => json!({"status": "ok", "file": path.to_string_lossy()}),
            Err(e) => json!({"status": "error", "error": e.to_string()}),
        },
        Err(e) => json!({"status": "error", "error": e.to_string()}),
    }
}

fn read_file(input: Value) -> Value {
    let filename = input["filename"].as_str().unwrap_or("");
    let Some(path) = safe_relative_path(filename) else {
        return json!({"status": "error", "error": "caminho inválido"});
    };
    const MAX: u64 = 512 * 1024;
    match fs::metadata(&path) {
        Ok(m) if m.len() > MAX => {
            return json!({"status": "error", "error": "arquivo muito grande (>512KiB)"});
        }
        Err(e) => return json!({"status": "error", "error": e.to_string()}),
        _ => {}
    }
    match fs::read_to_string(&path) {
        Ok(s) => json!({"status": "ok", "file": path.to_string_lossy(), "content": s}),
        Err(e) => json!({"status": "error", "error": e.to_string()}),
    }
}

fn list_dir(input: Value) -> Value {
    let dirname = input["dirname"].as_str().unwrap_or(".");
    let Some(base) = safe_relative_path(dirname) else {
        return json!({"status": "error", "error": "caminho inválido"});
    };
    match fs::read_dir(&base) {
        Ok(entries) => {
            let mut names: Vec<String> = Vec::new();
            for e in entries.flatten() {
                names.push(e.file_name().to_string_lossy().into_owned());
            }
            names.sort();
            json!({"status": "ok", "dirname": base.to_string_lossy(), "entries": names})
        }
        Err(e) => json!({"status": "error", "error": e.to_string()}),
    }
}

/// Diretório de trabalho atual + amostra de entradas (para o modelo saber onde está).
fn workspace_context(_input: Value) -> Value {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let entries: Vec<String> = fs::read_dir(&cwd)
        .map(|rd| {
            let mut v: Vec<String> = rd
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v.truncate(100);
            v
        })
        .unwrap_or_default();

    json!({
        "status": "ok",
        "cwd": cwd.to_string_lossy(),
        "entries_sample": entries,
        "note": "Todos os caminhos em read_file/write_file/list_dir/path_info são relativos a este cwd (sem ..)."
    })
}

/// Metadados de um caminho relativo (existe? arquivo? tamanho?).
fn path_info(input: Value) -> Value {
    let rel = input["path"].as_str().unwrap_or("");
    let Some(p) = safe_relative_path(rel) else {
        return json!({"status": "error", "error": "caminho inválido (vazio, .. ou absoluto)"});
    };

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let full = cwd.join(&p);

    match fs::metadata(&full) {
        Ok(m) => {
            let modified_ms = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis());
            json!({
                "status": "ok",
                "path": p.to_string_lossy(),
                "exists": true,
                "is_file": m.is_file(),
                "is_dir": m.is_dir(),
                "size_bytes": m.len(),
                "modified_ms": modified_ms,
            })
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => json!({
            "status": "ok",
            "path": p.to_string_lossy(),
            "exists": false
        }),
        Err(e) => json!({"status": "error", "error": e.to_string()}),
    }
}

/// Executáveis permitidos para `run_command` / `run_detached` (file_stem do argv[0]).
/// Preferir tools `git_*` e `gh_*` para Git/PRs.
fn run_command_program_allowed(program: &str) -> bool {
    let stem = Path::new(program)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(program)
        .to_ascii_lowercase();
    let base = [
        "cargo", "rustc", "rustfmt", "rustup", "git", "gh", "rg", "npm", "node", "python", "pip",
        "where", "which", "find", "cat", "curl", "ssh", "openssl", "tesseract",
    ];
    if base.iter().any(|a| stem == *a) {
        return true;
    }
    #[cfg(target_os = "windows")]
    {
        return matches!(
            stem.as_str(),
            "cmd" | "powershell" | "pwsh" | "wt" | "tasklist" | "taskkill"
        );
    }
    #[cfg(target_os = "macos")]
    {
        return matches!(stem.as_str(), "ps" | "kill" | "bash" | "sh" | "open");
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        return matches!(
            stem.as_str(),
            "ps" | "kill" | "bash" | "sh" | "xdg-open" | "gnome-terminal" | "xterm" | "konsole"
        );
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        let _ = stem;
        false
    }
}

fn run_command_allowlist_hint() -> &'static str {
    "cargo, rustc, rustfmt, rustup, git, gh, rg, npm, node, python, pip, where, which, find, cat, curl, ssh, openssl, tesseract + extras por SO (Windows: cmd, powershell, pwsh, wt, tasklist, taskkill; macOS: +open; Linux: +ps, kill, bash, sh, xdg-open, gnome-terminal, xterm, konsole)"
}

fn read_pipe_limited(mut r: impl Read, max_bytes: usize) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    while buf.len() < max_bytes {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let take = (max_bytes - buf.len()).min(n);
                buf.extend_from_slice(&chunk[..take]);
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Executa `argv` com stdout/stderr pipados, timeout e limite de leitura (usado por run_command, git_* e gh_*).
fn execute_argv_piped(argv: Vec<String>, timeout_secs: u64, max_out: usize) -> Value {
    if argv.is_empty() {
        return json!({"status": "error", "error": "argv vazio"});
    }

    let timeout_secs = timeout_secs.clamp(1, 600);
    let max_out = max_out.clamp(1024, 2 * 1024 * 1024) as usize;
    let argv_for_json = argv.clone();

    let mut cmd = Command::new(&argv[0]);
    if argv.len() > 1 {
        cmd.args(&argv[1..]);
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return json!({"status": "error", "error": format!("falha ao iniciar processo: {e}")});
        }
    };

    let stdout_h = child.stdout.take();
    let stderr_h = child.stderr.take();
    let max_stdout = max_out;
    let max_stderr = max_out;
    let drain_out = thread::spawn(move || {
        stdout_h
            .map(|r| read_pipe_limited(r, max_stdout))
            .unwrap_or_default()
    });
    let drain_err = thread::spawn(move || {
        stderr_h
            .map(|r| read_pipe_limited(r, max_stderr))
            .unwrap_or_default()
    });

    let timeout = Duration::from_secs(timeout_secs);
    let wait_outcome = child.wait_timeout(timeout);

    let (status_code, timed_out) = match wait_outcome {
        Ok(Some(st)) => (st.code(), false),
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            (None, true)
        }
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = drain_out.join();
            let _ = drain_err.join();
            return json!({"status": "error", "error": format!("wait_timeout: {e}")});
        }
    };

    let stdout = drain_out.join().unwrap_or_default();
    let stderr = drain_err.join().unwrap_or_default();

    let truncated_note = (stdout.len() >= max_out || stderr.len() >= max_out)
        .then_some("saída pode estar truncada (max_output_bytes)");

    json!({
        "status": if timed_out { "timeout" } else { "ok" },
        "argv": argv_for_json,
        "exit_code": status_code,
        "timed_out": timed_out,
        "timeout_secs": timeout_secs,
        "stdout": stdout,
        "stderr": stderr,
        "note": truncated_note,
    })
}

/// Executa um comando sem shell: apenas `argv`, lista branca no binário, timeout e saída limitada.
fn run_command(input: Value) -> Value {
    let argv: Vec<String> = input["argv"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    if argv.is_empty() {
        return json!({
            "status": "error",
            "error": "argv deve ser array não vazio: [\"cargo\", \"build\", ...]"
        });
    }

    let program = &argv[0];
    if !run_command_program_allowed(program) {
        return json!({
            "status": "error",
            "error": format!(
                "programa não permitido pela lista branca: {}. {}",
                program, run_command_allowlist_hint()
            ),
        });
    }

    let timeout_secs: u64 = input["timeout_secs"].as_u64().unwrap_or(60).clamp(1, 300);
    let max_out: usize = input["max_output_bytes"]
        .as_u64()
        .unwrap_or(256 * 1024)
        .clamp(1024, 1024 * 1024) as usize;

    execute_argv_piped(argv, timeout_secs, max_out)
}

/// Mapa declarativo de padrões úteis por SO (o agente escolhe como compor `argv`; o host não interpreta texto livre).
fn command_playbook() -> Value {
    json!({
        "run_command": "argv sem shell; só binários permitidos pela lista branca.",
        "run_detached": "igual run_command porém sem esperar fim; útil para servidores ou UIs; devolve pid.",
        "computer_survey": "sondagem só leitura: executáveis comuns no PATH (sem instalar nada).",
        "open_url": "abre http(s) no browser/app por defeito (sem simular rato/tecla).",
        "automation_log": "append JSONL em .toolmind/automation_audit.jsonl (checkpoint/auditoria).",
        "gui_automation_arm": "Windows: uma confirmação no terminal; depois gui_* até disarm ou `parar` no prompt.",
        "gui_automation_disarm": "revoga sessão GUI (ficheiro .toolmind/gui_session.json).",
        "gui_automation_status": "armed?, armed_at_ms.",
        "gui_screen_snapshot": "PNG + título/rect da janela em foco + textual_summary (sem OCR).",
        "gui_mouse_move": "coordenadas absolutas no virtual desktop.",
        "gui_mouse_click": "left|right|middle; double opcional.",
        "gui_type_text": "Unicode para a janela em foco (SendInput).",
        "vision_to_context": "OCR Tesseract + layout: summary, text, elements, llm_block (imagem relativa).",
        "terminal_open": "abre terminal gráfico num cwd (caminho relativo seguro); variantes fixas no host.",
        "websocket_exchange": "uma sessão WS curta (handshake + mensagens); não substitui cliente interativo longo.",
        "scripts": "automatize com write_file/append_file (ex: .ps1, .sh, .bat relativo) e execute com run_command ou run_detached.",
        "ssh": "via run_command ou run_detached: [\"ssh\", \"-o\", \"BatchMode=yes\", \"user@host\", \"cmd\"]; chaves/agent já no ambiente do utilizador.",
        "windows": {
            "list_processes": ["tasklist"],
            "kill_by_pid": ["taskkill", "/PID", "<pid>", "/F"],
            "kill_by_image": ["taskkill", "/IM", "nome.exe", "/F"],
            "new_powershell_window_same_cwd": ["wt", "-w", "0", "nt", "-d", "<abs_cwd>", "powershell", "-NoExit"],
            "curl_headers": ["curl", "-sS", "-D", "-", "-o", "NUL", "https://example.com"]
        },
        "macos": {
            "list_processes": ["ps", "aux"],
            "open_terminal_here": ["open", "-a", "Terminal", "<path>"],
            "kill": ["kill", "-TERM", "<pid>"]
        },
        "linux_common": {
            "list_processes": ["ps", "aux"],
            "gnome_terminal_here": ["gnome-terminal", "--working-directory=<abs>"],
            "xterm_here": ["xterm", "-e", "bash", "-lc", "cd <abs> && exec bash"]
        }
    })
}

fn runtime_host(_input: Value) -> Value {
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    json!({
        "status": "ok",
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "family": std::env::consts::FAMILY,
        "cwd": cwd,
        "executable": std::env::current_exe().ok().map(|p| p.to_string_lossy().into_owned()),
        "env": {
            "COMSPEC": std::env::var("COMSPEC").ok(),
            "SHELL": std::env::var("SHELL").ok(),
            "PATH_present": std::env::var("PATH").map(|s| !s.is_empty()).unwrap_or(false),
            "WT_SESSION": std::env::var("WT_SESSION").ok(),
            "SSH_CONNECTION": std::env::var("SSH_CONNECTION").ok(),
        },
        "command_playbook": command_playbook(),
        "allowlist_hint": run_command_allowlist_hint(),
    })
}

#[cfg(windows)]
fn command_creation_flags(new_console: bool) -> u32 {
    if new_console {
        0x0000_0010
    } else {
        0
    }
}

/// Inicia processo sem esperar; `new_console` (Windows) abre console visível para o filho.
fn spawn_process_detached(argv: Vec<String>, new_console: bool) -> Result<u32, String> {
    if argv.is_empty() {
        return Err("argv vazio".into());
    }
    let mut cmd = Command::new(&argv[0]);
    if argv.len() > 1 {
        cmd.args(&argv[1..]);
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let flags = command_creation_flags(new_console);
        if flags != 0 {
            cmd.creation_flags(flags);
        }
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("falha ao spawn: {e}"))?;
    let pid = child.id();
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(pid)
}

fn run_detached(input: Value) -> Value {
    let argv: Vec<String> = input["argv"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if argv.is_empty() {
        return json!({
            "status": "error",
            "error": "argv deve ser array não vazio"
        });
    }
    let program = &argv[0];
    if !run_command_program_allowed(program) {
        return json!({
            "status": "error",
            "error": format!(
                "programa não permitido: {}. {}",
                program,
                run_command_allowlist_hint()
            ),
        });
    }
    let new_console = input["new_console"].as_bool().unwrap_or(false);
    match spawn_process_detached(argv.clone(), new_console) {
        Ok(pid) => json!({
            "status": "ok",
            "pid": pid,
            "argv": argv,
            "new_console": new_console,
            "note": "Um thread interno faz wait() para evitar zumbi; stdout/stderr foram descartados."
        }),
        Err(e) => json!({ "status": "error", "error": e }),
    }
}

fn probe_on_path(program_base: &str) -> bool {
    let exe_suffix = std::env::consts::EXE_SUFFIX;
    let Ok(paths) = std::env::var("PATH") else {
        return false;
    };
    let sep = if cfg!(windows) { ';' } else { ':' };
    for dir in paths.split(sep) {
        if dir.is_empty() {
            continue;
        }
        let base = Path::new(dir).join(program_base);
        if base.is_file() {
            return true;
        }
        if !exe_suffix.is_empty() && !program_base.ends_with(exe_suffix) {
            let with_ext = Path::new(dir).join(format!("{program_base}{exe_suffix}"));
            if with_ext.is_file() {
                return true;
            }
        }
    }
    false
}

fn computer_survey(_input: Value) -> Value {
    let names = [
        "powershell",
        "pwsh",
        "cmd",
        "curl",
        "python",
        "node",
        "npm",
        "git",
        "ssh",
        "wt",
        "explorer",
        "msedge",
        "chrome",
        "firefox",
        "tesseract",
    ];
    let on_path: Vec<Value> = names
        .iter()
        .map(|n| json!({ "name": n, "found": probe_on_path(n) }))
        .collect();
    json!({
        "status": "ok",
        "note": "Procura nomes no PATH (stem + EXE_SUFFIX no Windows). Não instala nem altera o sistema.",
        "executables_on_path": on_path,
        "recommendations": [
            "Antes de fluxos GUI frágeis, prefira open_url para links (YouTube, docs).",
            "Registe passos com automation_log; em ações destrutivas ou invasivas use ask_user_choice.",
            "Instalação de software: proponha comandos explícitos ao utilizador ou use run_command com binários já permitidos.",
            "Paralelização: vários run_detached ou várias instâncias toolmind em terminais separados — não há orquestrador multi-agente embutido.",
            "Para vision_to_context: instale Tesseract OCR e pacotes de idioma (ex. por+eng)."
        ]
    })
}

/// Abre URL no browser / app por defeito (sem coordenadas de rato).
fn open_url(input: Value) -> Value {
    let Some(url) = input.get("url").and_then(|v| v.as_str()) else {
        return json!({"status":"error","error":"campo url obrigatório (http:// ou https://)"});
    };
    let u = url.trim();
    if !(u.starts_with("https://") || u.starts_with("http://")) {
        return json!({"status":"error","error":"só URLs http:// ou https://"});
    }
    if u.len() > 4096 {
        return json!({"status":"error","error":"url demasiado longa"});
    }

    #[cfg(target_os = "windows")]
    {
        let argv = vec![
            "cmd".into(),
            "/c".into(),
            "start".into(),
            "".into(),
            u.to_string(),
        ];
        return match spawn_process_detached(argv, false) {
            Ok(pid) => json!({
                "status": "ok",
                "pid": pid,
                "url": u,
                "note": "cmd /c start \"\" <url> — browser ou handler por defeito."
            }),
            Err(e) => json!({"status":"error","error": e}),
        };
    }

    #[cfg(target_os = "macos")]
    {
        let argv = vec!["open".into(), u.to_string()];
        return match spawn_process_detached(argv, false) {
            Ok(pid) => json!({
                "status": "ok",
                "pid": pid,
                "url": u,
                "note": "open <url> — app por defeito."
            }),
            Err(e) => json!({"status":"error","error": e}),
        };
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if !probe_on_path("xdg-open") {
            return json!({"status":"error","error":"xdg-open não encontrado no PATH"});
        }
        let argv = vec!["xdg-open".into(), u.to_string()];
        return match spawn_process_detached(argv, false) {
            Ok(pid) => json!({
                "status": "ok",
                "pid": pid,
                "url": u,
                "note": "xdg-open — browser por defeito (requer sessão gráfica típica)."
            }),
            Err(e) => json!({"status":"error","error": e}),
        };
    }

    #[cfg(all(
        not(target_os = "windows"),
        not(target_os = "macos"),
        not(unix)
    ))]
    {
        json!({"status":"error","error":"open_url não suportado neste alvo de compilação"})
    }
}

fn automation_log(input: Value) -> Value {
    let step = input["step"].as_str().unwrap_or("step");
    let detail = input["detail"].as_str().unwrap_or("");
    let level = input["level"].as_str().unwrap_or("info");
    if step.len() > 512 || detail.len() > 8192 || level.len() > 32 {
        return json!({"status":"error","error":"step/detail/level demasiado longos"});
    }
    let Ok(dir) = ensure_toolmind_dir() else {
        return json!({"status":"error","error":"não foi possível criar .toolmind"});
    };
    let path = dir.join(AUTOMATION_AUDIT_FILE);
    let line = json!({
        "ts_ms": chrono_like_epoch_ms(),
        "level": level,
        "step": step,
        "detail": detail,
    });
    let mut s = line.to_string();
    s.push('\n');
    match OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(mut f) => match f.write_all(s.as_bytes()) {
            Ok(_) => json!({
                "status": "ok",
                "path": path.to_string_lossy(),
            }),
            Err(e) => json!({"status":"error","error": e.to_string()}),
        },
        Err(e) => json!({"status":"error","error": e.to_string()}),
    }
}

fn terminal_open(input: Value) -> Value {
    let variant = input["variant"].as_str().unwrap_or("wt_here");
    let wd = input["working_dir"].as_str().unwrap_or(".");
    let Some(rel) = safe_relative_path(wd) else {
        return json!({"status": "error", "error": "working_dir deve ser relativo e sem .."});
    };
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(e) => return json!({"status": "error", "error": e.to_string()}),
    };
    let target = cwd.join(rel);
    let abs = match target.canonicalize() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => return json!({"status": "error", "error": e.to_string()}),
    };

    #[cfg(target_os = "windows")]
    {
        let argv: Vec<String> = match variant {
            "wt_here" => vec![
                "wt".into(),
                "-w".into(),
                "0".into(),
                "nt".into(),
                "-d".into(),
                abs.clone(),
            ],
            "powershell_here" => vec![
                "wt".into(),
                "-w".into(),
                "0".into(),
                "nt".into(),
                "-d".into(),
                abs.clone(),
                "powershell".into(),
                "-NoExit".into(),
                "-NoLogo".into(),
            ],
            "cmd_here" => vec![
                "wt".into(),
                "-w".into(),
                "0".into(),
                "nt".into(),
                "-d".into(),
                abs.clone(),
                "cmd".into(),
            ],
            "powershell_start_fallback" => vec![
                "cmd".into(),
                "/c".into(),
                "start".into(),
                "".into(),
                "powershell".into(),
                "-NoExit".into(),
                "-NoLogo".into(),
                "-WorkingDirectory".into(),
                abs.clone(),
            ],
            "cmd_start_fallback" => vec![
                "cmd".into(),
                "/c".into(),
                "start".into(),
                "".into(),
                "cmd".into(),
                "/K".into(),
                "cd".into(),
                "/d".into(),
                abs.clone(),
            ],
            _ => {
                return json!({
                    "status": "error",
                    "error": format!(
                        "variant desconhecido: {variant}. Use: wt_here | powershell_here | cmd_here | powershell_start_fallback | cmd_start_fallback"
                    )
                });
            }
        };
        return match spawn_process_detached(argv, true) {
            Ok(pid) => json!({
                "status": "ok",
                "pid": pid,
                "variant": variant,
                "working_directory": abs,
                "note": "Se wt nao existir, tente powershell_start_fallback ou cmd_start_fallback."
            }),
            Err(e) => json!({ "status": "error", "error": e }),
        };
    }
    #[cfg(target_os = "macos")]
    {
        let argv = vec![
            "open".into(),
            "-a".into(),
            "Terminal".into(),
            abs.clone(),
        ];
        return match spawn_process_detached(argv, false) {
            Ok(pid) => json!({
                "status": "ok",
                "pid": pid,
                "variant": "macos_open_terminal",
                "working_directory": abs,
            }),
            Err(e) => json!({ "status": "error", "error": e }),
        };
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let argv = vec![
            "gnome-terminal".into(),
            format!("--working-directory={abs}"),
        ];
        return match spawn_process_detached(argv, false) {
            Ok(pid) => json!({
                "status": "ok",
                "pid": pid,
                "variant": "gnome_terminal_here",
                "working_directory": abs,
                "note": "Se gnome-terminal nao existir, use run_detached com xterm ou konsole (ver runtime_host command_playbook)."
            }),
            Err(e) => json!({ "status": "error", "error": e }),
        };
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        json!({"status": "error", "error": "terminal_open: alvo nao suportado neste build"})
    }
}

fn websocket_exchange(input: Value) -> Value {
    let Some(url_s) = input["url"].as_str() else {
        return json!({"status": "error", "error": "url (ws:// ou wss://) obrigatoria"});
    };
    if !url_s.starts_with("ws://") && !url_s.starts_with("wss://") {
        return json!({"status": "error", "error": "url deve comecar com ws:// ou wss://"});
    }
    let timeout_secs: u64 = input["timeout_secs"].as_u64().unwrap_or(30).clamp(5, 120);
    let max_recv: usize = input["receive_max"]
        .as_u64()
        .unwrap_or(16)
        .clamp(1, 128) as usize;
    let send_texts: Vec<String> = input["send_texts"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let url_owned = url_s.to_string();
    let handle = thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(r) => r,
            Err(e) => return json!({"status": "error", "error": e.to_string()}),
        };
        rt.block_on(async move {
            use futures_util::{SinkExt, StreamExt};
            use tokio_tungstenite::connect_async;
            use tokio_tungstenite::tungstenite::Message;

            if url::Url::parse(&url_owned).is_err() {
                return json!({"status": "error", "error": "URL invalida"});
            }

            let handshake = tokio::time::timeout(
                Duration::from_secs(timeout_secs),
                connect_async(&url_owned),
            )
            .await;

            let (mut ws, _resp) = match handshake {
                Ok(Ok(pair)) => pair,
                Ok(Err(e)) => {
                    return json!({"status": "error", "error": format!("handshake: {e}")});
                }
                Err(_) => {
                    return json!({"status": "error", "error": "timeout no handshake WebSocket"});
                }
            };

            for t in &send_texts {
                if let Err(e) = ws.send(Message::Text(t.clone().into())).await {
                    return json!({"status": "error", "error": format!("send: {e}")});
                }
            }

            let mut received: Vec<Value> = Vec::new();
            for _ in 0..max_recv {
                match tokio::time::timeout(Duration::from_secs(timeout_secs), ws.next()).await {
                    Ok(Some(Ok(msg))) => {
                        let mut close_after = false;
                        let entry = match msg {
                            Message::Text(t) => json!({"kind": "text", "data": t.to_string()}),
                            Message::Binary(b) => json!({"kind": "binary", "len": b.len()}),
                            Message::Ping(_) => json!({"kind": "ping"}),
                            Message::Pong(_) => json!({"kind": "pong"}),
                            Message::Close(f) => {
                                close_after = true;
                                json!({"kind": "close", "frame": format!("{f:?}")})
                            }
                            other => json!({"kind": "other", "meta": format!("{other:?}")}),
                        };
                        received.push(entry);
                        if close_after {
                            break;
                        }
                    }
                    Ok(Some(Err(e))) => {
                        received.push(json!({"kind": "error", "message": e.to_string()}));
                        break;
                    }
                    Ok(None) => break,
                    Err(_) => {
                        received.push(json!({"kind": "timeout"}));
                        break;
                    }
                }
            }
            let _ = ws.close(None).await;

            json!({
                "status": "ok",
                "url": url_owned,
                "received": received,
            })
        })
    });

    match handle.join() {
        Ok(v) => v,
        Err(_) => json!({"status": "error", "error": "thread websocket panicou"}),
    }
}

fn git_timeout_network() -> u64 {
    180
}

fn git_timeout_local() -> u64 {
    90
}

fn gh_timeout() -> u64 {
    180
}

/// Refs curtos seguros (branch, tag, HEAD~n, SHA parcial).
fn safe_git_refish(s: &str) -> bool {
    if s.is_empty() || s.len() > 120 {
        return false;
    }
    if s.starts_with('-') || s.contains("..") {
        return false;
    }
    s.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '.' | '/' | '_' | '-' | '@' | '{' | '}' | '^' | '~' | ':' | '+' | '*'
            )
    })
}

fn git_paths_from_input(input: &Value) -> Result<Vec<String>, String> {
    let paths: Vec<String> = input["paths"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if paths.is_empty() {
        return Err("paths deve ser array não vazio de caminhos relativos".into());
    }
    for p in &paths {
        if safe_relative_path(p).is_none() {
            return Err(format!("caminho inválido em paths: {p}"));
        }
    }
    Ok(paths)
}

fn git_status(_input: Value) -> Value {
    execute_argv_piped(
        vec![
            "git".into(),
            "status".into(),
            "-sb".into(),
            "--porcelain=v1".into(),
        ],
        git_timeout_local(),
        512 * 1024,
    )
}

fn git_diff(input: Value) -> Value {
    let staged = input["staged"].as_bool().unwrap_or(false);
    let path = input["path"].as_str();
    let mut argv = vec!["git".into(), "diff".into()];
    if staged {
        argv.push("--staged".into());
    }
    if let Some(p) = path.filter(|s| !s.is_empty()) {
        if safe_relative_path(p).is_none() {
            return json!({"status": "error", "error": "path inválido"});
        }
        argv.push("--".into());
        argv.push(p.into());
    }
    execute_argv_piped(argv, git_timeout_local(), 512 * 1024)
}

fn git_log(input: Value) -> Value {
    let n = input["max_count"].as_u64().unwrap_or(20).clamp(1, 200);
    execute_argv_piped(
        vec![
            "git".into(),
            "log".into(),
            "--oneline".into(),
            "--decorate".into(),
            "-n".into(),
            n.to_string(),
        ],
        git_timeout_local(),
        512 * 1024,
    )
}

/// mode: "list" (ramas locais) ou "list_all" (inclui remotas).
fn git_branch(input: Value) -> Value {
    let mode = input["mode"].as_str().unwrap_or("list");
    let mut argv = vec!["git".into(), "branch".into(), "-vv".into()];
    if mode == "list_all" {
        argv.push("-a".into());
    }
    execute_argv_piped(argv, git_timeout_local(), 256 * 1024)
}

fn git_remote(_input: Value) -> Value {
    execute_argv_piped(
        vec!["git".into(), "remote".into(), "-v".into()],
        git_timeout_local(),
        128 * 1024,
    )
}

fn git_add(input: Value) -> Value {
    let paths = match git_paths_from_input(&input) {
        Ok(p) => p,
        Err(e) => return json!({"status": "error", "error": e}),
    };
    let mut argv = vec!["git".into(), "add".into(), "--".into()];
    argv.extend(paths);
    execute_argv_piped(argv, git_timeout_local(), 256 * 1024)
}

fn git_commit(input: Value) -> Value {
    let msg = input["message"].as_str().unwrap_or("");
    if msg.trim().is_empty() {
        return json!({"status": "error", "error": "message é obrigatório"});
    }
    if msg.contains("--") {
        return json!({"status": "error", "error": "message não pode conter '--'"});
    }
    let allow_empty = input["allow_empty"].as_bool().unwrap_or(false);
    let mut argv = vec!["git".into(), "commit".into(), "-m".into(), msg.to_string()];
    if allow_empty {
        argv.push("--allow-empty".into());
    }
    execute_argv_piped(argv, git_timeout_local(), 256 * 1024)
}

fn git_push(input: Value) -> Value {
    let remote = input["remote"].as_str().unwrap_or("origin");
    if !safe_git_refish(remote) {
        return json!({"status": "error", "error": "remote inválido"});
    }
    let mut argv = vec!["git".into(), "push".into()];
    if input["set_upstream"].as_bool().unwrap_or(false) {
        argv.push("-u".into());
    }
    argv.push(remote.into());
    if let Some(b) = input["branch"].as_str().filter(|s| !s.is_empty()) {
        if !safe_git_refish(b) {
            return json!({"status": "error", "error": "branch inválida"});
        }
        argv.push(b.into());
    }
    execute_argv_piped(argv, git_timeout_network(), 512 * 1024)
}

fn git_pull(input: Value) -> Value {
    let remote = input["remote"].as_str().unwrap_or("origin");
    if !safe_git_refish(remote) {
        return json!({"status": "error", "error": "remote inválido"});
    }
    let mut argv = vec!["git".into(), "pull".into(), remote.into()];
    if let Some(b) = input["branch"].as_str().filter(|s| !s.is_empty()) {
        if !safe_git_refish(b) {
            return json!({"status": "error", "error": "branch inválida"});
        }
        argv.push(b.into());
    }
    execute_argv_piped(argv, git_timeout_network(), 512 * 1024)
}

fn git_fetch(input: Value) -> Value {
    let mut argv = vec!["git".into(), "fetch".into()];
    if let Some(r) = input["remote"].as_str().filter(|s| !s.is_empty()) {
        if !safe_git_refish(r) {
            return json!({"status": "error", "error": "remote inválido"});
        }
        argv.push(r.into());
    }
    if let Some(prune) = input["prune"].as_bool() {
        if prune {
            argv.push("--prune".into());
        }
    }
    execute_argv_piped(argv, git_timeout_network(), 512 * 1024)
}

fn git_checkout(input: Value) -> Value {
    let branch = match input["branch"].as_str().filter(|s| !s.is_empty()) {
        Some(b) => b,
        None => return json!({"status": "error", "error": "branch é obrigatório"}),
    };
    if !safe_git_refish(branch) {
        return json!({"status": "error", "error": "nome de branch inválido"});
    }
    let create = input["create_branch"].as_bool().unwrap_or(false);
    let mut argv = vec!["git".into(), "checkout".into()];
    if create {
        argv.push("-b".into());
    }
    argv.push(branch.into());
    if let Some(sp) = input["start_point"].as_str().filter(|s| !s.is_empty()) {
        if !safe_git_refish(sp) {
            return json!({"status": "error", "error": "start_point inválido"});
        }
        argv.push(sp.into());
    }
    execute_argv_piped(argv, git_timeout_local(), 256 * 1024)
}

fn git_show(input: Value) -> Value {
    let rev = input["rev"].as_str().unwrap_or("HEAD");
    if !safe_git_refish(rev) {
        return json!({"status": "error", "error": "rev inválido"});
    }
    let stat_only = input["stat_only"].as_bool().unwrap_or(true);
    let mut argv = vec!["git".into(), "show".into(), rev.into()];
    if stat_only {
        argv.push("--stat".into());
    }
    execute_argv_piped(argv, git_timeout_local(), 512 * 1024)
}

fn git_stash(input: Value) -> Value {
    let action = input["action"].as_str().unwrap_or("list");
    let mut argv = vec!["git".into(), "stash".into()];
    match action {
        "list" => {
            argv.push("list".into());
        }
        "push" => {
            argv.push("push".into());
            if let Some(m) = input["message"].as_str().filter(|s| !s.is_empty()) {
                if m.contains("--") {
                    return json!({"status": "error", "error": "message inválida"});
                }
                argv.push("-m".into());
                argv.push(m.into());
            }
        }
        "pop" => argv.push("pop".into()),
        "apply" => argv.push("apply".into()),
        _ => {
            return json!({
                "status": "error",
                "error": "action deve ser list | push | pop | apply"
            });
        }
    }
    execute_argv_piped(argv, git_timeout_local(), 256 * 1024)
}

fn git_rev_parse(input: Value) -> Value {
    let rev = input["rev"].as_str().unwrap_or("HEAD");
    if !safe_git_refish(rev) {
        return json!({"status": "error", "error": "rev inválido"});
    }
    execute_argv_piped(
        vec!["git".into(), "rev-parse".into(), rev.into()],
        git_timeout_local(),
        64 * 1024,
    )
}

fn gh_pr_list(input: Value) -> Value {
    let state = input["state"].as_str().unwrap_or("open");
    if !matches!(state, "open" | "closed" | "merged" | "all") {
        return json!({"status": "error", "error": "state: open|closed|merged|all"});
    }
    let limit = input["limit"].as_u64().unwrap_or(30).clamp(1, 100);
    execute_argv_piped(
        vec![
            "gh".into(),
            "pr".into(),
            "list".into(),
            "--state".into(),
            state.into(),
            "--limit".into(),
            limit.to_string(),
            "--json".into(),
            "number,title,state,isDraft,headRefName,baseRefName,url,author".into(),
        ],
        gh_timeout(),
        1024 * 1024,
    )
}

fn gh_pr_view(input: Value) -> Value {
    let n = input["number"].as_u64().or_else(|| input["pr"].as_u64());
    let Some(num) = n else {
        return json!({"status": "error", "error": "number (ou pr) é obrigatório"});
    };
    execute_argv_piped(
        vec![
            "gh".into(),
            "pr".into(),
            "view".into(),
            num.to_string(),
            "--json".into(),
            "number,title,state,body,isDraft,headRefName,baseRefName,url,author,commits,files".into(),
        ],
        gh_timeout(),
        1024 * 1024,
    )
}

fn gh_pr_diff(input: Value) -> Value {
    let n = input["number"].as_u64().or_else(|| input["pr"].as_u64());
    let Some(num) = n else {
        return json!({"status": "error", "error": "number (ou pr) é obrigatório"});
    };
    execute_argv_piped(
        vec!["gh".into(), "pr".into(), "diff".into(), num.to_string()],
        gh_timeout(),
        1024 * 1024,
    )
}

fn gh_pr_create(input: Value) -> Value {
    let title = input["title"].as_str().unwrap_or("");
    if title.trim().is_empty() {
        return json!({"status": "error", "error": "title é obrigatório"});
    }
    if title.starts_with('-') {
        return json!({"status": "error", "error": "title inválido"});
    }
    let body = input["body"].as_str().unwrap_or("");
    if body.contains("--") {
        return json!({"status": "error", "error": "body não pode conter '--'"});
    }
    let base = input["base"].as_str().unwrap_or("main");
    if !safe_git_refish(base) {
        return json!({"status": "error", "error": "base inválida"});
    }
    let mut argv = vec![
        "gh".into(),
        "pr".into(),
        "create".into(),
        "--title".into(),
        title.into(),
        "--body".into(),
        body.into(),
        "--base".into(),
        base.into(),
    ];
    if input["draft"].as_bool().unwrap_or(false) {
        argv.push("--draft".into());
    }
    if let Some(h) = input["head"].as_str().filter(|s| !s.is_empty()) {
        if !safe_git_refish(h) {
            return json!({"status": "error", "error": "head inválido"});
        }
        argv.push("--head".into());
        argv.push(h.into());
    }
    execute_argv_piped(argv, gh_timeout(), 512 * 1024)
}

fn gh_pr_close(input: Value) -> Value {
    let n = input["number"].as_u64().or_else(|| input["pr"].as_u64());
    let Some(num) = n else {
        return json!({"status": "error", "error": "number (ou pr) é obrigatório"});
    };
    execute_argv_piped(
        vec!["gh".into(), "pr".into(), "close".into(), num.to_string()],
        gh_timeout(),
        256 * 1024,
    )
}

fn gh_pr_merge(input: Value) -> Value {
    let n = input["number"].as_u64().or_else(|| input["pr"].as_u64());
    let Some(num) = n else {
        return json!({"status": "error", "error": "number (ou pr) é obrigatório"});
    };
    let method = input["method"].as_str().unwrap_or("squash");
    let flag = match method {
        "merge" => "--merge",
        "squash" => "--squash",
        "rebase" => "--rebase",
        _ => return json!({"status": "error", "error": "method: merge|squash|rebase"}),
    };
    execute_argv_piped(
        vec![
            "gh".into(),
            "pr".into(),
            "merge".into(),
            num.to_string(),
            flag.into(),
        ],
        gh_timeout(),
        512 * 1024,
    )
}

fn git_restore(input: Value) -> Value {
    let staged = input["staged"].as_bool().unwrap_or(false);
    let paths = match git_paths_from_input(&input) {
        Ok(p) => p,
        Err(e) => return json!({"status": "error", "error": e}),
    };
    let mut argv = vec!["git".into(), "restore".into()];
    if staged {
        argv.push("--staged".into());
    }
    argv.push("--".into());
    argv.extend(paths);
    execute_argv_piped(argv, git_timeout_local(), 256 * 1024)
}

fn gh_pr_comment(input: Value) -> Value {
    let n = input["number"].as_u64().or_else(|| input["pr"].as_u64());
    let Some(num) = n else {
        return json!({"status": "error", "error": "number (ou pr) é obrigatório"});
    };
    let body = input["body"].as_str().unwrap_or("");
    if body.trim().is_empty() {
        return json!({"status": "error", "error": "body é obrigatório"});
    }
    if body.contains("--") {
        return json!({"status": "error", "error": "body não pode conter '--'"});
    }
    execute_argv_piped(
        vec![
            "gh".into(),
            "pr".into(),
            "comment".into(),
            num.to_string(),
            "--body".into(),
            body.into(),
        ],
        gh_timeout(),
        256 * 1024,
    )
}

/// Marcador da opção automática "Outro" (texto livre).
const OTHER_OPTION_PREFIX: &str = "✎ Outro";

fn options_already_include_other_slot(options: &[String]) -> bool {
    options
        .iter()
        .any(|o| o.starts_with(OTHER_OPTION_PREFIX) || o.trim().eq_ignore_ascii_case("outro"))
}

fn find_other_option_index(options: &[String]) -> Option<usize> {
    options
        .iter()
        .position(|o| o.starts_with(OTHER_OPTION_PREFIX))
}

/// Animação leve antes do menu (carrossel + listagem escalonada).
fn animate_ask_prelude(question: &str, subtitle: Option<&str>, options: &[String], enabled: bool) {
    println!();
    println!("{}", " ╭── ToolMind · pergunta interativa ──╮".bright_cyan());
    println!("{}", format!(" │ {}", question).white().bold());
    if let Some(s) = subtitle.filter(|x| !x.is_empty()) {
        println!("{}", format!(" │ {}", s).bright_black());
    }
    println!("{}", " ╰────────────────────────────────────╯".bright_cyan());

    if !enabled || options.is_empty() {
        return;
    }

    let cap = options.len().saturating_mul(3).min(22);
    for tick in 0..cap {
        let j = tick % options.len();
        let preview = options[j].chars().take(70).collect::<String>();
        print!(
            "\r {}  {} ",
            "◌".bright_yellow(),
            format!("[{}] {}", j + 1, preview).bright_black()
        );
        let _ = io::stdout().flush();
        thread::sleep(Duration::from_millis(48));
    }
    println!();
    println!(
        "{}",
        "   ▸ A seguir: menu com ↑↓ e Enter — Esc cancela (modo dialoguer)"
            .bright_black()
            .italic()
    );
    thread::sleep(Duration::from_millis(70));
}

fn finalize_choice_json(
    question: &str,
    subtitle: Option<&str>,
    footer_hint: &str,
    idx: usize,
    options: &[String],
    free_text: Option<String>,
) -> Value {
    let is_other = find_other_option_index(options) == Some(idx);
    let (kind, choice, free_json) = if is_other {
        let ft = free_text.unwrap_or_default();
        let trimmed = ft.trim();
        let display = if trimmed.is_empty() {
            "[Outro · sem texto]".to_string()
        } else {
            trimmed.to_string()
        };
        (
            "other",
            display,
            Some(Value::String(trimmed.to_string())),
        )
    } else {
        ("preset", options[idx].clone(), None)
    };

    let mut out = json!({
        "status": "ok",
        "kind": kind,
        "index": idx,
        "selected_index": idx,
        "choice": choice,
        "question": question,
        "subtitle": subtitle.unwrap_or(""),
        "footer_hint": footer_hint,
    });
    if let Some(ft) = free_json {
        out["free_text"] = ft;
    }
    out
}

fn ask_user_choice_dialoguer(
    prompt: &str,
    options: &[String],
    default_index: usize,
) -> Result<usize, String> {
    let theme = ColorfulTheme::default();
    Select::with_theme(&theme)
        .with_prompt(prompt)
        .items(options)
        .default(default_index.min(options.len().saturating_sub(1)))
        .interact_opt()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "cancelled".to_string())
}

fn ask_user_choice_simple(
    prompt: &str,
    options: &[String],
    footer_hint: &str,
) -> Result<(usize, Option<String>), String> {
    println!("{}", prompt.bold());
    if !footer_hint.is_empty() {
        println!("{}", footer_hint.bright_black());
    }
    for (i, o) in options.iter().enumerate() {
        println!("  [{}] {}", i + 1, o);
    }
    let other_idx = find_other_option_index(options);
    print!(
        "{}",
        "Digite o número da opção (ou 'o' / 'outro' para resposta livre): "
            .bright_blue()
    );
    io::stdout().flush().map_err(|e| e.to_string())?;

    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    let t = line.trim();
    if t.eq_ignore_ascii_case("o") || t.eq_ignore_ascii_case("outro") {
        let idx = other_idx.ok_or_else(|| "esta pergunta não tem opção Outro".to_string())?;
        print!("{}", "Texto livre: ".bright_cyan());
        io::stdout().flush().map_err(|e| e.to_string())?;
        let mut ft = String::new();
        io::stdin()
            .read_line(&mut ft)
            .map_err(|e| e.to_string())?;
        return Ok((idx, Some(ft)));
    }
    let n: usize = t
        .parse()
        .map_err(|_| "número inválido".to_string())?;
    if n < 1 || n > options.len() {
        return Err("número fora do intervalo".into());
    }
    let idx = n - 1;
    if other_idx == Some(idx) {
        print!("{}", "Texto livre: ".bright_cyan());
        io::stdout().flush().map_err(|e| e.to_string())?;
        let mut ft = String::new();
        io::stdin()
            .read_line(&mut ft)
            .map_err(|e| e.to_string())?;
        return Ok((idx, Some(ft)));
    }
    Ok((idx, None))
}

/// Pergunta interativa: menu com setas (dialoguer), animação opcional, **sempre** opção Outro (texto livre) por padrão.
///
/// Campos úteis em `input`:
/// - `question`, `options` (mín. 1 opção fixa; recomendado ≥2)
/// - `subtitle`, `footer_hint`, `default_index`
/// - `include_other` (default **true**) — acrescenta opção "✎ Outro — …"
/// - `animate_intro` (default **false**)
/// - `ui_mode`: `"dialoguer"` (default) ou `"simple"` (stdin só número / `o`)
fn ask_user_choice(input: Value) -> Value {
    let question = input["question"].as_str().unwrap_or("Escolha:");
    let subtitle = input["subtitle"].as_str().filter(|s| !s.is_empty());
    let footer_hint = input["footer_hint"].as_str().unwrap_or("");

    let include_other = input["include_other"].as_bool().unwrap_or(true);
    let animate_intro = input["animate_intro"].as_bool().unwrap_or(false);
    let ui_mode = input["ui_mode"].as_str().unwrap_or("dialoguer");

    let mut options: Vec<String> = input["options"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    if options.is_empty() {
        return json!({"status": "error", "error": "options não pode ser vazio"});
    }
    if !include_other && options.len() < 2 {
        return json!({
            "status": "error",
            "error": "inclua pelo menos 2 opções ou defina include_other: true para adicionar 'Outro'."
        });
    }

    if include_other && !options_already_include_other_slot(&options) {
        options.push(format!(
            "{} — resposta livre (texto customizado)",
            OTHER_OPTION_PREFIX
        ));
    }

    if options.len() < 2 {
        return json!({"status": "error", "error": "é necessário ao menos 2 opções no total."});
    }

    let default_index = input["default_index"]
        .as_u64()
        .map(|n| n as usize)
        .unwrap_or(0)
        .min(options.len().saturating_sub(1));

    let prompt_main = match subtitle {
        Some(s) => format!("{}\n{}", question, s),
        None => question.to_string(),
    };

    animate_ask_prelude(
        question,
        subtitle,
        &options,
        animate_intro,
    );

    println!("\n{}", "❓ Selecione uma opção".bright_cyan().bold());

    let (idx, free_text) = match ui_mode {
        "simple" => match ask_user_choice_simple(&prompt_main, &options, footer_hint) {
            Ok(v) => v,
            Err(e) => return json!({"status": "error", "error": e}),
        },
        _ => match ask_user_choice_dialoguer(&prompt_main, &options, default_index) {
            Ok(i) => {
                let ft = if find_other_option_index(&options) == Some(i) {
                    let theme = ColorfulTheme::default();
                    match Input::with_theme(&theme)
                        .with_prompt("Sua resposta (livre)")
                        .allow_empty(true)
                        .interact_text()
                    {
                        Ok(s) => Some(s),
                        Err(e) => return json!({"status": "error", "error": e.to_string()}),
                    }
                } else {
                    None
                };
                (i, ft)
            }
            Err(e) if e == "cancelled" => {
                return json!({
                    "status": "cancelled",
                    "message": "Seleção cancelada (Esc). Chame ask_user_choice novamente ou siga em modo texto se o usuário preferir."
                });
            }
            Err(e) => {
                return json!({
                    "status": "error",
                    "error": format!("{e}. Tente ui_mode: \"simple\"."),
                });
            }
        },
    };

    finalize_choice_json(
        question,
        subtitle,
        footer_hint,
        idx,
        &options,
        free_text,
    )
}

/// Grava um plano rico (passos, riscos, critérios) em `.toolmind/current_plan.json`.
fn save_plan(input: Value) -> Value {
    let title = input["title"].as_str().unwrap_or("plano");
    let steps = input["steps"].clone();
    let Ok(dir) = ensure_toolmind_dir() else {
        return json!({"status": "error", "error": "não foi possível criar .toolmind"});
    };
    let path = dir.join("current_plan.json");

    let doc = json!({
        "title": title,
        "steps": steps,
        "saved_at_ms": chrono_like_epoch_ms(),
        "risks": input.get("risks").cloned().unwrap_or_else(|| json!([])),
        "open_questions": input.get("open_questions").cloned().unwrap_or_else(|| json!([])),
        "definition_of_done": input.get("definition_of_done").cloned().unwrap_or_else(|| json!([])),
        "assumptions": input.get("assumptions").cloned().unwrap_or_else(|| json!([])),
        "notes": input.get("notes").cloned().unwrap_or_else(|| json!("")),
        "alternatives_rejected": input.get("alternatives_rejected").cloned().unwrap_or_else(|| json!([])),
    });

    match fs::write(&path, serde_json::to_string_pretty(&doc).unwrap_or_default()) {
        Ok(_) => {
            let progress_note = match init_plan_progress(&dir, title, &doc) {
                Ok(()) => format!("{}/{}", TOOLMIND_DIR, PLAN_PROGRESS_FILE),
                Err(e) => format!("(progresso não inicializado: {e})"),
            };
            json!({
                "status": "ok",
                "path": path.to_string_lossy(),
                "plan_progress_path": progress_note,
                "hint": "Progresso: update_plan_step ou, em qualquer tool com status ok, plan_mark_done_through_step (índice inclusivo). halt_execution:true para parar a cadeia."
            })
        }
        Err(e) => json!({"status": "error", "error": e.to_string()}),
    }
}

fn chrono_like_epoch_ms() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn load_plan(_input: Value) -> Value {
    let path = plan_current_path();
    match fs::read_to_string(&path) {
        Ok(s) => match serde_json::from_str::<Value>(&s) {
            Ok(v) => {
                let mut out = json!({"status": "ok", "plan": v});
                let pp = plan_progress_path();
                if let Ok(pr) = fs::read_to_string(&pp) {
                    if let Ok(pv) = serde_json::from_str::<Value>(&pr) {
                        out["progress"] = pv;
                    }
                }
                out
            }
            Err(e) => json!({"status": "error", "error": e.to_string()}),
        },
        Err(e) => json!({"status": "error", "error": e.to_string()}),
    }
}

/// Atualiza o estado de uma etapa do plano (e opcionalmente pede parada da cadeia de tools).
fn update_plan_step(input: Value) -> Value {
    let idx = input["step_index"].as_u64().or_else(|| input["index"].as_u64());
    let Some(i) = idx else {
        return json!({"status": "error", "error": "step_index (ou index) é obrigatório"});
    };
    let status = input["status"].as_str().unwrap_or("done");
    if !matches!(status, "done" | "in_progress" | "pending") {
        return json!({"status": "error", "error": "status deve ser: done | in_progress | pending"});
    }
    let halt = input["halt_execution"].as_bool().unwrap_or(false);
    let halt_reason = input["halt_reason"].as_str().unwrap_or("parada solicitada pelo agente");

    let path = plan_progress_path();
    let Ok(raw) = fs::read_to_string(&path) else {
        return json!({"status": "error", "error": "plan_progress.json não encontrado; execute save_plan antes."});
    };
    let mut prog: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => return json!({"status": "error", "error": e.to_string()}),
    };
    let Some(arr) = prog.get_mut("step_statuses").and_then(|x| x.as_array_mut()) else {
        return json!({"status": "error", "error": "step_statuses ausente ou inválido"});
    };
    let ii = i as usize;
    if ii >= arr.len() {
        return json!({
            "status": "error",
            "error": format!("step_index {ii} fora do intervalo (0..{})", arr.len())
        });
    }
    if status == "done" {
        for j in 0..ii {
            if arr[j].as_str() != Some("done") {
                arr[j] = json!("done");
            }
        }
    }
    arr[ii] = json!(status);
    if halt {
        prog["halt_requested"] = json!(true);
        prog["halt_reason"] = json!(halt_reason);
    }
    match fs::write(
        &path,
        serde_json::to_string_pretty(&prog).unwrap_or_default(),
    ) {
        Ok(_) => json!({
            "status": "ok",
            "step_index": i,
            "status_applied": status,
            "halt_requested": halt,
        }),
        Err(e) => json!({"status": "error", "error": e.to_string()}),
    }
}

/// Auto-crítica / retrospectiva da sessão (append em `.toolmind/self_reflections.jsonl`).
fn reflect_session(input: Value) -> Value {
    let critique = input["critique"].as_str().unwrap_or("");
    let hypothesis = input["improvement_hypothesis"].as_str().unwrap_or("");
    let severity = input["severity"].as_str().unwrap_or("medium");
    let next = input
        .get("next_experiments")
        .cloned()
        .unwrap_or_else(|| json!([]));

    if critique.trim().is_empty() {
        return json!({"status": "error", "error": "preencha critique (texto obrigatório)"});
    }

    let Ok(dir) = ensure_toolmind_dir() else {
        return json!({"status": "error", "error": "não foi possível criar .toolmind"});
    };
    let path = dir.join("self_reflections.jsonl");
    let entry = json!({
        "critique": critique,
        "improvement_hypothesis": hypothesis,
        "severity": severity,
        "next_experiments": next,
        "ts_ms": chrono_like_epoch_ms(),
    });
    let mut line = entry.to_string();
    line.push('\n');
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => match f.write_all(line.as_bytes()) {
            Ok(_) => json!({"status": "ok", "path": path.to_string_lossy()}),
            Err(e) => json!({"status": "error", "error": e.to_string()}),
        },
        Err(e) => json!({"status": "error", "error": e.to_string()}),
    }
}

fn create_tool(input: Value) -> Value {
    let name = input["name"].as_str().unwrap_or("unknown");
    let description = input["description"].as_str().unwrap_or("");
    let suggested_impl = input["suggested_impl"].as_str().unwrap_or("");

    println!("{}", "⚠️ Pedido de nova tool (registro pendente)".yellow());
    println!("Nome: {}", name);
    println!("Desc: {}", description);

    let Ok(dir) = ensure_toolmind_dir() else {
        return json!({"status": "error", "error": "não foi possível criar .toolmind"});
    };
    let path = dir.join("pending_tools.jsonl");
    let line = json!({
        "name": name,
        "description": description,
        "suggested_impl": suggested_impl,
        "requested_at_ms": chrono_like_epoch_ms(),
    });
    let mut line_str = line.to_string();
    line_str.push('\n');
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => match f.write_all(line_str.as_bytes()) {
            Ok(_) => json!({
                "status": "ok",
                "note": "Pedido anexado a .toolmind/pending_tools.jsonl para você integrar em Rust.",
                "path": path.to_string_lossy()
            }),
            Err(e) => json!({"status": "error", "error": e.to_string()}),
        },
        Err(e) => json!({"status": "error", "error": e.to_string()}),
    }
}

/// Recorta o último objeto JSON que começa com `{"tool"` usando balanceamento de chaves (suporta texto/prosa antes ou lixo depois).
fn slice_last_balanced_tool_json(text: &str) -> Option<&str> {
    let start = text.rfind("{\"tool\"")?;
    let slice = &text[start..];
    let mut depth = 0i32;
    let mut end_byte: Option<usize> = None;
    for (i, ch) in slice.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end_byte = Some(i + ch.len_utf8());
                    break;
                }
            }
            _ => {}
        }
    }
    let end = end_byte?;
    Some(&slice[..end])
}

/// Extrai `{"tool":"...","input":{...}}` da resposta (texto puro, bloco ```json, ou último objeto balanceado).
fn extract_tool_call(text: &str) -> Option<(String, Value)> {
    let trimmed = text.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        if let Some(name) = v["tool"].as_str() {
            return Some((name.to_string(), v["input"].clone()));
        }
    }
    // Último bloco ```json ... ```
    let mut cursor = text;
    let mut last_inner: Option<&str> = None;
    while let Some(start) = cursor.find("```json") {
        let rest = &cursor[start + 7..];
        if let Some(end) = rest.find("```") {
            last_inner = Some(rest[..end].trim());
            cursor = &rest[end + 3..];
        } else {
            break;
        }
    }
    if let Some(inner) = last_inner {
        if let Ok(v) = serde_json::from_str::<Value>(inner) {
            if let Some(name) = v["tool"].as_str() {
                return Some((name.to_string(), v["input"].clone()));
            }
        }
    }
    if let Some(slice) = slice_last_balanced_tool_json(text) {
        if let Ok(v) = serde_json::from_str::<Value>(slice) {
            if let Some(name) = v["tool"].as_str() {
                return Some((name.to_string(), v["input"].clone()));
            }
        }
    }
    None
}

/// Apaga a linha atual (spinner) no **thread principal**, após `join` do spinner — evita `\r` a competir com o stream.
fn erase_spinner_line_main() {
    print!("\x1b[2K\r");
    let _ = io::stdout().flush();
}

/// Texto de raciocínio / pensamento: cinza escuro (legível em fundo claro).
fn style_thinking(s: &str) -> ColoredString {
    s.truecolor(72, 76, 84)
}

/// Destaca início de JSON no pensamento (API mistura pensamento com rascunho de tool).
fn thinking_json_prefix(t: &str) -> Option<&'static str> {
    let u = t.trim_start();
    if u.starts_with('{') && (u.contains("\"tool\"") || u.contains("\"input\"")) {
        Some("⎿ ")
    } else if u.starts_with("\"tool\"") || u.starts_with("\"input\"") || u.starts_with("tool\"") {
        Some("⎿ … ")
    } else {
        None
    }
}

/// Spinner até `stop` ser true; **não** use `\r` extra ao sair — o thread principal apaga a linha após `join`.
fn spawn_thinking_spinner(stop: Arc<AtomicBool>, header: &'static str) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let phases = [
            "conectando à API",
            "carregando mensagens",
            "aguardando o primeiro token",
            "modelo gerando texto",
            "avaliando ferramentas",
        ];
        let mut tick: u64 = 0;
        while !stop.load(Ordering::Relaxed) {
            let frame = frames[(tick as usize) % frames.len()];
            let phase = phases[((tick / 14) as usize).min(phases.len() - 1)];
            print!(
                "\r {} {}  {}  {}",
                "ToolMind".bright_cyan().bold(),
                frame.bright_yellow(),
                header.bold().white(),
                phase.truecolor(95, 98, 105)
            );
            print!("{}", " ".repeat(8));
            let _ = io::stdout().flush();
            thread::sleep(Duration::from_millis(82));
            tick = tick.wrapping_add(1);
        }
    })
}

fn print_tool_banner(tool_name: &str) {
    println!();
    println!(
        " {} {} {}",
        "▶".bright_green(),
        "Ferramenta".green().bold(),
        tool_name.bright_yellow().bold()
    );
    println!("{}", " ───────────────────────────────".bright_black());
}

#[derive(Clone)]
struct Message {
    role: String,
    content: String,
}

fn frozen_api_turn_path() -> PathBuf {
    PathBuf::from(TOOLMIND_DIR).join(FROZEN_API_TURN_FILE)
}

fn http_status_retryable(code: u16) -> bool {
    matches!(
        code,
        408 | 429 | 500 | 502 | 503 | 504
    )
}

fn parse_retry_after_seconds(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

fn reqwest_error_retryable(e: &reqwest::Error) -> bool {
    if e.is_timeout() || e.is_connect() {
        return true;
    }
    e.status()
        .map(|s| http_status_retryable(s.as_u16()))
        .unwrap_or(false)
}

fn save_frozen_api_turn(
    messages: &[Message],
    tool_chain_count: u32,
    last_tool_sig: &Option<(String, String)>,
    reason: &str,
    http_status: Option<u16>,
    detail: &str,
) -> io::Result<()> {
    let _ = ensure_toolmind_dir()?;
    let sig_json = last_tool_sig
        .as_ref()
        .map(|(a, b)| json!({"tool": a, "input_sig": b}))
        .unwrap_or(Value::Null);
    let doc = json!({
        "saved_at_ms": chrono_like_epoch_ms(),
        "reason": reason,
        "http_status": http_status,
        "detail_preview": detail.chars().take(800).collect::<String>(),
        "tool_chain_count": tool_chain_count,
        "last_tool_sig": sig_json,
        "messages": messages.iter().map(|m| json!({"role": m.role, "content": m.content})).collect::<Vec<_>>(),
    });
    fs::write(
        frozen_api_turn_path(),
        serde_json::to_string_pretty(&doc).unwrap_or_default(),
    )
}

fn clear_frozen_api_turn() {
    let _ = fs::remove_file(frozen_api_turn_path());
}

fn warn_frozen_turn_if_present() {
    let p = frozen_api_turn_path();
    if p.exists() {
        println!(
            "{}",
            format!(
                "Nota: existe `{}` de um turno interrompido por limite/sobrecarga da API. Será substituído na próxima gravação ou removido após sucesso.",
                p.display()
            )
            .bright_yellow()
        );
    }
}

/// Segundos de espera antes de nova tentativa à API (respeita Retry-After quando houver).
fn calc_backoff_sleep_secs(backoff: u64, consecutive_fails: u32, retry_after: Option<u64>) -> u64 {
    let base = retry_after.unwrap_or(backoff).max(1).min(300);
    let circuit_extra = if consecutive_fails >= 5 { 45u64 } else { 0 };
    base.saturating_add(circuit_extra).min(300)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    let api_key = std::env::var("NVIDIA_API_KEY").map_err(|_| {
        "NVIDIA_API_KEY não encontrada. Defina no ambiente ou crie .env na raiz com:\nNVIDIA_API_KEY=sua_chave"
    })?;

    let client = Client::new();

    let mut registry = ToolRegistry::new();

    registry.register(Tool {
        name: "write_file".into(),
        description: "Escreve/sobrescreve arquivo (caminho relativo, sem ..). Parâmetros: filename, content."
            .into(),
        handler: write_file,
    });
    registry.register(Tool {
        name: "append_file".into(),
        description: "Anexa conteúdo ao final de um arquivo. Parâmetros: filename, content."
            .into(),
        handler: append_file,
    });
    registry.register(Tool {
        name: "read_file".into(),
        description: "Lê texto de um arquivo (máx. 512KiB). Parâmetros: filename."
            .into(),
        handler: read_file,
    });
    registry.register(Tool {
        name: "list_dir".into(),
        description: "Lista nomes no diretório relativo. Parâmetros: dirname (ex: \".\" ou \"src\")."
            .into(),
        handler: list_dir,
    });
    registry.register(Tool {
        name: "workspace_context".into(),
        description: "Mostra diretório atual (cwd) e amostra de arquivos/pastas. Use antes de escrever para saber onde está. Sem parâmetros ou {}."
            .into(),
        handler: workspace_context,
    });
    registry.register(Tool {
        name: "path_info".into(),
        description: "Metadados de um caminho relativo: existe?, arquivo/pasta, tamanho. Parâmetros: path (string relativa)."
            .into(),
        handler: path_info,
    });
    registry.register(Tool {
        name: "runtime_host".into(),
        description: "SO, cwd, variáveis úteis (COMSPEC/SHELL/PATH) e command_playbook (mapa declarativo de padrões: terminais, processos, curl, ssh). Sem parâmetros ou {}."
            .into(),
        handler: runtime_host,
    });
    registry.register(Tool {
        name: "computer_survey".into(),
        description: "Sondagem só leitura: executáveis comuns no PATH (powershell, git, browsers, etc.). Não instala nada. Parâmetros: {}."
            .into(),
        handler: computer_survey,
    });
    registry.register(Tool {
        name: "open_url".into(),
        description: "Abre URL http(s) no browser ou handler por defeito do SO (sem simular rato/tecla). Parâmetros: url."
            .into(),
        handler: open_url,
    });
    registry.register(Tool {
        name: "automation_log".into(),
        description: "Regista checkpoint/auditoria em .toolmind/automation_audit.jsonl (append JSONL). Parâmetros: step, detail (opcional), level info|warn|error (opcional)."
            .into(),
        handler: automation_log,
    });
    registry.register(Tool {
        name: "gui_automation_arm".into(),
        description: "Windows: pede UMA confirmação no terminal e arma sessão GUI até gui_automation_disarm ou o utilizador escrever `parar` no prompt. Sem isto, gui_* falham. Fora de Windows devolve erro. Parâmetros: {}."
            .into(),
        handler: gui_automation::gui_automation_arm,
    });
    registry.register(Tool {
        name: "gui_automation_disarm".into(),
        description: "Revoga sessão GUI (remove .toolmind/gui_session.json). Parâmetros: {}."
            .into(),
        handler: gui_automation::gui_automation_disarm,
    });
    registry.register(Tool {
        name: "gui_automation_status".into(),
        description: "Estado da sessão GUI: armed, armed_at_ms. Parâmetros: {}."
            .into(),
        handler: gui_automation::gui_automation_status,
    });
    registry.register(Tool {
        name: "gui_screen_snapshot".into(),
        description: "Windows + sessão armada: captura PNG (primeiro monitor), caminho, dimensões, título e rect da janela em foco, textual_summary (sem OCR no host). Parâmetros: {}."
            .into(),
        handler: gui_automation::gui_screen_snapshot,
    });
    registry.register(Tool {
        name: "gui_mouse_move".into(),
        description: "Windows + sessão armada: move o cursor (x,y inteiros, coordenadas absolutas no virtual desktop). Parâmetros: x, y."
            .into(),
        handler: gui_automation::gui_mouse_move,
    });
    registry.register(Tool {
        name: "gui_mouse_click".into(),
        description: "Windows + sessão armada: clique na posição atual. Parâmetros: button left|right|middle (default left); double bool opcional."
            .into(),
        handler: gui_automation::gui_mouse_click,
    });
    registry.register(Tool {
        name: "gui_type_text".into(),
        description: "Windows + sessão armada: envia texto Unicode (UTF-16) para a janela em foco via SendInput. Parâmetros: text (máx. 8000 bytes). Cuidado com foco errado."
            .into(),
        handler: gui_automation::gui_type_text,
    });
    registry.register(Tool {
        name: "vision_to_context".into(),
        description: "Image→contexto estruturado (Tesseract no PATH): OCR + agrupamento por linha + heurística de tipo (button/input/text). Campos: image_path (relativo, png/jpg/…); mode full|fast|ocr_only (default fast); lang opcional (ex. por+eng). Devolve summary, text[], elements[{type,label,x,y,w,h}], paragraphs (full), page, semantic_tags, llm_block (texto pronto para o modelo)."
            .into(),
        handler: vision_context::vision_to_context,
    });
    registry.register(Tool {
        name: "run_detached".into(),
        description: "Igual run_command (lista branca, argv) mas não espera o fim: devolve pid; stdout/stderr descartados; new_console (bool, Windows) abre console visível."
            .into(),
        handler: run_detached,
    });
    registry.register(Tool {
        name: "terminal_open".into(),
        description: "Abre terminal gráfico no cwd (working_dir relativo). variant: wt_here | powershell_here | cmd_here | powershell_start_fallback | cmd_start_fallback (Windows); macOS usa Terminal; Linux tenta gnome-terminal."
            .into(),
        handler: terminal_open,
    });
    registry.register(Tool {
        name: "websocket_exchange".into(),
        description: "Sessão WebSocket curta: url (ws/wss), send_texts [], receive_max (1–128), timeout_secs (5–120). Handshake + envio + até N mensagens recebidas (texto/binário/ping/pong/close)."
            .into(),
        handler: websocket_exchange,
    });
    registry.register(Tool {
        name: "run_command".into(),
        description: "Executa comando SEM shell: argv = [programa, ...args]; lista branca (cargo, git, gh, curl, ssh, tasklist…); timeout_secs 1–300; max_output_bytes. Para fluxos Git/PR use git_* / gh_*."
            .into(),
        handler: run_command,
    });
    registry.register(Tool {
        name: "git_status".into(),
        description: "git status resumido (-sb --porcelain). {}"
            .into(),
        handler: git_status,
    });
    registry.register(Tool {
        name: "git_diff".into(),
        description: "git diff. Parâmetros: staged (bool), path (opcional, relativo)."
            .into(),
        handler: git_diff,
    });
    registry.register(Tool {
        name: "git_log".into(),
        description: "git log --oneline. Parâmetros: max_count (1–200, default 20)."
            .into(),
        handler: git_log,
    });
    registry.register(Tool {
        name: "git_branch".into(),
        description: "git branch -vv. Parâmetros: mode list|list_all."
            .into(),
        handler: git_branch,
    });
    registry.register(Tool {
        name: "git_remote".into(),
        description: "git remote -v. {}"
            .into(),
        handler: git_remote,
    });
    registry.register(Tool {
        name: "git_add".into(),
        description: "git add -- paths. Parâmetros: paths [relativos, sem ..]."
            .into(),
        handler: git_add,
    });
    registry.register(Tool {
        name: "git_restore".into(),
        description: "git restore [--staged] -- paths. Parâmetros: paths[], staged (bool)."
            .into(),
        handler: git_restore,
    });
    registry.register(Tool {
        name: "git_commit".into(),
        description: "git commit -m. Parâmetros: message (obrigatório), allow_empty (bool)."
            .into(),
        handler: git_commit,
    });
    registry.register(Tool {
        name: "git_push".into(),
        description: "git push. Parâmetros: remote (default origin), branch (opcional), set_upstream (bool)."
            .into(),
        handler: git_push,
    });
    registry.register(Tool {
        name: "git_pull".into(),
        description: "git pull. Parâmetros: remote (default origin), branch (opcional)."
            .into(),
        handler: git_pull,
    });
    registry.register(Tool {
        name: "git_fetch".into(),
        description: "git fetch. Parâmetros: remote (opcional), prune (bool)."
            .into(),
        handler: git_fetch,
    });
    registry.register(Tool {
        name: "git_checkout".into(),
        description: "git checkout. Parâmetros: branch (obrigatório), create_branch (bool), start_point (opcional)."
            .into(),
        handler: git_checkout,
    });
    registry.register(Tool {
        name: "git_show".into(),
        description: "git show. Parâmetros: rev (default HEAD), stat_only (bool, default true)."
            .into(),
        handler: git_show,
    });
    registry.register(Tool {
        name: "git_stash".into(),
        description: "git stash. Parâmetros: action list|push|pop|apply; message (opcional, só push)."
            .into(),
        handler: git_stash,
    });
    registry.register(Tool {
        name: "git_rev_parse".into(),
        description: "git rev-parse. Parâmetros: rev (default HEAD)."
            .into(),
        handler: git_rev_parse,
    });
    registry.register(Tool {
        name: "gh_pr_list".into(),
        description: "Lista PRs (JSON). Parâmetros: state open|closed|merged|all, limit (1–100)."
            .into(),
        handler: gh_pr_list,
    });
    registry.register(Tool {
        name: "gh_pr_view".into(),
        description: "Detalhe de um PR (JSON). Parâmetros: number ou pr."
            .into(),
        handler: gh_pr_view,
    });
    registry.register(Tool {
        name: "gh_pr_diff".into(),
        description: "Diff de um PR. Parâmetros: number ou pr."
            .into(),
        handler: gh_pr_diff,
    });
    registry.register(Tool {
        name: "gh_pr_create".into(),
        description: "Abre PR. Parâmetros: title, body, base (default main), draft (bool), head (opcional)."
            .into(),
        handler: gh_pr_create,
    });
    registry.register(Tool {
        name: "gh_pr_close".into(),
        description: "Fecha PR. Parâmetros: number ou pr."
            .into(),
        handler: gh_pr_close,
    });
    registry.register(Tool {
        name: "gh_pr_merge".into(),
        description: "Mescla PR. Parâmetros: number ou pr, method merge|squash|rebase (default squash)."
            .into(),
        handler: gh_pr_merge,
    });
    registry.register(Tool {
        name: "gh_pr_comment".into(),
        description: "Comentário em PR. Parâmetros: number ou pr, body."
            .into(),
        handler: gh_pr_comment,
    });
    registry.register(Tool {
        name: "ask_user_choice".into(),
        description: "Pergunta interativa obrigatória para decisões: menu com ↑↓ (dialoguer), animação opcional, opção '✎ Outro' automática (texto livre). Campos: question; options (≥1 fixa, ≥2 no total com Outro); subtitle; footer_hint; default_index; include_other (default true); animate_intro (default false); ui_mode dialoguer|simple. Opcional (todas as tools): plan_mark_done_through_step — ver regras globais de plano no system prompt."
            .into(),
        handler: ask_user_choice,
    });
    registry.register(Tool {
        name: "save_plan".into(),
        description: "Plano em .toolmind/current_plan.json e inicializa plan_progress.json (etapas pending). Campos: title, steps; opcional: risks, open_questions, definition_of_done, assumptions, alternatives_rejected, notes."
            .into(),
        handler: save_plan,
    });
    registry.register(Tool {
        name: "load_plan".into(),
        description: "Lê current_plan.json; inclui progress (plan_progress.json) se existir. Parâmetros: {}."
            .into(),
        handler: load_plan,
    });
    registry.register(Tool {
        name: "update_plan_step".into(),
        description: "Atualiza etapa do plano em plan_progress.json: step_index (ou index), status done|in_progress|pending; opcional halt_execution (bool), halt_reason. Com status done, etapas anteriores ainda pending passam automaticamente a done."
            .into(),
        handler: update_plan_step,
    });
    registry.register(Tool {
        name: "reflect_session".into(),
        description: "Auto-crítica: append em .toolmind/self_reflections.jsonl. Parâmetros: critique (obrigatório), improvement_hypothesis, severity (low|medium|high), next_experiments []."
            .into(),
        handler: reflect_session,
    });
    registry.register(Tool {
        name: "create_tool".into(),
        description: "Registra pedido de nova tool em .toolmind/pending_tools.jsonl (nome, description, suggested_impl opcional)."
            .into(),
        handler: create_tool,
    });

    let tools_block = registry.system_tools_block();

    let mut messages = vec![Message {
        role: "system".into(),
        content: format!(
            r#"Você é um agente com ferramentas REAIS no disco e no terminal. Tom **crítico e proativo**: questiona premissas, expõe riscos, propõe alternativas e registra reflexões.

REGRAS DE FERRAMENTA (JSON):
- Quando for usar tool, responda APENAS com um único objeto JSON (sem markdown extra):
{{"tool": "nome_da_tool", "input": {{ ... }}}}
- Em **cadeia** de várias tools no mesmo turno, cada resposta sua deve ser **só** esse JSON (sem frases antes ou depois). Prosa misturada aumenta falhas de parse no host.

MODO INTERATIVO (prioridade alta):
- **Nunca** faça perguntas ao humano só em prosa no chat. Qualquer pergunta, confirmação, priorização ou “qual opção?” → **ask_user_choice** com opções concretas (mín. 2 no total; a tool acrescenta “✎ Outro” para texto livre por padrão).
- Depois de uma resposta livre em “Outro”, continue o fluxo com **nova** ask_user_choice ou avance com save_plan + execução — não desvie para perguntas soltas no texto.
- Ao fechar um raciocínio complexo, considere **reflect_session** (crítica + hipótese de melhoria + próximos experimentos) e **save_plan** com risks / open_questions / definition_of_done quando fizer sentido.

ARQUIVOS E LOOP:
- Depois de write_file ok, não regrave o mesmo arquivo sem pedido explícito; use path_info/read_file para validar.
- Antes de criar arquivos, use workspace_context e/ou path_info.

PLANO E EVOLUÇÃO:
- **save_plan** cria também `plan_progress.json` (etapas `pending`). Use **update_plan_step** para marcar `in_progress` / `done` ao concluir cada parte real do trabalho.
- **Plano e input de tools (genérico)**: qualquer tool pode incluir no `input` o campo opcional **`plan_mark_done_through_step`** (inteiro, índice 0-based **inclusivo** da última etapa a marcar como `done`, mesma escala que `update_plan_step`). Quando o resultado da tool vier com **`status`: `"ok"`**, o host aplica isso em `plan_progress.json` (etapas `0..=valor`). O **modelo** decide o número com base no contexto (prova, deploy, etc.); o código **não** interpreta texto livre do utilizador nem padrões fixos de linguagem.
- **load_plan** devolve `plan` + `progress` quando existir — releia antes de agir se estiver em dúvida do estado.
- **Interpretação de tarefas**: se o usuário pedir “uma questão de cada vez”, “aplicar prova interativa”, “ir perguntando”, etc., isso significa fluxo **sequencial** com várias chamadas a **ask_user_choice** (e opcionalmente write_file só para registro), **não** despejar todas as perguntas num único write_file salvo que ele peça explicitamente “folha única”.
- Alinhe os `steps` do plano ao que será realmente executado (ex.: “Q1 ask_user_choice”, “Q2 ask_user_choice”, …, “correção final”). Não prometa “uma a uma” e execute “tudo de uma vez”.
- Para **parar** a cadeia automática de tools: **update_plan_step** com `halt_execution: true` e `halt_reason` claro; caso contrário o host continua a cadeia até o limite ou até resposta sem tool.
- create_tool para lacunas de capacidade; reflect_session para metacognição.

RESILIÊNCIA DA API: em **429 / 5xx / 408** ou falhas de rede recuperáveis, o host entra em **Reconnecting…**, grava o turno em `.toolmind/frozen_api_turn.json`, aplica **circuit breaker** (backoff exponencial até 120s + extra após 5 falhas seguidas) e **repete o mesmo pedido** sem descartar mensagens nem a cadeia de tools. Após sucesso, o ficheiro é apagado.

Host e terminal: **runtime_host** devolve SO, cwd e **command_playbook** (padrões declarativos: abrir terminal, listar/matar processos, curl, ssh). **run_detached** inicia processo com lista branca sem bloquear (pid). **terminal_open** abre janela de terminal no cwd (variantes por SO). **websocket_exchange** para handshake WS curto + mensagens.
run_command / run_detached: sem shell; só argv e binários permitidos (inclui curl, ssh, tasklist/taskkill no Windows, ps/kill em Unix, etc.).

AUTOMACÃO DO COMPUTADOR (camada de segurança enxuta):
- **computer_survey** antes de planear fluxos que dependem de GUI, scripts ou binários — mapeia o que existe no PATH; **não** instala software sozinho (evite supor que pyautogui, AutoHotkey, etc. existem).
- **open_url** para links (YouTube, docs): determinístico, sem coordenadas de rato.
- **automation_log** após passos relevantes ou antes de ações de risco — trilho em `.toolmind/automation_audit.jsonl` para correlacionar falhas e “como correu”.
- **Sessão GUI (só Windows)**: uma única aprovação no **terminal** ao chamar **gui_automation_arm** (não peça ask_user_choice a cada clique depois disso). Enquanto armada: **gui_screen_snapshot** (PNG + título/rect da janela em foco + resumo textual sem OCR), **gui_mouse_move**, **gui_mouse_click**, **gui_type_text**. O administrador revoga com **gui_automation_disarm** ou escrevendo **`parar`** no prompt do CLI (antes de outra mensagem ao modelo). Fora de Windows estas tools devolvem erro.
- **Checkpoints humanos** (outros domínios): ações destrutivas ou ambíguas → **ask_user_choice**; use **load_plan** / **update_plan_step** quando fizer sentido.
- **Observação global** de teclado/rato (hooks contínuos) **não** existe: use **gui_screen_snapshot** entre passos para “ver” o estado; não prometa sniffer global.
- **vision_to_context**: transforma ficheiro de imagem (caminho relativo) em **JSON + `llm_block`** (resumo, texto OCR, elementos com caixas aproximadas, tags semânticas). Requer **Tesseract** instalado (`computer_survey` deteta `tesseract`). Modos: **fast** (TSV + linhas), **full** (+ parágrafos Tesseract), **ocr_only** (só texto, sem layout). Combine com **gui_screen_snapshot** (guarda PNG) e depois **vision_to_context** nesse path.
- **Paralelização**: vários **run_detached** em paralelo ou **várias instâncias** do CLI em terminais separados; não há orquestrador multi-agente embutido — coordene tarefas com plano explícito e nomes de ficheiros/queues no workspace se precisar.
Git: prefira git_status, git_diff, git_log, git_branch, git_remote, git_add, git_restore, git_commit, git_push, git_pull, git_fetch, git_checkout, git_show, git_stash, git_rev_parse.
GitHub CLI: gh_pr_list, gh_pr_view, gh_pr_diff, gh_pr_create, gh_pr_close, gh_pr_merge, gh_pr_comment (requer `gh` autenticado).

Tools disponíveis:
{tools_block}
"#
        ),
    }];

    warn_frozen_turn_if_present();

    loop {
        print!("{}", "\n🚀 > ".bright_blue());
        io::stdout().flush().unwrap();

        let mut user_input = String::new();
        io::stdin().read_line(&mut user_input)?;
        let user_input = user_input.trim();

        if user_input == "exit" {
            break;
        }

        if user_input.eq_ignore_ascii_case("parar") {
            let out = gui_automation::user_command_disarm();
            println!(
                "{}",
                out.get("note")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Sessão GUI revogada.")
                    .bright_green()
            );
            continue;
        }

        messages.push(Message {
            role: "user".into(),
            content: user_input.into(),
        });

        let mut turn_first_api_call = true;
        let mut last_tool_sig: Option<(String, String)> = None;
        let mut tool_chain_count = 0u32;
        let mut api_backoff_secs: u64 = 2;
        let mut consecutive_api_fails: u32 = 0;

        'tool_turn: loop {
            let header: &'static str = if turn_first_api_call {
                "Analisando seu pedido"
            } else if plan_execution_active_for_spinner() {
                "Plano ativo · próxima ferramenta"
            } else {
                "Continuando após ferramenta"
            };

            let packaged: Option<(
                reqwest::Response,
                Arc<AtomicBool>,
                thread::JoinHandle<()>,
            )> = loop {
                let stop_spin = Arc::new(AtomicBool::new(false));
                let spinner = spawn_thinking_spinner(stop_spin.clone(), header);

                let resp = match client
                    .post("https://integrate.api.nvidia.com/v1/chat/completions")
                    .bearer_auth(api_key.clone())
                    .json(&json!({
                        "model": std::env::var("NVIDIA_MODEL").unwrap_or("z-ai/glm-5.1".to_string()),
                        "messages": messages.iter().map(|m| {
                            json!({"role": m.role, "content": m.content})
                        }).collect::<Vec<_>>(),
                        "stream": true
                    }))
                    .send()
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        stop_spin.store(true, Ordering::Relaxed);
                        let _ = spinner.join();
                        if reqwest_error_retryable(&e) {
                            let _ = save_frozen_api_turn(
                                &messages,
                                tool_chain_count,
                                &last_tool_sig,
                                "network",
                                None,
                                &e.to_string(),
                            );
                            consecutive_api_fails = consecutive_api_fails.saturating_add(1);
                            let sleep_s =
                                calc_backoff_sleep_secs(api_backoff_secs, consecutive_api_fails, None);
                            println!(
                                "\n{} falha de rede · {}ª seguida · pausa {}s · estado em .toolmind/{}",
                                "↻ Reconnecting…".bright_yellow().bold(),
                                consecutive_api_fails,
                                sleep_s,
                                FROZEN_API_TURN_FILE
                            );
                            io::stdout().flush().unwrap();
                            tokio::time::sleep(std::time::Duration::from_secs(sleep_s)).await;
                            api_backoff_secs = (api_backoff_secs.saturating_mul(2)).min(120);
                            continue;
                        }
                        if turn_first_api_call {
                            messages.pop();
                        }
                        clear_frozen_api_turn();
                        return Err(e.into());
                    }
                };

                if !resp.status().is_success() {
                    let code = resp.status().as_u16();
                    let headers = resp.headers().clone();
                    let body = resp.text().await.unwrap_or_default();
                    stop_spin.store(true, Ordering::Relaxed);
                    let _ = spinner.join();
                    if http_status_retryable(code) {
                        let _ = save_frozen_api_turn(
                            &messages,
                            tool_chain_count,
                            &last_tool_sig,
                            "http",
                            Some(code),
                            &body,
                        );
                        consecutive_api_fails = consecutive_api_fails.saturating_add(1);
                        let ra = parse_retry_after_seconds(&headers);
                        let sleep_s =
                            calc_backoff_sleep_secs(api_backoff_secs, consecutive_api_fails, ra);
                        println!(
                            "\n{} HTTP {} · {}ª falha seguida · pausa {}s · circuit breaker · congelado em .toolmind/{}",
                            "↻ Reconnecting…".bright_yellow().bold(),
                            code,
                            consecutive_api_fails,
                            sleep_s,
                            FROZEN_API_TURN_FILE
                        );
                        io::stdout().flush().unwrap();
                        tokio::time::sleep(std::time::Duration::from_secs(sleep_s)).await;
                        api_backoff_secs = (api_backoff_secs.saturating_mul(2)).min(120);
                        continue;
                    }
                    eprintln!(
                        "{} {}",
                        "Erro da API:".red(),
                        body.chars().take(500).collect::<String>()
                    );
                    if turn_first_api_call {
                        messages.pop();
                    }
                    clear_frozen_api_turn();
                    break 'tool_turn;
                }

                consecutive_api_fails = 0;
                api_backoff_secs = 2;
                clear_frozen_api_turn();
                break Some((resp, stop_spin, spinner));
            };

            let (response, stop_spin, spinner) =
                packaged.expect("fluxo HTTP: sucesso grava Some antes do stream");
            let mut spinner_handle: Option<thread::JoinHandle<()>> = Some(spinner);

            turn_first_api_call = false;

            let mut stream = response.bytes_stream();
            let mut full_response = String::new();
            let mut stream_delivered_output = false;
            let mut showed_reasoning_label = false;

            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                let text = String::from_utf8_lossy(&chunk);

                for line in text.lines() {
                    if !line.starts_with("data: ") {
                        continue;
                    }

                    let json_str = &line[6..];
                    if json_str.trim() == "[DONE]" {
                        break;
                    }

                    let parsed: Value = match serde_json::from_str(json_str) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    let Some(delta) = parsed
                        .get("choices")
                        .and_then(|c| c.get(0))
                        .and_then(|ch| ch.get("delta"))
                    else {
                        continue;
                    };

                    let reasoning = delta
                        .get("reasoning_content")
                        .and_then(|v| v.as_str())
                        .or_else(|| delta.get("reasoning").and_then(|v| v.as_str()))
                        .filter(|s| !s.is_empty());
                    let content_opt = delta["content"].as_str().filter(|s| !s.is_empty());

                    if reasoning.is_some() || content_opt.is_some() {
                        if !stream_delivered_output {
                            stop_spin.store(true, Ordering::Relaxed);
                            if let Some(h) = spinner_handle.take() {
                                let _ = h.join();
                            }
                            erase_spinner_line_main();
                            stream_delivered_output = true;
                        }
                    }

                    if let Some(r) = reasoning {
                        if !showed_reasoning_label {
                            println!(
                                "\n {} {}",
                                "◆".truecolor(120, 90, 130),
                                "Raciocínio (pensamento — não é saída final)".truecolor(110, 105, 118)
                            );
                            showed_reasoning_label = true;
                        }
                        if let Some(p) = thinking_json_prefix(r) {
                            print!("{}", style_thinking(p).dimmed());
                        }
                        print!("{}", style_thinking(r));
                        io::stdout().flush().unwrap();
                    }

                    if let Some(content) = content_opt {
                        print!("{}", content);
                        io::stdout().flush().unwrap();
                        full_response.push_str(content);
                    }
                }
            }

            stop_spin.store(true, Ordering::Relaxed);
            if let Some(h) = spinner_handle.take() {
                let _ = h.join();
            }
            erase_spinner_line_main();

            println!();

            if let Some((tool_name, input)) = extract_tool_call(&full_response) {
                tool_chain_count += 1;
                if tool_chain_count > MAX_TOOL_CHAIN_PER_TURN {
                    eprintln!(
                        "{}",
                        format!(
                            "Limite de {} chamadas de ferramentas neste turno; encerrando a cadeia.",
                            MAX_TOOL_CHAIN_PER_TURN
                        )
                        .red()
                    );
                    messages.push(Message {
                        role: "assistant".into(),
                        content: full_response.clone(),
                    });
                    messages.push(Message {
                        role: "user".into(),
                        content: format!(
                            "Sistema: limite de {} ferramentas por mensagem atingido. Responda ao usuário em texto: o que já foi feito e o que falta.",
                            MAX_TOOL_CHAIN_PER_TURN
                        ),
                    });
                    break;
                }

                print_tool_banner(&tool_name);

                let input_sig = serde_json::to_string(&input).unwrap_or_default();
                let sig = (tool_name.clone(), input_sig);
                let plan_input = input.clone();

                let mut result = if last_tool_sig.as_ref() == Some(&sig) {
                    json!({
                        "status": "skipped_duplicate",
                        "tool": tool_name,
                        "message": "Chamada idêntica à anterior: não repita. Se write_file já retornou ok, confirme ao usuário em texto ou use path_info/read_file apenas para verificar."
                    })
                } else if let Some(v) = registry.execute(&tool_name, input) {
                    last_tool_sig = Some(sig);
                    v
                } else {
                    println!(
                        "{}",
                        format!("Tool desconhecida: {}", tool_name).red()
                    );
                    json!({
                        "status": "error",
                        "error": format!("tool '{}' não registrada", tool_name)
                    })
                };

                if let Some(pp) = plan_progress_apply_optional_mark(&plan_input, &result) {
                    result["plan_progress"] = pp;
                }

                println!(
                    "{}",
                    serde_json::to_string_pretty(&result)
                        .unwrap_or_else(|_| result.to_string())
                        .bright_green()
                );
                println!(
                    "{}",
                    " · resultado da ferramenta · "
                        .bright_black()
                        .italic()
                );

                render_plan_execution_panel();

                let mut tool_feedback = format!(
                    "Resultado da tool `{}`:\n{}",
                    tool_name,
                    serde_json::to_string_pretty(&result)
                        .unwrap_or_else(|_| result.to_string())
                );
                if result.get("plan_progress").is_some() {
                    tool_feedback.push_str(
                        "\n\n[Sistema] plan_progress.json foi atualizado via plan_mark_done_through_step no input. Na cadeia de tools, envie só o JSON da próxima chamada, sem prosa na mesma mensagem.",
                    );
                }
                messages.push(Message {
                    role: "user".into(),
                    content: tool_feedback,
                });

                continue;
            }

            messages.push(Message {
                role: "assistant".into(),
                content: full_response,
            });
            break;
        }
    }

    Ok(())
}
