//! Sessão de automação GUI (Windows): uma aprovação no host ao armar; até `gui_automation_disarm`
//! ou o comando `parar` no prompt do CLI.

use colored::Colorize;
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TOOLMIND_DIR: &str = ".toolmind";
const GUI_SESSION_FILE: &str = "gui_session.json";
const GUI_SNAPSHOT_DIR: &str = "gui_snapshots";

fn toolmind_dir() -> PathBuf {
    PathBuf::from(TOOLMIND_DIR)
}

fn session_path() -> PathBuf {
    toolmind_dir().join(GUI_SESSION_FILE)
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn ensure_toolmind() -> Result<PathBuf, Value> {
    let p = toolmind_dir();
    fs::create_dir_all(&p).map_err(|e| json!({"status":"error","error": format!("{e}")}))?;
    Ok(p)
}

fn session_armed() -> bool {
    let p = session_path();
    let Ok(s) = fs::read_to_string(&p) else {
        return false;
    };
    serde_json::from_str::<Value>(&s)
        .ok()
        .and_then(|v| v.get("armed").and_then(|x| x.as_bool()))
        == Some(true)
}

#[cfg(not(windows))]
fn not_supported() -> Value {
    json!({
        "status": "error",
        "error": "Automação GUI (rato/teclado/captura) só está implementada no Windows."
    })
}

/// Revoga sessão a partir do prompt do utilizador (`parar`).
pub fn user_command_disarm() -> Value {
    #[cfg(windows)]
    {
        clear_session_file();
        return json!({"status":"ok","disarmed": true,"note":"Sessão GUI revogada pelo administrador (prompt)."});
    }
    #[cfg(not(windows))]
    {
        json!({"status":"ok","disarmed": false,"note":"Sem sessão GUI neste SO."})
    }
}

fn clear_session_file() {
    let _ = fs::remove_file(session_path());
}

#[cfg(windows)]
mod win {
    use super::*;
    use screenshots::Screen;
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
        KEYEVENTF_UNICODE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
        MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEINPUT,
        MOUSE_EVENT_FLAGS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetSystemMetrics, GetWindowRect, GetWindowTextW, IsWindow,
        SetCursorPos, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };

    fn virtual_screen_bounds() -> (i32, i32, i32, i32) {
        unsafe {
            let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            let h = GetSystemMetrics(SM_CYVIRTUALSCREEN);
            (x, y, w, h)
        }
    }

    fn clamp_pos(x: i32, y: i32) -> (i32, i32) {
        let (vx, vy, vw, vh) = virtual_screen_bounds();
        let max_x = vx.saturating_add(vw.saturating_sub(1));
        let max_y = vy.saturating_add(vh.saturating_sub(1));
        let x = x.clamp(vx, max_x);
        let y = y.clamp(vy, max_y);
        (x, y)
    }

    fn foreground_info() -> (String, Option<(i32, i32, i32, i32)>) {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0.is_null() || !IsWindow(Some(hwnd)).as_bool() {
                return (String::new(), None);
            }
            let mut buf = [0u16; 512];
            let n = GetWindowTextW(hwnd, &mut buf);
            let title = if n > 0 {
                String::from_utf16_lossy(&buf[..n as usize])
            } else {
                String::new()
            };
            let mut r = RECT::default();
            let rect = if GetWindowRect(hwnd, &mut r).is_ok() {
                Some((r.left, r.top, r.right, r.bottom))
            } else {
                None
            };
            (title, rect)
        }
    }

    fn send_inputs(inputs: &[INPUT]) -> Result<(), String> {
        let n = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        if n as usize != inputs.len() {
            return Err(format!(
                "SendInput incompleto: {}/{}",
                n,
                inputs.len()
            ));
        }
        Ok(())
    }

    fn mouse_btn_pair(down: MOUSE_EVENT_FLAGS, up: MOUSE_EVENT_FLAGS) -> Result<(), String> {
        let down_in = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: down,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let up_in = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: up,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        send_inputs(&[down_in, up_in])
    }

    pub fn do_mouse_move(x: i32, y: i32) -> Result<(), String> {
        let (x, y) = clamp_pos(x, y);
        unsafe {
            SetCursorPos(x, y).map_err(|e| format!("SetCursorPos: {e}"))?;
        }
        Ok(())
    }

    pub fn do_mouse_click(button: &str, double: bool) -> Result<(), String> {
        let (down, up) = match button {
            "right" => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
            "middle" => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
            _ => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        };
        mouse_btn_pair(down, up)?;
        if double {
            thread::sleep(Duration::from_millis(45));
            mouse_btn_pair(down, up)?;
        }
        Ok(())
    }

    pub fn do_type_text(text: &str) -> Result<(), String> {
        for unit in text.encode_utf16() {
            let down = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(0),
                        wScan: unit,
                        dwFlags: KEYEVENTF_UNICODE,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            let up = INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(0),
                        wScan: unit,
                        dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            send_inputs(&[down, up])?;
        }
        Ok(())
    }

    pub fn snapshot_path_and_meta() -> Result<(PathBuf, u32, u32, String, Option<(i32, i32, i32, i32)>), String> {
        let (title, rect) = foreground_info();
        let screens = Screen::all().map_err(|e| e.to_string())?;
        let screen = screens.first().ok_or_else(|| "nenhum ecrã".to_string())?;
        let img = screen.capture().map_err(|e| e.to_string())?;
        let w = img.width();
        let h = img.height();

        let dir = toolmind_dir().join(GUI_SNAPSHOT_DIR);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let name = format!("snap_{}.png", now_ms());
        let path = dir.join(&name);
        img.save(&path).map_err(|e| e.to_string())?;
        Ok((path, w, h, title, rect))
    }

    /// Resumo textual heurístico (sem OCR): título da janela + geometria + amostra de cor no canto.
    pub fn screen_text_summary(w: u32, h: u32, title: &str, path: &Path) -> String {
        let mut parts = vec![
            format!("Janela em foco: \"{}\"", title.replace('"', "'")),
            format!("Resolução da captura: {}×{}", w, h),
            format!("Ficheiro PNG: {}", path.display()),
        ];
        parts.push(
            "Conteúdo visual detalhado não é inferido pelo host (sem OCR). Use o ficheiro ou um cliente com visão."
                .into(),
        );
        parts.join("\n")
    }
}

pub fn gui_automation_arm(_input: Value) -> Value {
    #[cfg(windows)]
    {
        if session_armed() {
            return json!({
                "status": "ok",
                "already_armed": true,
                "note": "Sessão GUI já estava armada; não é necessário aprovar de novo."
            });
        }
        let _ = std::io::stdout().flush();
        eprintln!(
            "{}",
            "⚠  CONTROLO GUI: rato, teclado e capturas de ecrã até revogar (gui_automation_disarm ou escrever parar no prompt)."
                .bright_red()
        );
        let ok = dialoguer::Confirm::new()
            .with_prompt("Autorizar UMA VEZ o controlo GUI nesta máquina até revogar?")
            .default(false)
            .interact()
            .unwrap_or(false);
        if !ok {
            return json!({"status":"error","error":"Autorização recusada no terminal."});
        }
        if let Err(e) = ensure_toolmind() {
            return e;
        }
        let body = json!({
            "armed": true,
            "armed_at_ms": now_ms(),
        });
        match fs::write(session_path(), serde_json::to_string_pretty(&body).unwrap()) {
            Ok(_) => json!({
                "status": "ok",
                "armed": true,
                "note": "Sessão armada. Use gui_screen_snapshot, gui_mouse_move, gui_mouse_click, gui_type_text. Revogue com gui_automation_disarm ou `parar` no prompt."
            }),
            Err(e) => json!({"status":"error","error": e.to_string()}),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = _input;
        not_supported()
    }
}

pub fn gui_automation_disarm(_input: Value) -> Value {
    #[cfg(windows)]
    {
        let _ = _input;
        let was = session_armed();
        clear_session_file();
        json!({
            "status": "ok",
            "disarmed": true,
            "was_armed": was,
            "note": "Sessão GUI revogada. Novas ações gui_* falham até gui_automation_arm."
        })
    }
    #[cfg(not(windows))]
    {
        let _ = _input;
        not_supported()
    }
}

pub fn gui_automation_status(_input: Value) -> Value {
    #[cfg(windows)]
    {
        let _ = _input;
        let armed = session_armed();
        let armed_at = fs::read_to_string(session_path())
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.get("armed_at_ms").and_then(|x| x.as_u64()));
        json!({
            "status": "ok",
            "armed": armed,
            "armed_at_ms": armed_at,
            "platform": "windows"
        })
    }
    #[cfg(not(windows))]
    {
        let _ = _input;
        json!({"status":"ok","armed": false, "platform": std::env::consts::OS})
    }
}

fn require_armed() -> Result<(), Value> {
    if !session_armed() {
        return Err(json!({
            "status": "error",
            "error": "Sessão GUI não armada. Chame gui_automation_arm primeiro (aprovação única no terminal)."
        }));
    }
    Ok(())
}

pub fn gui_screen_snapshot(_input: Value) -> Value {
    #[cfg(windows)]
    {
        if let Err(e) = require_armed() {
            return e;
        }
        match win::snapshot_path_and_meta() {
            Ok((path, w, h, title, rect)) => {
                let summary = win::screen_text_summary(w, h, &title, &path);
                json!({
                    "status": "ok",
                    "image_path": path.to_string_lossy(),
                    "width": w,
                    "height": h,
                    "foreground_title": title,
                    "foreground_rect": rect,
                    "textual_summary": summary,
                })
            }
            Err(e) => json!({"status":"error","error": e}),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = _input;
        not_supported()
    }
}

pub fn gui_mouse_move(input: Value) -> Value {
    #[cfg(windows)]
    {
        if let Err(e) = require_armed() {
            return e;
        }
        let Some(x) = input.get("x").and_then(|v| v.as_i64()) else {
            return json!({"status":"error","error":"x obrigatório (inteiro, coordenada de ecrã)"});
        };
        let Some(y) = input.get("y").and_then(|v| v.as_i64()) else {
            return json!({"status":"error","error":"y obrigatório (inteiro, coordenada de ecrã)"});
        };
        match win::do_mouse_move(x as i32, y as i32) {
            Ok(()) => json!({"status":"ok","x": x, "y": y, "note": "Cursor movido (coordenadas absolutas no virtual desktop)."}),
            Err(e) => json!({"status":"error","error": e}),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = input;
        not_supported()
    }
}

pub fn gui_mouse_click(input: Value) -> Value {
    #[cfg(windows)]
    {
        if let Err(e) = require_armed() {
            return e;
        }
        let button = input
            .get("button")
            .and_then(|v| v.as_str())
            .unwrap_or("left");
        let button = match button {
            "right" | "middle" | "left" => button,
            _ => {
                return json!({"status":"error","error":"button deve ser left, right ou middle"});
            }
        };
        let double = input.get("double").and_then(|v| v.as_bool()).unwrap_or(false);
        match win::do_mouse_click(button, double) {
            Ok(()) => json!({"status":"ok","button": button, "double": double}),
            Err(e) => json!({"status":"error","error": e}),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = input;
        not_supported()
    }
}

pub fn gui_type_text(input: Value) -> Value {
    #[cfg(windows)]
    {
        if let Err(e) = require_armed() {
            return e;
        }
        let Some(text) = input.get("text").and_then(|v| v.as_str()) else {
            return json!({"status":"error","error":"text obrigatório (UTF-8)"});
        };
        if text.is_empty() {
            return json!({"status":"error","error":"text vazio"});
        }
        if text.len() > 8000 {
            return json!({"status":"error","error":"text demasiado longo (máx. 8000 bytes UTF-8)"});
        }
        match win::do_type_text(text) {
            Ok(()) => json!({"status":"ok","chars_utf16": text.encode_utf16().count(), "note": "Texto enviado como Unicode para a janela em foco."}),
            Err(e) => json!({"status":"error","error": e}),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = input;
        not_supported()
    }
}
