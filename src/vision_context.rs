//! Image → contexto estruturado para LLM (OCR via Tesseract CLI + heurísticas de layout).
//! Requer `tesseract` no PATH (instalação do utilizador).

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const MAX_IMAGE_BYTES: u64 = 25 * 1024 * 1024;

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

fn allowed_image_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "tif" | "tiff" | "bmp" | "webp"
            )
        })
        .unwrap_or(false)
}

fn sanitize_lang(raw: Option<&str>) -> String {
    let d = raw.unwrap_or("por+eng");
    let s: String = d
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '_')
        .take(48)
        .collect();
    if s.is_empty() {
        "por+eng".into()
    } else {
        s
    }
}

fn run_tesseract_tsv(abs_image: &Path, lang: &str) -> Result<String, String> {
    let out = Command::new("tesseract")
        .arg(abs_image.as_os_str())
        .arg("stdout")
        .args(["-l", lang])
        .arg("tsv")
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "tesseract não encontrado no PATH. Instale Tesseract OCR e reinicie o terminal.".into()
            } else {
                format!("falha ao executar tesseract: {e}")
            }
        })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("tesseract (tsv) falhou: {err}"));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("stdout inválido UTF-8: {e}"))
}

fn run_tesseract_txt(abs_image: &Path, lang: &str) -> Result<String, String> {
    let out = Command::new("tesseract")
        .arg(abs_image.as_os_str())
        .arg("stdout")
        .args(["-l", lang])
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "tesseract não encontrado no PATH.".into()
            } else {
                format!("falha ao executar tesseract: {e}")
            }
        })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("tesseract (txt) falhou: {err}"));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("stdout inválido UTF-8: {e}"))
}

#[derive(Clone, Debug)]
struct WordBox {
    page: i32,
    block: i32,
    par: i32,
    line: i32,
    left: i32,
    top: i32,
    width: i32,
    height: i32,
    conf: f64,
    text: String,
}

fn parse_tsv_words(tsv: &str) -> (Option<(i32, i32)>, Vec<WordBox>, Vec<String>, Vec<String>) {
    let mut page_size: Option<(i32, i32)> = None;
    let mut words: Vec<WordBox> = Vec::new();
    let mut paragraphs: Vec<String> = Vec::new();
    let mut ocr_lines: Vec<String> = Vec::new();

    for line in tsv.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 12 {
            continue;
        }
        let level: i32 = parts[0].parse().unwrap_or(-1);
        if level == 1 {
            let w: i32 = parts[8].parse().unwrap_or(0);
            let h: i32 = parts[9].parse().unwrap_or(0);
            if w > 0 && h > 0 {
                page_size = Some((w, h));
            }
            continue;
        }
        if level == 3 {
            let t = parts[11..].join("\t").trim().to_string();
            if !t.is_empty() {
                paragraphs.push(t);
            }
            continue;
        }
        if level == 4 {
            let t = parts[11..].join("\t").trim().to_string();
            if !t.is_empty() {
                ocr_lines.push(t);
            }
            continue;
        }
        if level != 5 {
            continue;
        }

        let page: i32 = parts[1].parse().unwrap_or(0);
        let block: i32 = parts[2].parse().unwrap_or(0);
        let par: i32 = parts[3].parse().unwrap_or(0);
        let line: i32 = parts[4].parse().unwrap_or(0);
        let left: i32 = parts[6].parse().unwrap_or(0);
        let top: i32 = parts[7].parse().unwrap_or(0);
        let width: i32 = parts[8].parse().unwrap_or(0);
        let height: i32 = parts[9].parse().unwrap_or(0);
        let conf: f64 = parts[10].parse().unwrap_or(-1.0);
        let text = parts[11..].join("\t").trim().to_string();
        if text.is_empty() {
            continue;
        }
        words.push(WordBox {
            page,
            block,
            par,
            line,
            left,
            top,
            width,
            height,
            conf,
            text,
        });
    }

    (page_size, words, paragraphs, ocr_lines)
}

fn merge_line_key(w: &WordBox) -> (i32, i32, i32, i32) {
    (w.page, w.block, w.par, w.line)
}

fn merge_words_to_elements(words: &[WordBox]) -> Vec<Value> {
    let mut groups: BTreeMap<(i32, i32, i32, i32), Vec<&WordBox>> = BTreeMap::new();
    for w in words {
        groups.entry(merge_line_key(w)).or_default().push(w);
    }

    let mut out: Vec<Value> = Vec::new();
    for ((_p, _b, _par, _ln), mut ws) in groups {
        ws.sort_by_key(|w| w.left);
        let merged_text = ws
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string();
        if merged_text.is_empty() {
            continue;
        }
        let min_l = ws.iter().map(|w| w.left).min().unwrap_or(0);
        let min_t = ws.iter().map(|w| w.top).min().unwrap_or(0);
        let max_r = ws
            .iter()
            .map(|w| w.left.saturating_add(w.width))
            .max()
            .unwrap_or(0);
        let max_b = ws
            .iter()
            .map(|w| w.top.saturating_add(w.height))
            .max()
            .unwrap_or(0);
        let confs: Vec<f64> = ws.iter().map(|w| w.conf).filter(|c| *c >= 0.0).collect();
        let conf_avg = if confs.is_empty() {
            None
        } else {
            Some(confs.iter().sum::<f64>() / confs.len() as f64)
        };

        let low = merged_text.to_lowercase();
        let element_type = if low.contains("entrar")
            || low.contains("login")
            || low.contains("submit")
            || low.contains("regist")
            || low == "ok"
            || low.contains("continuar")
        {
            "button"
        } else if low.contains("email")
            || low.contains("senha")
            || low.contains("password")
            || low.contains("utilizador")
            || low.contains("username")
            || low.contains("@")
        {
            "input"
        } else if merged_text.len() > 80 {
            "text_block"
        } else {
            "text"
        };

        let mut el = json!({
            "type": element_type,
            "label": merged_text,
            "x": min_l,
            "y": min_t,
            "w": max_r.saturating_sub(min_l),
            "h": max_b.saturating_sub(min_t),
        });
        if let Some(c) = conf_avg {
            el["conf_avg"] = json!(c);
        }
        out.push(el);
    }
    out.sort_by(|a, b| {
        let ya = a["y"].as_i64().unwrap_or(0);
        let yb = b["y"].as_i64().unwrap_or(0);
        ya.cmp(&yb).then_with(|| {
            let xa = a["x"].as_i64().unwrap_or(0);
            let xb = b["x"].as_i64().unwrap_or(0);
            xa.cmp(&xb)
        })
    });
    out
}

fn classify_summary(text_lines: &[String], elements: &[Value], full: bool) -> (String, Vec<String>) {
    let blob = text_lines.join(" ").to_lowercase();
    let mut tags: Vec<String> = Vec::new();
    if blob.contains("http") || blob.contains("www.") {
        tags.push("browser".into());
    }
    let auth = blob.contains("senha")
        || blob.contains("password")
        || blob.contains("email")
        || blob.contains("entrar")
        || blob.contains("login");
    if auth {
        tags.push("auth_form".into());
    }
    if blob.contains("youtube") {
        tags.push("youtube".into());
    }

    let summary = if auth {
        "Provável ecrã de autenticação ou formulário (campos de credenciais / ação)."
            .into()
    } else if blob.contains("http") || blob.contains("www.") {
        "Conteúdo com hiperligações ou navegação web (OCR)."
            .into()
    } else if full {
        format!(
            "Imagem com {} linhas de texto e {} elementos agrupados (OCR + heurística).",
            text_lines.len(),
            elements.len()
        )
    } else {
        format!(
            "Imagem analisada: {} linhas, {} elementos (modo rápido).",
            text_lines.len(),
            elements.len()
        )
    };

    (summary, tags)
}

fn build_llm_block(
    summary: &str,
    text_lines: &[String],
    elements: &[Value],
    paragraphs: &[String],
    full: bool,
) -> String {
    let mut s = String::new();
    s.push_str("Resumo:\n");
    s.push_str(summary);
    s.push_str("\n\nTexto detectado (linhas):\n");
    for t in text_lines {
        if !t.trim().is_empty() {
            s.push_str("- ");
            s.push_str(t);
            s.push('\n');
        }
    }
    if full && !paragraphs.is_empty() {
        s.push_str("\nParágrafos (OCR):\n");
        for p in paragraphs {
            s.push_str("- ");
            s.push_str(p);
            s.push('\n');
        }
    }
    s.push_str("\nElementos (coordenadas do canto sup. esquerdo, pixels no bitmap OCR):\n");
    for (i, e) in elements.iter().enumerate() {
        let ty = e["type"].as_str().unwrap_or("?");
        let label = e["label"].as_str().unwrap_or("");
        let x = e["x"].as_i64().unwrap_or(0);
        let y = e["y"].as_i64().unwrap_or(0);
        let w = e["w"].as_i64().unwrap_or(0);
        let h = e["h"].as_i64().unwrap_or(0);
        s.push_str(&format!(
            "[{}] {}: \"{}\" (x={}, y={}, w={}, h={})\n",
            i + 1,
            ty,
            label.replace('"', "'"),
            x,
            y,
            w,
            h
        ));
    }
    s.push_str("\nAções possíveis (exemplos):\n");
    s.push_str("- gui_mouse_move + gui_mouse_click num botão listado\n");
    s.push_str("- gui_type_text após focar o campo certo\n");
    s
}

/// Input: `image_path` (relativo, sem ..), `mode` full|fast|ocr_only, opcional `lang` (ex. por+eng).
pub fn vision_to_context(input: Value) -> Value {
    let Some(rel_s) = input.get("image_path").and_then(|v| v.as_str()) else {
        return json!({"status":"error","error":"image_path obrigatório (caminho relativo a um ficheiro de imagem)"});
    };
    let Some(rel) = safe_relative_path(rel_s) else {
        return json!({"status":"error","error":"image_path inválido (sem .., não absoluto)"});
    };
    if !allowed_image_ext(&rel) {
        return json!({"status":"error","error":"extensão não suportada (use png, jpg, webp, tiff, bmp)"});
    }

    let mode = input
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("fast");
    let mode = match mode {
        "full" | "fast" | "ocr_only" => mode,
        _ => {
            return json!({"status":"error","error":"mode deve ser full, fast ou ocr_only"});
        }
    };
    let full = mode == "full";
    let ocr_only = mode == "ocr_only";
    let lang = sanitize_lang(input.get("lang").and_then(|v| v.as_str()));

    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(e) => return json!({"status":"error","error": e.to_string()}),
    };
    let abs = cwd.join(&rel);
    let canon = match fs::canonicalize(&abs) {
        Ok(p) => p,
        Err(e) => return json!({"status":"error","error": format!("ficheiro inexistente ou inacessível: {e}")}),
    };

    let meta = match fs::metadata(&canon) {
        Ok(m) => m,
        Err(e) => return json!({"status":"error","error": e.to_string()}),
    };
    if !meta.is_file() {
        return json!({"status":"error","error":"caminho não é ficheiro"});
    }
    if meta.len() > MAX_IMAGE_BYTES {
        return json!({"status":"error","error":"imagem demasiado grande (máx. 25 MiB)"});
    }

    if ocr_only {
        let txt = match run_tesseract_txt(&canon, &lang) {
            Ok(t) => t,
            Err(e) => return json!({"status":"error","error": e}),
        };
        let text_lines: Vec<String> = txt
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        let (summary, tags) = classify_summary(&text_lines, &[], full);
        let llm = build_llm_block(&summary, &text_lines, &[], &[], full);
        return json!({
            "status": "ok",
            "mode": mode,
            "image_path": rel.to_string_lossy(),
            "lang": lang,
            "summary": summary,
            "semantic_tags": tags,
            "text": text_lines,
            "elements": [],
            "paragraphs": [],
            "page": Value::Null,
            "llm_block": llm,
            "note": "ocr_only: sem caixas; use fast/full para layout aproximado via TSV."
        });
    }

    let tsv = match run_tesseract_tsv(&canon, &lang) {
        Ok(t) => t,
        Err(e) => return json!({"status":"error","error": e}),
    };

    let (page_size, words, paragraphs, ocr_lines) = parse_tsv_words(&tsv);
    let elements = merge_words_to_elements(&words);

    let text_lines: Vec<String> = if !ocr_lines.is_empty() {
        ocr_lines
    } else {
        elements
            .iter()
            .filter_map(|e| e.get("label").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect()
    };

    let (summary, tags) = classify_summary(&text_lines, &elements, full);

    let paras_out: Vec<String> = if full {
        paragraphs
    } else {
        Vec::new()
    };

    let page_json = match page_size {
        Some((w, h)) => json!({"width": w, "height": h}),
        None => Value::Null,
    };

    let llm = build_llm_block(&summary, &text_lines, &elements, &paras_out, full);

    json!({
        "status": "ok",
        "mode": mode,
        "image_path": rel.to_string_lossy(),
        "lang": lang,
        "summary": summary,
        "semantic_tags": tags,
        "text": text_lines,
        "elements": elements,
        "paragraphs": paras_out,
        "page": page_json,
        "llm_block": llm,
        "note": "Coordenadas vêm do OCR (TSV); alinhar com gui_screen_snapshot/gui_mouse_* no mesmo referencial da imagem analisada."
    })
}
