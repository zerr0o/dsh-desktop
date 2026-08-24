// DSH Desktop runtime: starts, supervises, and stops a local DeepSeek Harness
// web server, then hands the window over to its UI.

use serde::Deserialize;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Defaults describe this machine's DeepSeekHarness checkout; every field can
/// be overridden through an optional `config.json` placed next to the executable.
#[derive(Debug)]
struct Config {
    workdir: PathBuf,
    node_dir: PathBuf,
    corepack_cmd: String,
    host: String,
    port: u16,
    startup_timeout_secs: u64,
}

fn corepack_default() -> &'static str {
    if cfg!(target_os = "windows") {
        "corepack.cmd"
    } else {
        "corepack"
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            workdir: PathBuf::from(r"E:\Documents\GitHub\DeepseekHarness"),
            node_dir: PathBuf::from(r"E:\Documents\GitHub\DeepseekHarness\.tools\node-win"),
            corepack_cmd: corepack_default().to_string(),
            host: "127.0.0.1".into(),
            port: 3080,
            startup_timeout_secs: 120,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ConfigFile {
    workdir: Option<PathBuf>,
    #[serde(rename = "nodeDir")]
    node_dir: Option<PathBuf>,
    #[serde(rename = "corepackCmd")]
    corepack_cmd: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    #[serde(rename = "startupTimeoutSecs")]
    startup_timeout_secs: Option<u64>,
}

impl Config {
    /// Loads overrides from `config.json` beside the executable when present.
    fn load() -> Config {
        let defaults = Config::default();
        let Some(exe) = std::env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from))
        else {
            return defaults;
        };
        let Ok(raw) = std::fs::read_to_string(exe.join("config.json")) else {
            return defaults;
        };
        let Ok(parsed) = serde_json::from_str::<ConfigFile>(&raw) else {
            eprintln!("config.json is not valid JSON; using built-in defaults");
            return defaults;
        };
        Config {
            workdir: parsed.workdir.unwrap_or(defaults.workdir),
            node_dir: parsed.node_dir.unwrap_or(defaults.node_dir),
            corepack_cmd: parsed.corepack_cmd.unwrap_or(defaults.corepack_cmd),
            host: parsed.host.unwrap_or(defaults.host),
            port: parsed.port.unwrap_or(defaults.port),
            startup_timeout_secs: parsed
                .startup_timeout_secs
                .unwrap_or(defaults.startup_timeout_secs),
        }
    }
}

// ---------------------------------------------------------------------------
// Server supervision
// ---------------------------------------------------------------------------

/// Tracks the server process this app started, if any.
struct ServerState {
    child: Mutex<Option<Child>>,
    spawned_by_app: AtomicBool,
    shutting_down: AtomicBool,
}

impl Default for ServerState {
    fn default() -> Self {
        Self {
            child: Mutex::new(None),
            spawned_by_app: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
        }
    }
}

/// Minimal raw-HTTP probe: true when the local UI answers with a success or
/// redirect status. Avoids pulling an HTTP client crate.
fn server_responds(host: &str, port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect((host, port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let request = format!("GET / HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut head = [0u8; 32];
    match stream.read(&mut head) {
        Ok(n) if n >= 12 => {
            let line = String::from_utf8_lossy(&head[..n]);
            line.starts_with("HTTP/") && (line.contains(" 200") || line.contains(" 30"))
        }
        _ => false,
    }
}

/// Builds the platform launcher that runs `pnpm dsh web` from the harness
/// checkout with the portable Node on PATH.
#[cfg(target_os = "windows")]
fn build_command(cfg: &Config) -> Command {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new("cmd.exe");
    cmd.arg("/c")
        .arg(format!(
            "cd /d {work} && set PATH={node};%PATH% && {corepack} pnpm dsh web --no-open --port {port}",
            work = cfg.workdir.display(),
            node = cfg.node_dir.display(),
            corepack = cfg.corepack_cmd,
            port = cfg.port,
        ))
        .env_remove("FORCE_COLOR")
        .env("NO_COLOR", "1")
        .creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd
}

#[cfg(not(target_os = "windows"))]
fn build_command(cfg: &Config) -> Command {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(format!(
            "cd '{work}' && export PATH='{node}':$PATH && {corepack} pnpm dsh web --no-open --port {port}",
            work = cfg.workdir.display(),
            node = cfg.node_dir.display(),
            corepack = cfg.corepack_cmd,
            port = cfg.port,
        ))
        .env_remove("FORCE_COLOR")
        .env("NO_COLOR", "1")
        .process_group(0);
    cmd
}

/// Streams captured process output lines to the webview as `server-log`.
/// The loop ends on its own when the process dies and its pipes close.
fn stream_output(app: AppHandle, reader: impl Read + Send + 'static) {
    std::thread::spawn(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines().map_while(Result::ok) {
            let _ = app.emit("server-log", line);
        }
    });
}

/// Spawns the harness server and registers it in shared state.
fn spawn_server(app: &AppHandle, cfg: &Config) -> Result<(), String> {
    let mut child = build_command(cfg)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to start server: {e}"))?;

    if let Some(stdout) = child.stdout.take() {
        stream_output(app.clone(), stdout);
    }
    if let Some(stderr) = child.stderr.take() {
        stream_output(app.clone(), stderr);
    }

    let state = app.state::<ServerState>();
    *state.child.lock().unwrap() = Some(child);
    state.spawned_by_app.store(true, Ordering::Relaxed);
    Ok(())
}

/// Kills the whole process tree of a spawned server.
fn kill_tree(pid: u32) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(0x0800_0000)
            .output();
    }
    #[cfg(not(target_os = "windows"))]
    {
        // The child leads its own process group (process_group(0)), so a
        // negative pid signals the entire tree.
        let _ = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "kill -TERM -{pid} 2>/dev/null || kill -TERM {pid} 2>/dev/null"
            ))
            .status();
    }
}

/// Stops a server this app spawned and marks supervision ended.
fn cleanup(state: &State<ServerState>) {
    state.shutting_down.store(true, Ordering::Relaxed);
    if state.spawned_by_app.load(Ordering::Relaxed) {
        let child = state.child.lock().unwrap().take();
        if let Some(mut child) = child {
            let pid = child.id();
            kill_tree(pid);
            let _ = child.kill();
        }
        state.spawned_by_app.store(false, Ordering::Relaxed);
    }
}

/// Command invoked by the status page once its event listeners are ready.
#[tauri::command]
fn boot_server(app: AppHandle) {
    boot_flow(app, Config::load());
}

/// Detects an existing server, spawns one when absent, and polls until the UI
/// answers or the startup timeout elapses. Runs off the main thread.
fn boot_flow(app: AppHandle, cfg: Config) {
    std::thread::spawn(move || {
        let state = app.state::<ServerState>();

        if server_responds(&cfg.host, cfg.port) {
            let _ = app.emit("server-status", "Server already running.");
            let _ = app.emit("server-ready", ());
            return;
        }

        let _ = app.emit("server-status", "Starting dsh web server...");
        if let Err(message) = spawn_server(&app, &cfg) {
            let _ = app.emit("server-error", message);
            return;
        }

        let deadline = Instant::now() + Duration::from_secs(cfg.startup_timeout_secs);
        while Instant::now() < deadline {
            if state.shutting_down.load(Ordering::Relaxed) {
                return;
            }
            if server_responds(&cfg.host, cfg.port) {
                let _ = app.emit("server-ready", ());
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }

        let _ = app.emit(
            "server-error",
            format!(
                "The server did not answer within {} seconds. Check the log below.",
                cfg.startup_timeout_secs
            ),
        );
    });
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run() {
    tauri::Builder::default()
        .manage(ServerState::default())
        .invoke_handler(tauri::generate_handler![boot_server])
        .build(tauri::generate_context!())
        .expect("error while building DSH Desktop")
        .run(|app_handle, event| match event {
            RunEvent::ExitRequested { .. } | RunEvent::Exit => {
                cleanup(&app_handle.state::<ServerState>());
            }
            _ => {}
        });
}
