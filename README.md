# VeroMass Desktop

Tauri shell wrapping the real `moleculeid-web` app (same codebase as
app.veromass.com — login, Library, Workbench, everything) plus five local
processing tools launched as sidecar processes: VeroMass Aligner, MoleculeID
Processor, VOLTA, MGF Extractor, Phyto CrossMatcher.

## Dev setup

Sidecar binaries are NOT committed to this repo (450MB+ of prebuilt exes —
fetched/built from their own source repos instead):

```
sidecars/VeroMass_Aligner.exe        <- veromass-aligner/dist/VeroMass_Aligner.exe
sidecars/VOLTA.exe                   <- excalibar/dist/VOLTA.exe
sidecars/MoleculeID_Processor.exe    <- build via PyInstaller from MoleculeID_Processor/
sidecars/MGF_Extractor.exe           <- build via PyInstaller from mgf-extractor/
sidecars/Phyto_CrossMatcher.exe      <- build via PyInstaller from phyto-crossmatcher/
sidecars/VeroMass_Bridge/            <- veromass-bridge/dist/VeroMass_Bridge/ (whole onedir folder)
```

Place `src-tauri/sidecars/` populated with the above before `cargo tauri dev`
or `cargo tauri build`.

```bash
cd ../moleculeid-web && npm run dev     # frontend dev server (localhost:5173)
cargo tauri dev                          # in this repo
```

## Build

```bash
cd ../moleculeid-web && npm run build    # frontendDist = ../../moleculeid-web/dist
cargo tauri build                        # produces MSI + NSIS installers
```

Installers are unsigned — no code-signing certificate configured yet, so
Windows SmartScreen will warn on first run.

## Architecture notes

- `moleculeid-web`'s `Workbench.jsx` detects `window.__TAURI__` to switch
  between calling the `process_locally` Tauri command (this app) and the old
  `veromass://` OS-scheme link (plain browser at app.veromass.com, which
  must keep working standalone).
- `process_locally` reuses `VeroMass_Bridge.exe --scheme-launch` as-is
  rather than reimplementing auth/watch/commit — lower risk, same proven
  logic from the veromass-bridge project, just invoked by direct process
  spawn instead of OS URL-scheme dispatch.
- `launch_tool` spawns any of the other four sidecars directly, surfaced via
  the "Desktop Tools" menu (desktop-app-only, hidden in the plain browser).
