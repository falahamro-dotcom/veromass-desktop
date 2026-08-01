use std::process::Command;
use tauri::Manager;

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

// Replaces the old `veromass://job?workbench=...&job=...` OS-scheme dispatch
// (Workbench.jsx used to construct that link and rely on Windows routing it
// to a registered handler). Same downstream logic — VeroMass_Bridge.exe's
// --scheme-launch flag still writes the pending-job hint, launches the
// aligner, and ensures a background watch loop — just invoked directly by
// the shell instead of through the OS's URL-scheme registry.
#[tauri::command]
fn process_locally(app: tauri::AppHandle, workbench_id: String, job_id: String) -> Result<(), String> {
  let exe_path = resolve_sidecar_path(&app, sidecar_relative_path("bridge")?)?;
  let url = format!("veromass://job?workbench={workbench_id}&job={job_id}");
  Command::new(&exe_path)
    .arg("--scheme-launch")
    .arg(url)
    .spawn()
    .map_err(|e| format!("Failed to start Process Locally: {e}"))?;
  Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  tauri::Builder::default()
    .invoke_handler(tauri::generate_handler![launch_tool, process_locally])
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
