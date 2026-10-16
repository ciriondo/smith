/* smith web UI.
 *
 * The page uses no framework and has no build step. Two rules this file must not break:
 *
 *  1. Model output is untrusted input, and this page holds a token that can control smith.
 *     Render it with textContent, never innerHTML.
 *  2. The token is stored in sessionStorage and sent in an Authorization header. It never
 *     appears in a query string or a cookie, and the URL it arrived in is cleaned
 *     immediately.
 */

const TOKEN_KEY = "smith.token";

function takeToken() {
  const params = new URLSearchParams(location.hash.slice(1));
  const fromUrl = params.get("token");
  if (fromUrl) {
    sessionStorage.setItem(TOKEN_KEY, fromUrl);
    // Strip it: a fragment is not sent to servers, but it should not sit in the address bar
    // or in the browser's history either.
    history.replaceState(null, "", location.pathname);
    return fromUrl;
  }
  return sessionStorage.getItem(TOKEN_KEY);
}

const token = takeToken();
if (!token) {
  document.getElementById("status").textContent =
    "no token: open the URL smith printed when it started";
}

const el = (id) => document.getElementById(id);
const status = el("status");
const transcript = el("transcript");
const sessionIdLabel = el("session-id");

let currentSession = null;
let lastSeq = 0;
let streaming = false;
let reconnectDelay = 500;

async function api(path, options = {}) {
  const headers = { Authorization: `Bearer ${token}` };
  if (options.body) headers["Content-Type"] = "application/json";
  const response = await fetch(path, { ...options, headers });
  const body = await response.json().catch(() => null);
  if (!response.ok) {
    throw new Error((body && body.error) || response.statusText);
  }
  return body;
}

function say(text, className) {
  status.textContent = text;
  status.className = "status " + (className || "");
}

// --- rendering -------------------------------------------------------------

function addMessage(role, text) {
  const item = document.createElement("li");
  item.className = "message " + role;

  const who = document.createElement("span");
  who.className = "who";
  who.textContent = role === "assistant" ? "smith" : "you";

  const body = document.createElement("div");
  body.className = "body";
  if (text) body.appendChild(document.createTextNode(text)); // use textContent, never innerHTML

  item.append(who, body);
  transcript.appendChild(item);
  item.scrollIntoView({ block: "end" });
  return body;
}

function addNote(text) {
  const item = document.createElement("li");
  item.className = "note";
  item.textContent = text;
  transcript.appendChild(item);
  item.scrollIntoView({ block: "end" });
}

function renderTranscript(messages) {
  transcript.replaceChildren();
  for (const message of messages) {
    const text = (message.content || [])
      .filter((block) => block.type === "text")
      .map((block) => block.text)
      .join("");
    const calls = (message.content || []).filter((block) => block.type === "tool_call");
    if (text) addMessage(message.role, text);
    for (const call of calls) addNote(`tool call: ${call.name}`);
  }
}

function setBusy(busy) {
  streaming = busy;
  el("send").disabled = busy;
  el("cancel").disabled = !busy;
}

// --- sessions --------------------------------------------------------------

async function refreshSessions() {
  const { sessions } = await api("/api/sessions");
  const list = el("sessions");
  list.replaceChildren();
  for (const session of sessions) {
    const item = document.createElement("li");
    item.className = session.id === currentSession ? "selected" : "";
    const button = document.createElement("button");
    button.textContent = session.id;
    button.title = session.id;
    if (session.busy) button.textContent = session.id + " •";
    button.onclick = () => selectSession(session.id);
    item.appendChild(button);
    list.appendChild(item);
  }
  return sessions;
}

async function createSession() {
  const { id } = await api("/api/sessions", { method: "POST" });
  await selectSession(id);
}

async function selectSession(id) {
  currentSession = id;
  lastSeq = 0;
  sessionIdLabel.textContent = id;
  await refreshSessions();

  const snapshot = await api(`/api/sessions/${encodeURIComponent(id)}`);
  renderTranscript(snapshot.transcript);
  setBusy(snapshot.busy);
  // The stream starts from seq 0 and replays the session backlog after the transcript above
  // has been rendered.
  lastSeq = 0;
  stream(id);
}

// --- the event stream ------------------------------------------------------

async function stream(id) {
  while (currentSession === id) {
    try {
      await drain(id);
      say("stream ended", "warn");
      return;
    } catch (error) {
      say(`reconnecting (${error.message})`, "warn");
      // Manual reconnect, with backoff, and always re-fetch the transcript first: a gap
      // must never be rendered as a continuous conversation.
      await new Promise((resolve) => setTimeout(resolve, reconnectDelay));
      reconnectDelay = Math.min(reconnectDelay * 2, 5000);
      try {
        const snapshot = await api(`/api/sessions/${encodeURIComponent(id)}`);
        renderTranscript(snapshot.transcript);
      } catch (refreshError) {
        say(`cannot reach smith: ${refreshError.message}`, "error");
      }
    }
  }
}

async function drain(id) {
  const response = await fetch(
    `/api/sessions/${encodeURIComponent(id)}/events?since=${lastSeq}`,
    { headers: { Authorization: `Bearer ${token}` } },
  );
  if (!response.ok) throw new Error(response.statusText);
  say("connected", "ok");
  reconnectDelay = 500;

  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";

  for (;;) {
    const { value, done } = await reader.read();
    if (done) return;
    buffer += decoder.decode(value, { stream: true });

    let boundary;
    while ((boundary = buffer.indexOf("\n\n")) >= 0) {
      const raw = buffer.slice(0, boundary);
      buffer = buffer.slice(boundary + 2);
      const frame = parseFrame(raw);
      if (frame) apply(frame);
    }
  }
}

function parseFrame(raw) {
  let id = null;
  let event = null;
  let data = "";
  for (const line of raw.split("\n")) {
    if (line.startsWith(":")) continue; // ignore keep-alive comments
    const colon = line.indexOf(":");
    const field = colon === -1 ? line : line.slice(0, colon);
    const value = colon === -1 ? "" : line.slice(colon + 1).replace(/^ /, "");
    if (field === "id") id = Number(value);
    else if (field === "event") event = value;
    else if (field === "data") data += value;
  }
  if (!event) return null;
  return { id, event, data: data ? JSON.parse(data) : {} };
}

let openReply = null;

function apply(frame) {
  if (frame.id !== null && frame.id > lastSeq) lastSeq = frame.id;
  const payload = frame.data || {};

  switch (frame.event) {
    case "turn_started":
      setBusy(true);
      openReply = addMessage("assistant", "");
      break;
    case "text_delta":
      if (!openReply) openReply = addMessage("assistant", "");
      openReply.appendChild(document.createTextNode(payload.text));
      openReply.scrollIntoView({ block: "end" });
      break;
    case "reasoning_delta":
      say("thinking…", "ok");
      break;
    case "turn_finished":
      setBusy(false);
      openReply = null;
      if (payload.usage && (payload.usage.input_tokens || payload.usage.output_tokens)) {
        addNote(`${payload.usage.input_tokens} in / ${payload.usage.output_tokens} out tokens`);
      }
      for (const failure of payload.tool_arg_errors || []) {
        addNote(`unparseable arguments for ${failure.tool}: ${failure.message}`);
      }
      say("connected", "ok");
      refreshSessions();
      break;
    case "turn_failed":
      setBusy(false);
      openReply = null;
      addNote(`failed: ${payload.message}`);
      break;
    case "cancelled":
      setBusy(false);
      openReply = null;
      addNote("cancelled");
      refreshSessions();
      break;
    case "send_rejected":
      addNote("A turn is already running. That message was not sent.");
      break;
    case "resync":
      // Events were dropped. The transcript is the source of truth.
      say("resyncing…", "warn");
      api(`/api/sessions/${encodeURIComponent(currentSession)}`)
        .then((snapshot) => renderTranscript(snapshot.transcript))
        .catch(() => {});
      break;
    default:
      break;
  }
}

// --- controls --------------------------------------------------------------

el("new-session").onclick = () => createSession().catch((e) => say(e.message, "error"));

el("cancel").onclick = () => {
  const id = currentSession;
  api(`/api/sessions/${encodeURIComponent(id)}/cancel`, { method: "POST" }).catch((e) =>
    say(e.message, "error"),
  );
};

el("composer").onsubmit = async (event) => {
  event.preventDefault();
  const input = el("input");
  const text = input.value.trim();
  if (!text || !currentSession || streaming) return;
  input.value = "";
  addMessage("user", text);
  try {
    await api(`/api/sessions/${encodeURIComponent(currentSession)}/messages`, {
      method: "POST",
      body: JSON.stringify({ text }),
    });
  } catch (error) {
    say(error.message, "error");
    addNote(error.message);
  }
};

el("input").addEventListener("keydown", (event) => {
  if (event.key === "Enter" && !event.shiftKey) {
    event.preventDefault();
    el("composer").requestSubmit();
  }
});

// --- start -----------------------------------------------------------------

refreshSessions()
  .then((sessions) => {
    if (sessions.length) selectSession(sessions[0].id);
    else el("new-session").focus();
  })
  .catch((error) => say(`cannot reach smith: ${error.message}`, "error"));
