const invoke = window.__TAURI__.core.invoke;
const $ = (id) => document.getElementById(id);

function say(text, isError) {
  $("message").textContent = text;
  $("message").className = isError ? "error" : "";
}

async function refresh() {
  try {
    const s = await invoke("status");
    $("state").textContent = s.state;
    $("node").textContent = s.node_id ?? "–";
    $("mesh").textContent = s.mesh_name ?? "–";
    $("peers").textContent = String(s.peers_online);
    $("services").textContent = String(s.services_hosted);
    $("last-error").textContent = s.last_error ?? "–";
  } catch (e) {
    say("status: " + e, true);
  }
}

// Runs a command and shows its outcome; true when it succeeded.
async function run(label, command, args) {
  say(label + "…", false);
  let ok = false;
  try {
    await invoke(command, args);
    say(label + ": done", false);
    ok = true;
  } catch (e) {
    say(label + ": " + e, true);
  }
  refresh();
  return ok;
}

$("start").addEventListener("click", () => run("Start", "service_start"));
$("stop").addEventListener("click", () => run("Stop", "service_stop"));
$("join").addEventListener("click", async () => {
  const invite = $("invite").value.trim();
  const name = $("name").value.trim();
  if (!invite || !name) {
    say("Join: an invite and a name are needed", true);
    return;
  }
  if (await run("Join", "join", { invite, name })) {
    $("invite").value = "";
  }
});

refresh();
setInterval(refresh, 5000);
