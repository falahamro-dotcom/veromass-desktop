use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use tauri::{Emitter, Manager};

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
    .post(format!("https://moleculeid-api.onrender.com/api/jobs/{job_id}/commit"))
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .plugin(tauri_plugin_dialog::init())
    .invoke_handler(tauri::generate_handler![
      launch_tool, run_alignment_embedded, commit_job_embedded
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
