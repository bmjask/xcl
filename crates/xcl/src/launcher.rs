//! `xcl run <game folder | exe> [-- args]` – prepares data folders and config, then
//! starts the host process with the game's own command line (argv[0] = game exe).

use std::path::{Path, PathBuf};

use crate::config::{Boot, Config};
use crate::util::wide;

/// Data root: `%XCL_DATA_ROOT%` or `%LOCALAPPDATA%\XCL`.
pub fn data_root() -> PathBuf {
    if let Ok(p) = std::env::var("XCL_DATA_ROOT") {
        return PathBuf::from(p);
    }
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
    PathBuf::from(base).join("XCL")
}

/// Values of `attr` on every `<tag …>` element (so "<Executable" != "<ExecutableList").
fn xml_attrs(xml: &str, tag: &str, attr: &str) -> Vec<String> {
    let open = format!("<{tag}");
    let key = format!("{attr}=\"");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = xml[from..].find(&open).map(|i| from + i) {
        from = i + open.len();
        let Some(next) = xml[from..].chars().next() else { break };
        if !(next.is_whitespace() || next == '/' || next == '>') {
            continue;
        }
        let Some(end) = xml[from..].find('>') else { break };
        let t = &xml[from..from + end];
        // attribute must start at a whitespace boundary ("Name=" but not "DisplayName=")
        let mut pos = 0;
        while let Some(k) = t[pos..].find(&key).map(|k| pos + k) {
            if k == 0 || t[..k].ends_with(char::is_whitespace) {
                let a = k + key.len();
                if let Some(b) = t[a..].find('"') {
                    out.push(t[a..a + b].to_string());
                }
                break;
            }
            pos = k + key.len();
        }
    }
    out
}

fn xml_attr(xml: &str, tag: &str, attr: &str) -> Option<String> {
    xml_attrs(xml, tag, attr).into_iter().next()
}

fn decode_game_config(raw: &[u8]) -> Option<String> {
    let (body, little_endian) = if let Some(body) = raw.strip_prefix(&[0xFF, 0xFE]) {
        (body, true)
    } else if let Some(body) = raw.strip_prefix(&[0xFE, 0xFF]) {
        (body, false)
    } else {
        return String::from_utf8(raw.to_vec()).ok();
    };
    if body.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|b| if little_endian { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) })
        .collect();
    String::from_utf16(&units).ok()
}

/// (executables, title) from MicrosoftGame.config if present.
pub fn read_game_config(root: &Path) -> (Vec<String>, Option<String>) {
    let p = root.join("MicrosoftGame.config");
    let Some(xml) = std::fs::read(p).ok().and_then(|raw| decode_game_config(&raw)) else { return (Vec::new(), None) };
    (xml_attrs(&xml, "Executable", "Name"), xml_attr(&xml, "Identity", "Name"))
}

/// `xcl.exe` itself (by name, path or identical bytes) must never be picked as the game.
fn is_xcl_copy(p: &Path) -> bool {
    if p.file_name().map(|n| n.to_string_lossy().eq_ignore_ascii_case("xcl.exe")).unwrap_or(false) {
        return true;
    }
    let Ok(me) = std::env::current_exe() else { return false };
    if std::fs::canonicalize(p).ok() == std::fs::canonicalize(&me).ok() {
        return true;
    }
    match (std::fs::metadata(p), std::fs::metadata(&me)) {
        (Ok(a), Ok(b)) if a.len() == b.len() => std::fs::read(p).ok() == std::fs::read(&me).ok(),
        _ => false,
    }
}

fn is_exe(p: &Path) -> bool {
    p.is_file() && p.extension().map(|x| x.eq_ignore_ascii_case("exe")).unwrap_or(false)
}

/// Executables in `dir` and its subfolders (up to `depth` levels), excluding XCL itself.
fn find_exes(dir: &Path, depth: u32, skipped_self: &mut bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() && depth > 0 {
            out.extend(find_exes(&p, depth - 1, skipped_self));
        } else if is_exe(&p) {
            if is_xcl_copy(&p) {
                *skipped_self = true;
            } else {
                out.push(p);
            }
        }
    }
    out
}

fn pick_exe(root: &Path) -> Result<PathBuf, String> {
    let (cfg_exes, _) = read_game_config(root);
    if !cfg_exes.is_empty() {
        for e in &cfg_exes {
            let p = root.join(e);
            if is_exe(&p) && !is_xcl_copy(&p) {
                return Ok(p);
            }
        }
        return Err(format!(
            "MicrosoftGame.config names {}, but there is no such file in {}",
            cfg_exes.join(", "),
            root.display()
        ));
    }
    let mut skipped_self = false;
    let rel = |p: &PathBuf| p.strip_prefix(root).unwrap_or(p).display().to_string();
    let top = find_exes(root, 0, &mut skipped_self);
    let candidates = if top.is_empty() { find_exes(root, 2, &mut skipped_self) } else { top };
    match candidates.len() {
        1 => Ok(candidates.into_iter().next().unwrap()),
        0 => Err(format!(
            "no game .exe file in {}{}",
            root.display(),
            if skipped_self { " (only xcl.exe – that is XCL itself; do not copy it into the game folder)" } else { "" }
        )),
        _ => Err(format!(
            "several .exe files ({}) – point to the game exe directly or set 'exe' in xcl.toml",
            candidates.iter().map(rel).collect::<Vec<_>>().join(", ")
        )),
    }
}

fn sanitize(s: &str) -> String {
    let t: String = s.chars().map(|c| if c.is_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
    if t.is_empty() {
        "title".into()
    } else {
        t
    }
}

pub fn resolve_target(target: &Path) -> Result<(PathBuf, PathBuf, String), String> {
    let target = std::fs::canonicalize(target).map_err(|e| format!("{}: {e}", target.display()))?;
    let target = PathBuf::from(target.to_string_lossy().trim_start_matches("\\\\?\\"));
    let (root, exe) = if target.is_dir() {
        (target.clone(), pick_exe(&target)?)
    } else {
        if is_xcl_copy(&target) {
            return Err(format!("{} is XCL itself, not a game – give the game exe or folder", target.display()));
        }
        (target.parent().ok_or("no parent folder")?.to_path_buf(), target.clone())
    };
    let (_, identity) = read_game_config(&root);
    let title = sanitize(&identity.unwrap_or_else(|| root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
    Ok((root, exe, title))
}

fn quote(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"']) {
        return a.to_string();
    }
    let mut s = String::from("\"");
    let mut bs = 0;
    for c in a.chars() {
        match c {
            '\\' => bs += 1,
            '"' => {
                s.push_str(&"\\".repeat(bs * 2 + 1));
                s.push('"');
                bs = 0;
            }
            _ => {
                s.push_str(&"\\".repeat(bs));
                bs = 0;
                s.push(c);
            }
        }
    }
    s.push_str(&"\\".repeat(bs * 2));
    s.push('"');
    s
}

pub fn run(target: &Path, args: &[String]) -> i32 {
    let (root, exe, title) = match resolve_target(target) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("xcl: {e}");
            return 2;
        }
    };
    let data_dir = data_root().join(&title);
    for d in ["logs", "cache", "overlay", "saves", "pls", "temp"] {
        let _ = std::fs::create_dir_all(data_dir.join(d));
    }
    let mut config = match Config::load_or_create(&data_dir.join("xcl.toml")) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("xcl: error in xcl.toml: {e}");
            return 2;
        }
    };
    let exe = if !config.exe.is_empty() && root.join(&config.exe).is_file() { root.join(&config.exe) } else { exe };
    config.exe = exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let manifest = crate::manifest::load(&root);
    let from_manifest = config.resolve(manifest.as_ref());
    crate::tls::cleanup_cache(&data_dir.join("cache"));
    let boot = Boot { title: title.clone(), exe_path: exe.clone(), game_root: root.clone(), data_dir: data_dir.clone(), config };
    let boot_path = data_dir.join("cache").join("boot.json");
    if let Err(e) = std::fs::write(&boot_path, serde_json::to_string_pretty(&boot).unwrap()) {
        eprintln!("xcl: writing {}: {e}", boot_path.display());
        return 2;
    }
    let _ = std::fs::remove_file(boot.report_path());

    println!("XCL {} – starting '{title}'", env!("CARGO_PKG_VERSION"));
    println!("  exe:   {}", exe.display());
    println!("  data:  {}", data_dir.display());
    if !from_manifest.is_empty() {
        println!("  MicrosoftGame.Config: {}", from_manifest.join(", "));
    }
    match crate::inspect::console_imports(&exe) {
        Some(c) if c.is_empty() => println!(
            "  WARNING: this file imports no console library (kernelx / xgameruntime / d3d12_xs) –\n           it is most likely a regular Windows program, not an Xbox Series game."
        ),
        Some(c) => println!("  console: imports {}", c.join(", ")),
        None => println!("  WARNING: this is not a valid x64 PE file (encrypted? from an XVC package?)"),
    }

    let mut cmdline = quote(&exe.to_string_lossy());
    for a in args {
        cmdline.push(' ');
        cmdline.push_str(&quote(a));
    }
    let code = match spawn_host(&cmdline, &root, &boot_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("xcl: failed to start the game process: {e}");
            return 2;
        }
    };
    summarize(&boot, code);
    code as i32
}

fn spawn_host(cmdline: &str, cwd: &Path, boot_path: &Path) -> Result<u32, String> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, GetExitCodeProcess, WaitForSingleObject, INFINITE, PROCESS_INFORMATION, STARTUPINFOW,
    };
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    std::env::set_var("XCL_HOST", boot_path);
    let app = wide(&me.to_string_lossy());
    let mut cmd = wide(cmdline);
    let dir = wide(&cwd.to_string_lossy());
    unsafe {
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessW(
            app.as_ptr(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            0,
            std::ptr::null(),
            dir.as_ptr(),
            &si,
            &mut pi,
        );
        std::env::remove_var("XCL_HOST");
        if ok == 0 {
            return Err(format!("CreateProcessW: error {}", GetLastError()));
        }
        WaitForSingleObject(pi.hProcess, INFINITE);
        let mut code = 0u32;
        GetExitCodeProcess(pi.hProcess, &mut code);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        Ok(code)
    }
}

fn summarize(boot: &Boot, code: u32) {
    let path = boot.report_path();
    let mut report = crate::report::load(&path);
    if let Some(r) = report.as_mut() {
        r.exit_code = Some(code as i64);
        let _ = std::fs::write(&path, serde_json::to_string_pretty(r).unwrap_or_default());
    }
    println!();
    println!("Game exited with code {code:#x} ({code})");
    let Some(r) = report else {
        println!("No report – see the log: {}", boot.logs_dir().join("xcl.log").display());
        return;
    };
    if let Some(c) = &r.crash {
        println!("CRASH: {c}");
    }
    let total: usize = r.imports_by_source.values().sum();
    println!("Imports: {total} ({})", r.imports_by_source.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join(", "));
    if !r.called_stubs.is_empty() {
        println!("Unimplemented functions called ({}):", r.called_stubs.len());
        for s in r.called_stubs.iter().take(15) {
            println!("  {:>8}×  {}!{}   (first call: {})", s.calls, s.dll, s.function, s.first_caller);
        }
    }
    if !r.vfs.unmapped_paths.is_empty() {
        println!("Unmapped console paths: {}", r.vfs.unmapped_paths.len());
        for p in r.vfs.unmapped_paths.iter().take(5) {
            println!("  {p}");
        }
    }
    if !r.vfs.redirected_writes.is_empty() {
        println!("Writes to game files redirected to the overlay: {}", r.vfs.redirected_writes.len());
    }
    println!("Report: {}", path.display());
    println!("Log:    {}", boot.logs_dir().join("xcl.log").display());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microsoft_game_config_parsing() {
        let xml = r#"<?xml version="1.0"?><Game configVersion="1"><Identity Name="Contoso.Game" Publisher="CN=X" Version="1.0.0.0"/>
            <ExecutableList><Executable Name="game_scarlett.exe" TargetDeviceFamily="Scarlett" Id="Game"/></ExecutableList></Game>"#;
        assert_eq!(xml_attr(xml, "Identity", "Name").as_deref(), Some("Contoso.Game"));
        assert_eq!(xml_attr(xml, "Executable", "Name").as_deref(), Some("game_scarlett.exe"));
    }

    #[test]
    fn several_executables_in_config() {
        let xml = r#"<ExecutableList><Executable Name="Bin\game.exe" Id="Game"/><Executable Name="tool.exe" Id="T"/></ExecutableList>
            <ShellVisuals DefaultDisplayName="X" Name="no"/>"#;
        assert_eq!(xml_attrs(xml, "Executable", "Name"), vec!["Bin\\game.exe".to_string(), "tool.exe".to_string()]);
        assert_eq!(xml_attr(xml, "ShellVisuals", "Name").as_deref(), Some("no"), "DisplayName= must not match Name=");
    }

    #[test]
    fn game_config_accepts_bom_marked_utf16_and_rejects_malformed_text() {
        let dir = TempDir::new("utf16_config");
        let xml = r#"<Game><Identity Name="Contoso.Test"/><Executable Name="Bin\game.exe"/></Game>"#;
        for (bom, bytes) in [
            ([0xFF, 0xFE], xml.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>()),
            ([0xFE, 0xFF], xml.encode_utf16().flat_map(u16::to_be_bytes).collect::<Vec<_>>()),
        ] {
            let mut raw = bom.to_vec();
            raw.extend(bytes);
            let decoded = decode_game_config(&raw).unwrap();
            assert_eq!(xml_attr(&decoded, "Executable", "Name").as_deref(), Some(r"Bin\game.exe"));
            assert_eq!(xml_attr(&decoded, "Identity", "Name").as_deref(), Some("Contoso.Test"));
            std::fs::write(dir.0.join("MicrosoftGame.config"), &raw).unwrap();
            assert_eq!(read_game_config(&dir.0), (vec![r"Bin\game.exe".into()], Some("Contoso.Test".into())));
        }
        assert!(decode_game_config(&[0xFF, 0xFE, b'<']).is_none());
        assert!(decode_game_config(&[0xFF, 0xFE, 0x00, 0xD8]).is_none());
        assert!(decode_game_config(&[0xFF, 0xFF]).is_none());
        std::fs::write(dir.0.join("MicrosoftGame.config"), [0xFF, 0xFE, b'<']).unwrap();
        assert_eq!(read_game_config(&dir.0), (Vec::new(), None));
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let p = std::env::temp_dir().join(format!("xcl_launcher_{tag}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn never_picks_xcl_itself() {
        let d = TempDir::new("self");
        let me = std::env::current_exe().unwrap();
        std::fs::copy(&me, d.0.join("xcl.exe")).unwrap();
        std::fs::copy(&me, d.0.join("renamed_copy.exe")).unwrap(); // byte-identical copy of XCL
        let e = resolve_target(&d.0).unwrap_err();
        assert!(e.contains("xcl.exe") && e.contains("XCL itself"), "{e}");
        assert!(resolve_target(&d.0.join("xcl.exe")).unwrap_err().contains("XCL itself"));
        // a real game exe next to it wins
        std::fs::write(d.0.join("game.exe"), b"MZ not really a game").unwrap();
        let (_, exe, _) = resolve_target(&d.0).unwrap();
        assert_eq!(exe.file_name().unwrap(), "game.exe");
    }

    #[test]
    fn finds_exe_in_subfolder_and_via_config() {
        let d = TempDir::new("sub");
        std::fs::create_dir_all(d.0.join("Bin")).unwrap();
        std::fs::write(d.0.join("Bin").join("game.exe"), b"MZ").unwrap();
        let (_, exe, _) = resolve_target(&d.0).unwrap();
        assert!(exe.ends_with("Bin\\game.exe"), "{exe:?}");
        std::fs::write(
            d.0.join("MicrosoftGame.config"),
            r#"<Game><Identity Name="Contoso.Test"/><ExecutableList><Executable Name="Bin\missing.exe"/></ExecutableList></Game>"#,
        )
        .unwrap();
        let e = resolve_target(&d.0).unwrap_err();
        assert!(e.contains("missing.exe"), "{e}");
    }

    #[test]
    fn console_detection() {
        // a plain Windows binary imports no console DLLs
        let me = std::env::current_exe().unwrap();
        assert_eq!(crate::inspect::console_imports(&me), Some(vec![]));
        let sys = std::path::PathBuf::from(std::env::var("SystemRoot").unwrap_or("C:\\Windows".into())).join("System32\\notepad.exe");
        assert_eq!(crate::inspect::console_imports(&sys), Some(vec![]));
        let d = TempDir::new("pe");
        std::fs::write(d.0.join("junk.exe"), b"not a PE").unwrap();
        assert_eq!(crate::inspect::console_imports(&d.0.join("junk.exe")), None);
    }

    #[test]
    fn command_line_quoting() {
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("C:\\Games\\My Game\\a.exe"), "\"C:\\Games\\My Game\\a.exe\"");
        assert_eq!(quote("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(quote(""), "\"\"");
    }
}
