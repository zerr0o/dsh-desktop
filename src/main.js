// DSH Desktop status screen.
// Listens for server lifecycle events from the Rust side; when the DeepSeek
// Harness web UI answers, this page navigates to it.

const statusLine = document.getElementById("status-line");
const logEl = document.getElementById("log");
const spinner = document.getElementById("spinner");
const retryBtn = document.getElementById("retry-btn");

function setStatus(text) {
  statusLine.textContent = text;
}

const NOISE = /^(\(node:\d+\)|\(Use `node --trace-warnings)/;

function appendLog(rawLine) {
  const line = rawLine.replace(/\x1b\[[0-9;]*m/g, "");
  if (NOISE.test(line)) return;
  logEl.hidden = false;
  logEl.textContent += line + "\n";
  logEl.scrollTop = logEl.scrollHeight;
}

function showError(message) {
  spinner.style.display = "none";
  setStatus(message);
  retryBtn.hidden = false;
}

async function boot() {
  const { listen } = window.__TAURI__.event;

  await listen("server-log", (event) => appendLog(String(event.payload ?? "")));
  await listen("server-status", (event) => setStatus(String(event.payload ?? "")));
  await listen("server-ready", () => {
    setStatus("Server is up - opening UI...");
    // Hand the whole window over to the DeepSeek Harness web UI.
    window.location.replace("http://127.0.0.1:3080/");
  });
  await listen("server-error", (event) => showError(String(event.payload ?? "Failed to start server.")));

  retryBtn.addEventListener("click", () => window.location.reload());

  // Start (or re-check) the server only after listeners are registered,
  // so no lifecycle event can fire into the void.
  await window.__TAURI__.core.invoke("boot_server");
}

boot().catch((err) => showError(String(err)));
