use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use tauri::{Emitter, Manager};

// moleculeid-api's live backend. Migrated from Render (onrender.com, 512MB,
// OOM-crashing on large Untargeted job commits) to Cloud Run (2GB) — this
// was the ONE place in the whole stack that hardcoded the old Render URL
// directly in native code, bypassing moleculeid-web's VITE_API_URL entirely.
// Every "Commit failed (502 Bad Gateway)" during that migration traced back
// to this literal still pointing at the decommissioned Render instance.
const MOLECULEID_API_BASE: &str = "https://moleculeid-api-1069861439051.us-central1.run.app";

fn sidecar_relative_path(tool: &str) -> Result<&'static str, String> {
  match tool {
    "aligner" => Ok("VeroMass_Aligner.exe"),
    "processor" => Ok("MoleculeID_Processor.exe"),
    "mgf_extractor" => Ok("MGF_Extractor.exe"),
    "phyto_crossmatcher" => Ok("Phyto_CrossMatcher.exe"),
    "bridge" => Ok("VeroMass_Bridge/VeroMass_Bridge.exe"),
    other => Err(format!("Unknown tool '{other}'")),
  }
}

// Sidecars are bundled as resources ("sidecars/*") in production and read
// straight from src-tauri/sidecars in dev — same dual-path pattern
// veromass-bridge's launcher.py already used for sys.frozen vs dev mode.
fn resolve_sidecar_path(app: &tauri::AppHandle, relative: &str) -> Result<std::path::PathBuf, String> {
  let path = if cfg!(debug_assertions) {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("sidecars").join(relative)
  } else {
    app
      .path()
      .resource_dir()
      .map_err(|e| e.to_string())?
      .join("sidecars")
      .join(relative)
  };

  if !path.exists() {
    return Err(format!("Sidecar not found: {}", path.display()));
  }
  Ok(path)
}

#[tauri::command]
fn launch_tool(app: tauri::AppHandle, tool: String, env_vars: Option<std::collections::HashMap<String, String>>) -> Result<(), String> {
  let relative = sidecar_relative_path(&tool)?;
  let exe_path = resolve_sidecar_path(&app, relative)?;

  let mut cmd = Command::new(&exe_path);
  if let Some(vars) = env_vars {
    for (k, v) in vars {
      cmd.env(k, v);
    }
  }
  // No CREATE_NO_WINDOW here on purpose: these are windowed GUI tools
  // (Tkinter/Streamlit) that need their own visible window. The Bridge
  // spike already found CREATE_NO_WINDOW is redundant/harmful for a
  // --windowed build and only belongs on truly console-hidden spawns.
  cmd.spawn().map_err(|e| format!("Failed to launch {tool}: {e}"))?;
  Ok(())
}

// Embedded Aligner: replaces the earlier desktop-app "process_locally"
// command (which spawned VeroMass_Bridge.exe --scheme-launch, opening the
// aligner's own separate window — same as the browser fallback) with
// running the exact same algorithm headlessly and showing progress
// inside VeroMass Desktop's own UI." VeroMass_Aligner.py's --headless mode
// (additive, does not touch a single line of the actual alignment
// algorithm — see that file's HEADLESS ENTRY POINT section) prints one
// JSON object per line to stdout; this just forwards each line as a Tauri
// event so the frontend never needs to poll or parse process output itself.
#[tauri::command]
fn run_alignment_embedded(
  app: tauri::AppHandle,
  folder: String,
  out_dir: String,
) -> Result<(), String> {
  let exe_path = resolve_sidecar_path(&app, sidecar_relative_path("aligner")?)?;

  let mut child = Command::new(&exe_path)
    .arg("--headless")
    .arg("--folder").arg(&folder)
    .arg("--out").arg(&out_dir)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .map_err(|e| format!("Failed to start alignment: {e}"))?;

  let stdout = child.stdout.take().ok_or("No stdout handle on aligner child process")?;
  let handle = app.clone();
  std::thread::spawn(move || {
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
      // Each line is already a JSON object emitted by --headless — forward
      // as-is under a single event name; the frontend switches on `type`.
      let _ = handle.emit("align-event", line);
    }
    let _ = child.wait();
  });

  Ok(())
}

// Reuses veromass-bridge's proven, tested mapping.py (via VeroMass_Bridge.exe
// --build-payload — a pure, offline, no-auth utility mode, see bridge.py)
// to turn aligned_features.xlsx into the commit body shape, then posts it
// directly with reqwest using the access token the webview's OWN live
// Supabase session already has (passed in from the frontend) — deliberately
// does not go through veromass-bridge's separate browser-mediated login at
// all for this path. The desktop app is already logged in; no reason to
// log in twice.
#[tauri::command]
fn commit_job_embedded(
  app: tauri::AppHandle,
  job_id: String,
  mode: String,
  xlsx_path: String,
  access_token: String,
) -> Result<serde_json::Value, String> {
  let bridge_exe = resolve_sidecar_path(&app, sidecar_relative_path("bridge")?)?;

  let output = Command::new(&bridge_exe)
    .arg("--build-payload").arg(&xlsx_path).arg(&mode)
    .output()
    .map_err(|e| format!("Failed to build commit payload: {e}"))?;

  if !output.status.success() {
    return Err(format!(
      "build-payload failed: {}",
      String::from_utf8_lossy(&output.stderr)
    ));
  }

  let mode_body: serde_json::Value = serde_json::from_slice(&output.stdout)
    .map_err(|e| format!("Could not parse payload JSON: {e}"))?;

  let mut body = serde_json::Map::new();
  body.insert("package_uuid".into(), serde_json::Value::String(uuid::Uuid::new_v4().to_string()));
  if let serde_json::Value::Object(map) = mode_body {
    body.extend(map);
  }

  let client = reqwest::blocking::Client::new();
  let resp = client
    .post(format!("{MOLECULEID_API_BASE}/api/jobs/{job_id}/commit"))
    .bearer_auth(&access_token)
    .json(&serde_json::Value::Object(body))
    .send()
    .map_err(|e| format!("Commit request failed: {e}"))?;

  // Read as text first, not resp.json() directly — an error response (a
  // timeout/proxy error page, an empty body, anything non-JSON) must still
  // produce a useful message instead of "error decoding response body"
  // hiding what actually went wrong. Same fallback api_client.py's
  // _raise_for_detail() already uses on the Python side.
  let status = resp.status();
  let body_text = resp.text().map_err(|e| format!("Could not read response body: {e}"))?;

  if !status.is_success() {
    let detail = serde_json::from_str::<serde_json::Value>(&body_text)
      .ok()
      .and_then(|v| v.get("detail").cloned())
      .map(|v| v.to_string())
      .unwrap_or_else(|| body_text.clone());
    return Err(format!("Commit failed ({status}): {detail}"));
  }

  serde_json::from_str(&body_text)
    .map_err(|e| format!("Commit succeeded ({status}) but response wasn't valid JSON: {e} — body: {body_text}"))
}

// Embedded MGF Extractor: same run-headless-and-stream-events pattern as
// run_alignment_embedded above, for a different sidecar/event name. MGF
// Extractor's own --headless mode (MGF_Extractor.py, additive, mirrors the
// Aligner's own headless convention) prints one JSON object per line.
#[tauri::command]
fn run_mgf_extractor_embedded(
  app: tauri::AppHandle,
  folder: String,
  out_dir: String,
) -> Result<(), String> {
  let exe_path = resolve_sidecar_path(&app, sidecar_relative_path("mgf_extractor")?)?;

  let mut child = Command::new(&exe_path)
    .arg("--headless")
    .arg("--folder").arg(&folder)
    .arg("--out").arg(&out_dir)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .map_err(|e| format!("Failed to start MGF extraction: {e}"))?;

  let stdout = child.stdout.take().ok_or("No stdout handle on mgf_extractor child process")?;
  let handle = app.clone();
  std::thread::spawn(move || {
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
      let _ = handle.emit("mgf-extract-event", line);
    }
    let _ = child.wait();
  });

  Ok(())
}

// Generic (tool-agnostic) job-attach for Toolkit Phase 2 tools that don't
// go through commit_job_embedded's targeted/untargeted xlsx mapping — MGF
// Extractor today, Phyto CrossMatcher/Processor once they get their own
// --headless mode. Uploads ONE local output file to job_artifacts.py's
// GCS-backed storage (upload-session -> direct PUT -> complete, the same
// three-step contract the web frontend would use, just driven from Rust
// since only the desktop process can read the local filesystem), then
// calls the new tool-agnostic /complete endpoint with the caller's result
// summary. Returns the final job row.
#[tauri::command]
fn attach_artifact_and_complete(
  job_id: String,
  file_path: String,
  access_token: String,
  result: serde_json::Value,
) -> Result<serde_json::Value, String> {
  let filename = std::path::Path::new(&file_path)
    .file_name()
    .ok_or("file_path has no filename component")?
    .to_string_lossy()
    .to_string();
  let bytes = std::fs::read(&file_path).map_err(|e| format!("Could not read {file_path}: {e}"))?;

  let client = reqwest::blocking::Client::new();

  // 1) Start a resumable upload session, scoped to this job by the server.
  let session_resp = client
    .post(format!("{MOLECULEID_API_BASE}/api/jobs/{job_id}/artifacts/upload-session"))
    .bearer_auth(&access_token)
    .json(&serde_json::json!({ "filename": filename, "kind": "output" }))
    .send()
    .map_err(|e| format!("Could not start artifact upload session: {e}"))?;
  if !session_resp.status().is_success() {
    return Err(format!(
      "upload-session failed ({}): {}",
      session_resp.status(),
      session_resp.text().unwrap_or_default()
    ));
  }
  let session_json: serde_json::Value = session_resp
    .json()
    .map_err(|e| format!("upload-session response wasn't valid JSON: {e}"))?;
  let upload_url = session_json["upload_url"]
    .as_str()
    .ok_or("upload-session response missing upload_url")?;
  let object_name = session_json["object_name"]
    .as_str()
    .ok_or("upload-session response missing object_name")?
    .to_string();

  // 2) PUT the whole file in one shot (small tool-output files — no chunking
  // needed). A GCS resumable session still requires Content-Range even for
  // a single-shot upload of the complete object.
  let put_resp = client
    .put(upload_url)
    .header("Content-Length", bytes.len().to_string())
    .header("Content-Range", format!("bytes 0-{}/{}", bytes.len().saturating_sub(1), bytes.len()))
    .body(bytes)
    .send()
    .map_err(|e| format!("Artifact upload failed: {e}"))?;
  if !put_resp.status().is_success() {
    return Err(format!(
      "Artifact upload failed ({}): {}",
      put_resp.status(),
      put_resp.text().unwrap_or_default()
    ));
  }

  // 3) Record the finished upload against the job (starts the 90-day TTL).
  let complete_upload_resp = client
    .post(format!("{MOLECULEID_API_BASE}/api/jobs/{job_id}/artifacts/complete"))
    .bearer_auth(&access_token)
    .json(&serde_json::json!({ "kind": "output", "object_name": object_name }))
    .send()
    .map_err(|e| format!("Could not finalize artifact ref: {e}"))?;
  if !complete_upload_resp.status().is_success() {
    return Err(format!(
      "artifacts/complete failed ({}): {}",
      complete_upload_resp.status(),
      complete_upload_resp.text().unwrap_or_default()
    ));
  }

  // 4) Flip the job to ready with the tool's own result summary.
  let complete_resp = client
    .post(format!("{MOLECULEID_API_BASE}/api/jobs/{job_id}/complete"))
    .bearer_auth(&access_token)
    .json(&serde_json::json!({ "result": result }))
    .send()
    .map_err(|e| format!("Could not complete job: {e}"))?;
  let status = complete_resp.status();
  let body_text = complete_resp.text().map_err(|e| format!("Could not read response body: {e}"))?;
  if !status.is_success() {
    return Err(format!("Job completion failed ({status}): {body_text}"));
  }
  serde_json::from_str(&body_text)
    .map_err(|e| format!("Job completed ({status}) but response wasn't valid JSON: {e} — body: {body_text}"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .plugin(tauri_plugin_dialog::init())
    .invoke_handler(tauri::generate_handler![
      launch_tool, run_alignment_embedded, commit_job_embedded,
      run_mgf_extractor_embedded, attach_artifact_and_complete
    ])
    .setup(|app| {
      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }
      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
