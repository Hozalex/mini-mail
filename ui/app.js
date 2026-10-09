const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);

const state = {
  folders: [],
  folder: null,
  // A non-empty string means the list shows search results for the folder
  query: "",
  messages: [],
  hasMore: false,
  selected: -1,
  // The open message, its id in the list, and the message being written
  message: null,
  openId: null,
  draft: null,
  // Files attached to the draft
  files: [],
  // Index of the list message currently being dragged onto a folder
  dragIndex: null,
  // Race guard: a reply to a stale request must not repaint the screen
  listToken: 0,
  messageToken: 0,
};

// The only script allowed to run inside a message: it reports link clicks to the app.
// It is allowed by hash, so any edit to its text means updating FRAME_SCRIPT_HASH
// and the same hash in script-src in src-tauri/tauri.conf.json.
const FRAME_SCRIPT = "(function(){function L(e){var t=e.target;return t&&t.closest?t.closest(\"a[href]\"):null}function S(m){parent.postMessage(m,\"*\")}document.addEventListener(\"click\",function(e){var a=L(e);if(!a||a.getAttribute(\"href\").charAt(0)===\"#\")return;e.preventDefault();S({mail:\"link\",href:a.href})},true);document.addEventListener(\"mouseover\",function(e){var a=L(e);if(a)S({mail:\"hover\",href:a.href})});document.addEventListener(\"mouseout\",function(e){if(L(e))S({mail:\"hover\",href:\"\"})});document.addEventListener(\"keydown\",function(e){if(!e.metaKey&&!e.ctrlKey&&!e.altKey)S({mail:\"key\",code:e.code})})})();";
const FRAME_SCRIPT_HASH = "sha256-U4Jl6S2N4JLWECZKPdHFc72K42gbT26dbjQD+2efl3g=";

// Remote images and foreign scripts are blocked in messages: protection from trackers and hostile markup
const BODY_HEAD =
  '<meta charset="utf-8">' +
  `<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src data:; style-src 'unsafe-inline'; script-src '${FRAME_SCRIPT_HASH}'">` +
  `<script>${FRAME_SCRIPT}</script>` +
  "<style>body{font:14px/1.45 -apple-system,sans-serif;margin:16px;color:#111;background:#fff;overflow-wrap:anywhere}" +
  "img{max-width:100%;height:auto}pre{white-space:pre-wrap;font:inherit}" +
  // In dark mode the message frame is inverted as a whole (style.css); images are inverted back
  "@media (prefers-color-scheme:dark){img,video{filter:invert(1) hue-rotate(180deg)}}</style>";

let statusTimer;

function showStatus(text, isError = false, sticky = false) {
  const el = $("status");
  clearTimeout(statusTimer);
  el.textContent = text;
  el.classList.toggle("error", isError);
  el.classList.add("visible");
  if (!sticky) statusTimer = setTimeout(hideStatus, isError ? 8000 : 1500);
}

function hideStatus() {
  $("status").classList.remove("visible");
}

function showError(error) {
  showStatus(String(error), true);
}

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function formatDate(iso, full = false) {
  if (!iso) return "";
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return iso;
  if (full) return date.toLocaleString("en-GB", { dateStyle: "long", timeStyle: "short" });
  const now = new Date();
  if (date.toDateString() === now.toDateString()) {
    return date.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" });
  }
  const options = { day: "numeric", month: "short" };
  if (date.getFullYear() !== now.getFullYear()) options.year = "numeric";
  return date.toLocaleDateString("en-GB", options);
}

function formatSize(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function formatAddresses(list) {
  return list
    .map((a) => (a.name && a.email && a.name !== a.email ? `${a.name} <${a.email}>` : a.name || a.email))
    .join(", ");
}

/* Sign-in */

function showLogin(account, error) {
  $("app").hidden = true;
  $("login").hidden = false;
  if (account) {
    $("login-url").value = account.url;
    $("login-username").value = account.username;
  }
  $("login-password").value = "";
  $("login-error").hidden = !error;
  $("login-error").textContent = error || "";
  (account ? $("login-password") : $("login-url")).focus();
}

$("login").addEventListener("submit", async (event) => {
  event.preventDefault();
  const button = $("login-submit");
  button.disabled = true;
  button.textContent = "Signing in…";
  $("login-error").hidden = true;
  try {
    await invoke("login", {
      url: $("login-url").value,
      username: $("login-username").value,
      password: $("login-password").value,
    });
    $("login-password").value = "";
    await showMain();
  } catch (error) {
    $("login-error").textContent = String(error);
    $("login-error").hidden = false;
  } finally {
    button.disabled = false;
    button.textContent = "Sign in";
  }
});

$("logout").addEventListener("click", async () => {
  await invoke("logout");
  signedOut();
  showLogin({ url: $("login-url").value, username: $("login-username").value });
});

function signedOut() {
  state.draft = null;
  state.folders = [];
  state.folder = null;
  state.messages = [];
  clearReader();
}

// The core stopped checking mail because the server no longer accepts the password
window.__TAURI__.event?.listen("auth:failed", () => {
  signedOut();
  showLogin(
    { url: $("login-url").value, username: $("login-username").value },
    "The server no longer accepts the password. If you changed it, enter the new one.",
  );
});

/* Folders */

async function showMain() {
  $("login").hidden = true;
  $("app").hidden = false;
  await loadFolders();
}

async function loadFolders() {
  try {
    state.folders = await invoke("list_folders");
  } catch (error) {
    showError(error);
    return;
  }
  const current = state.folder && state.folders.find((f) => f.id === state.folder.id);
  state.folder = current || state.folders.find((f) => f.kind === "inbox") || state.folders[0] || null;
  renderFolders();
  if (state.folder) await loadMessages(true);
}

function renderFolders() {
  const inbox = state.folders.find((f) => f.kind === "inbox");
  invoke("set_badge", { count: inbox ? inbox.unread : 0 }).catch(() => {});
  const list = $("folders");
  list.replaceChildren(
    ...state.folders.map((folder) => {
      const item = el("li");
      item.dataset.id = folder.id;
      item.style.paddingLeft = `${8 + folder.depth * 14}px`;
      item.classList.toggle("selected", state.folder?.id === folder.id);
      item.classList.toggle("has-unread", folder.unread > 0);
      item.append(el("span", "name", folder.name));
      // Drafts show the total count; every other folder shows unread
      const count = folder.kind === "drafts" ? folder.total : folder.unread;
      if (count > 0) item.append(el("span", "count", String(count)));
      item.addEventListener("click", () => selectFolder(folder));
      // A folder accepts a dragged message
      item.addEventListener("dragover", (event) => {
        if (state.dragIndex === null || folder.id === state.folder.id) return;
        event.preventDefault();
        item.classList.add("drop-target");
      });
      item.addEventListener("dragleave", () => item.classList.remove("drop-target"));
      item.addEventListener("drop", (event) => {
        event.preventDefault();
        item.classList.remove("drop-target");
        if (state.dragIndex !== null) moveMessage(state.dragIndex, folder);
      });
      return item;
    }),
  );
}

async function selectFolder(folder) {
  state.folder = folder;
  state.query = "";
  $("search").value = "";
  renderFolders();
  clearReader();
  await loadMessages(true);
}

/* Right-click menu in the folder column */

function showContextMenu(event, entries) {
  event.preventDefault();
  const menu = $("context-menu");
  menu.replaceChildren(
    ...entries.map(([label, action]) => {
      // An entry without an action is a caption above a group
      if (!action) return el("li", "label", label);
      const item = el("li", "", label);
      item.setAttribute("role", "menuitem");
      item.addEventListener("click", () => {
        hideContextMenu();
        action();
      });
      return item;
    }),
  );
  menu.hidden = false;
  // Keep the menu inside the window
  const { width, height } = menu.getBoundingClientRect();
  menu.style.left = `${Math.min(event.clientX, window.innerWidth - width - 4)}px`;
  menu.style.top = `${Math.min(event.clientY, window.innerHeight - height - 4)}px`;
}

function hideContextMenu() {
  $("context-menu").hidden = true;
}

document.addEventListener("click", hideContextMenu);
window.addEventListener("blur", hideContextMenu);
// The browser's own menu (Reload and so on) has no place in the app; text fields keep it
document.addEventListener("contextmenu", (event) => {
  if (!event.target.closest("input, textarea")) event.preventDefault();
});

$("folders-pane").addEventListener("contextmenu", (event) => {
  const row = event.target.closest("#folders li[data-id]");
  const folder = row && state.folders.find((f) => f.id === row.dataset.id);
  const entries = [];
  if (folder && folder.unread > 0) entries.push(["Mark all as read", () => markAllRead(folder)]);
  if (folder) entries.push([`New folder in "${folder.name}"`, () => startNewFolder(folder)]);
  entries.push([folder ? "New top-level folder" : "New folder", () => startNewFolder(null)]);
  showContextMenu(event, entries);
});

async function reloadFolders() {
  state.folders = await invoke("list_folders");
  if (state.folder) state.folder = state.folders.find((f) => f.id === state.folder.id) || state.folder;
  renderFolders();
}

async function markAllRead(folder) {
  try {
    await invoke("mark_all_read", { folderId: folder.id });
    if (state.folder?.id === folder.id) {
      state.messages.forEach((message) => (message.isRead = true));
      renderMessages();
    }
    await reloadFolders();
    showStatus(`All messages in "${folder.name}" marked as read`);
  } catch (error) {
    showError(error);
  }
}

/* Moving messages: right-click a message or drag it onto a folder */

async function moveMessage(index, folder) {
  const summary = state.messages[index];
  if (!summary || folder.id === state.folder.id) return;
  try {
    await invoke("move_message", { id: summary.id, folderId: folder.id });
  } catch (error) {
    return showError(error);
  }
  // The list may have refreshed while the request ran, so look the message up again
  const current = state.messages.findIndex((m) => m.id === summary.id);
  if (current !== -1) removeMessage(current, `Moved to "${folder.name}"`);
  reloadFolders().catch(() => {});
}

$("messages").addEventListener("contextmenu", (event) => {
  const row = event.target.closest("li[data-index]");
  if (!row) return;
  const index = Number(row.dataset.index);
  const targets = state.folders.filter((f) => f.id !== state.folder.id && f.kind !== "drafts");
  showContextMenu(event, [
    ["Move to folder:"],
    ...targets.map((folder) => ["\u00a0\u00a0".repeat(folder.depth) + folder.name, () => moveMessage(index, folder)]),
  ]);
});

/* New folder: the name field appears right in the tree; Enter creates, Esc cancels */

function startNewFolder(parent) {
  $("folders").querySelector("li.editing")?.remove();
  const row = el("li", "editing");
  row.style.paddingLeft = `${8 + (parent ? parent.depth + 1 : 0) * 14}px`;
  const input = el("input");
  input.type = "text";
  input.placeholder = "Folder name";
  row.append(input);
  const parentRow = parent && $("folders").querySelector(`li[data-id="${CSS.escape(parent.id)}"]`);
  if (parentRow) parentRow.after(row);
  else $("folders").append(row);
  input.focus();

  let finished = false;
  const finish = async (create) => {
    if (finished) return;
    finished = true;
    const name = input.value.trim();
    row.remove();
    if (!create || !name) return;
    try {
      await invoke("create_folder", { parentId: parent ? parent.id : null, name });
      await reloadFolders();
      showStatus(`Folder "${name}" created`);
    } catch (error) {
      showError(error);
    }
  };
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter") finish(true);
    else if (event.key === "Escape") finish(false);
  });
  input.addEventListener("blur", () => finish(false));
}

/* Message list */

// Message order: by date, or unread on top. The choice is remembered
function unreadFirst() {
  return $("sort").value === "unread";
}

try {
  $("sort").value = localStorage.getItem("sort") || "date";
} catch {}

$("sort").addEventListener("change", () => {
  try {
    localStorage.setItem("sort", $("sort").value);
  } catch {}
  if (!state.folder) return;
  clearReader();
  loadMessages(true);
});

async function loadMessages(reset) {
  const folder = state.folder;
  const token = ++state.listToken;
  if (reset) {
    state.messages = [];
    state.selected = -1;
    $("list-name").textContent = folder.name;
    $("messages").replaceChildren(el("li", "empty", state.query ? "Searching…" : "Loading…"));
  }
  let page;
  try {
    page = await invoke("list_messages", {
      folderId: folder.id,
      offset: state.messages.length,
      query: state.query,
      unreadFirst: unreadFirst(),
    });
  } catch (error) {
    if (token === state.listToken) showError(error);
    return;
  }
  if (token !== state.listToken) return;
  state.messages.push(...page.items);
  state.hasMore = !page.last && page.items.length > 0;
  renderMessages();
}

function renderMessages() {
  const list = $("messages");
  const showRecipient = state.folder.kind === "sent" || state.folder.kind === "drafts";
  const rows = state.messages.map((message, index) => {
    const item = el("li");
    item.classList.toggle("unread", !message.isRead);
    item.classList.toggle("selected", index === state.selected);
    const top = el("div", "row");
    const who = showRecipient ? message.to : message.from;
    top.append(el("span", "who", who || "(no recipient)"), el("span", "date", formatDate(message.date)));
    item.append(
      top,
      el("div", "subject", (message.isMeeting ? "📅 " : "") + (message.hasAttachments ? "📎 " : "") + (message.subject || "(no subject)")),
      el("div", "preview", message.preview),
    );
    item.addEventListener("click", () => openMessage(index));
    item.dataset.index = index;
    item.draggable = true;
    item.addEventListener("dragstart", (event) => {
      state.dragIndex = index;
      event.dataTransfer.effectAllowed = "move";
      event.dataTransfer.setData("text/plain", message.subject);
    });
    item.addEventListener("dragend", () => (state.dragIndex = null));
    return item;
  });
  if (rows.length === 0) rows.push(el("li", "empty", state.query ? "Nothing found" : "This folder is empty"));
  if (state.hasMore) {
    const more = el("li", "more", "Show more");
    more.addEventListener("click", () => {
      more.textContent = "Loading…";
      loadMessages(false);
    });
    rows.push(more);
  }
  list.replaceChildren(...rows);
}

/* Reading a message */

// The right column shows one thing: the message being written, the open message, or a placeholder.
// A message in progress wins: moving between folders and messages does not close it.
function syncReader() {
  const composing = Boolean(state.draft);
  $("compose").hidden = !composing;
  $("reader-content").hidden = composing || !state.message;
  $("reader-empty").hidden = composing || Boolean(state.message);
}

function clearReader() {
  state.messageToken++;
  state.message = null;
  state.openId = null;
  $("msg-body").srcdoc = "";
  syncReader();
}

async function openMessage(index) {
  const summary = state.messages[index];
  if (!summary) return;
  state.selected = index;
  const list = $("messages");
  [...list.children].forEach((row, i) => row.classList.toggle("selected", i === index));
  list.children[index]?.scrollIntoView({ block: "nearest" });

  const token = ++state.messageToken;
  let message;
  try {
    message = await invoke("get_message", { id: summary.id });
  } catch (error) {
    if (token === state.messageToken) showError(error);
    return;
  }
  if (token !== state.messageToken) return;
  state.message = message;
  state.openId = summary.id;
  renderMessage(message);

  if (!summary.isRead) {
    summary.isRead = true;
    list.children[index]?.classList.remove("unread");
    if (state.folder.unread > 0) state.folder.unread--;
    renderFolders();
    invoke("mark_read", { id: message.id, changeKey: message.changeKey, read: true }).catch(showError);
  }
}

function renderMessage(message) {
  syncReader();
  $("msg-subject").textContent = message.subject || "(no subject)";
  $("msg-from").textContent = message.from ? formatAddresses([message.from]) : "";
  $("msg-to").textContent = formatAddresses(message.to);
  $("msg-cc").textContent = formatAddresses(message.cc);
  $("msg-cc").hidden = $("msg-cc-label").hidden = message.cc.length === 0;
  $("msg-date").textContent = formatDate(message.date, true);
  renderMeeting(message.meeting);
  // From Deleted Items a message could only be erased for good, which the app never does
  $("delete").hidden = state.folder.kind === "trash";
  $("msg-attachments").replaceChildren(
    ...message.attachments
      .filter((a) => !a.inline)
      .map((a) => {
        const item = el("li", "", `${a.name} · ${formatSize(a.size)}`);
        item.title = "Save to Downloads";
        item.addEventListener("click", () => downloadAttachment(a));
        return item;
      }),
  );
  const body = message.isHtml ? message.body : `<pre>${escapeHtml(message.body)}</pre>`;
  $("msg-body").srcdoc = BODY_HEAD + body;
}

/* Removes a message from the list after the server deleted or moved it */
function removeMessage(index, statusText) {
  if (index < 0) return showStatus(statusText);
  const wasOpen = index === state.selected;
  state.messages.splice(index, 1);
  if (state.folder.total > 0) state.folder.total--;
  if (wasOpen) {
    state.selected = -1;
    clearReader();
  } else if (index < state.selected) {
    state.selected--;
  }
  renderFolders();
  renderMessages();
  showStatus(statusText);
  // The next message takes the removed one's place; open it so mail can be worked through in a row
  if (wasOpen && state.messages.length > 0) openMessage(Math.min(index, state.messages.length - 1));
}

// An action applies to the message shown on screen, not to the row selected in the list:
// while the next message is loading these are different messages
function removeMessageById(id, statusText) {
  removeMessage(state.messages.findIndex((m) => m.id === id), statusText);
}

async function deleteCurrentMessage() {
  const id = state.openId;
  if (!state.message || !id) return;
  const button = $("delete");
  button.disabled = true;
  try {
    await invoke("delete_message", { id });
  } catch (error) {
    showError(error);
    return;
  } finally {
    button.disabled = false;
  }
  removeMessageById(id, "Moved to Deleted Items");
}

$("delete").addEventListener("click", deleteCurrentMessage);

/* Meeting invitations */

const MEETING_RESPONSES = {
  Accept: "You accepted this invitation.",
  Tentative: "You replied tentatively.",
  Decline: "You declined this invitation.",
};

const MEETING_DONE = {
  accept: "Accepted — the meeting is in your calendar",
  tentative: "Replied tentatively — the meeting is in your calendar",
  decline: "Invitation declined",
};

function formatMeetingTime(meeting) {
  const start = new Date(meeting.start);
  const end = new Date(meeting.end);
  if (Number.isNaN(start.getTime())) return "Time not specified";
  const day = (date) => date.toLocaleDateString("en-GB", { weekday: "short", day: "numeric", month: "long", year: "numeric" });
  const time = (date) => date.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" });
  if (meeting.allDay) return `${day(start)}, all day`;
  if (Number.isNaN(end.getTime())) return `${day(start)}, ${time(start)}`;
  if (start.toDateString() === end.toDateString()) return `${day(start)}, ${time(start)} – ${time(end)}`;
  return `${day(start)}, ${time(start)} – ${day(end)}, ${time(end)}`;
}

function renderMeeting(meeting) {
  $("msg-meeting").hidden = !meeting;
  if (!meeting) return;
  $("meeting-when").textContent = `📅 ${formatMeetingTime(meeting)}`;
  $("meeting-where").textContent = [
    meeting.location && `Where: ${meeting.location}`,
    meeting.organizer && `Organizer: ${meeting.organizer}`,
  ]
    .filter(Boolean)
    .join(" · ");
  let status = MEETING_RESPONSES[meeting.response] || "";
  if (!meeting.isRequest) status = "The organizer cancelled this meeting.";
  else if (meeting.outOfDate) status = "This invitation is out of date: the organizer sent a newer one.";
  $("meeting-status").textContent = status;
  $("meeting-actions").hidden = !meeting.isRequest || meeting.outOfDate;
}

$("meeting-actions").addEventListener("click", async (event) => {
  const response = event.target.dataset?.response;
  const id = state.openId;
  if (!response || !state.message || !id) return;
  const buttons = [...$("meeting-actions").children];
  buttons.forEach((button) => (button.disabled = true));
  try {
    await invoke("respond_to_meeting", { id, response });
  } catch (error) {
    showError(error);
    return;
  } finally {
    buttons.forEach((button) => (button.disabled = false));
  }
  // After a response the server removes the invitation from the Inbox itself
  removeMessageById(id, MEETING_DONE[response]);
});

/* New mail: the core checks the Inbox once a minute and sends this event */

async function onNewMail() {
  try {
    await reloadFolders();
  } catch {
    return;
  }
  // The list is refreshed in place only when that will not get in the way: the Inbox is open,
  // there is no search, and the user has not loaded older messages
  const untouched = state.folder?.kind === "inbox" && !state.query && state.messages.length <= 50;
  if (!untouched) return;
  const token = ++state.listToken;
  let page;
  try {
    page = await invoke("list_messages", {
      folderId: state.folder.id,
      offset: 0,
      query: "",
      unreadFirst: unreadFirst(),
    });
  } catch {
    return;
  }
  if (token !== state.listToken) return;
  const openId = state.messages[state.selected]?.id;
  state.messages = page.items;
  state.hasMore = !page.last && page.items.length > 0;
  state.selected = openId ? state.messages.findIndex((m) => m.id === openId) : -1;
  renderMessages();
}

window.__TAURI__.event?.listen("mail:new", onNewMail);

async function downloadAttachment(attachment) {
  showStatus(`Downloading: ${attachment.name}…`, false, true);
  try {
    const savedAs = await invoke("download_attachment", { id: attachment.id });
    showStatus(`Saved to Downloads: ${savedAs}`);
  } catch (error) {
    showError(error);
  }
}

/* Search in the current folder: Enter searches, an empty field or Esc restores the normal list */

function applySearch(query) {
  query = query.trim();
  if (query === state.query || !state.folder) return;
  state.query = query;
  clearReader();
  loadMessages(true);
}

$("search").addEventListener("keydown", (event) => {
  if (event.key === "Enter") {
    applySearch($("search").value);
  } else if (event.key === "Escape") {
    $("search").value = "";
    applySearch("");
    $("search").blur();
  }
});

$("search").addEventListener("input", () => {
  if ($("search").value === "") applySearch("");
});

function escapeHtml(text) {
  return text.replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" })[c]);
}

/* Links in a message. The message frame is isolated from the app (its own origin, no access
   to app data or commands); only our FRAME_SCRIPT reports clicks */

window.addEventListener("message", (event) => {
  if (event.source !== $("msg-body").contentWindow || !event.data) return;
  const { mail, href = "", code } = event.data;
  const isWeb = /^https?:\/\//i.test(href);
  if (mail === "link" && isWeb) {
    invoke("open_link", { url: href }).catch(showError);
  } else if (mail === "link" && /^mailto:/i.test(href)) {
    const address = href.slice(7).split("?")[0];
    try {
      openCompose("new", { to: decodeURIComponent(address) });
    } catch {
      openCompose("new", { to: address });
    }
  } else if (mail === "hover") {
    // The real link address is visible before the click, as protection from spoofed link text
    if (isWeb) showStatus(href, false, true);
    else hideStatus();
  } else if (mail === "key" && /^Key[JKRN]$/.test(code)) {
    // Arrow keys stay with the message for scrolling; letter shortcuts work from inside it too
    document.dispatchEvent(new KeyboardEvent("keydown", { code }));
  }
});

/* Compose, reply, forward */

const COMPOSE_TITLES = { new: "New message", reply: "Reply", replyAll: "Reply all", forward: "Forward" };

function withPrefix(prefix, pattern, subject) {
  return pattern.test(subject) ? subject : `${prefix}: ${subject}`;
}

function openCompose(mode, preset = {}) {
  const message = state.message;
  let to = preset.to || "";
  let cc = "";
  let subject = "";
  if (mode !== "new" && message) {
    const sender = message.from?.email || "";
    if (mode === "reply") to = sender;
    if (mode === "replyAll") {
      to = sender;
      const skip = new Set([sender.toLowerCase(), message.receivedBy.toLowerCase()]);
      const others = [...message.to, ...message.cc].map((a) => a.email).filter((email) => {
        const key = email.toLowerCase();
        if (!email || skip.has(key)) return false;
        skip.add(key);
        return true;
      });
      cc = others.join(", ");
    }
    subject =
      mode === "forward"
        ? withPrefix("FW", /^(fw|fwd)\s*:/i, message.subject)
        : withPrefix("RE", /^re\s*:/i, message.subject);
  }
  const signature = getSignature();
  const initialBody = signature ? `\n\n${signature}` : "";
  state.draft = { mode, referenceId: mode === "new" ? null : message?.id, initialBody };
  $("compose-title").textContent = COMPOSE_TITLES[mode];
  $("compose-to").value = to;
  $("compose-cc").value = cc;
  $("compose-subject").value = subject;
  // The signature sits below blank lines for the text; in a reply the server adds the quote after it
  $("compose-body").value = initialBody;
  $("compose-body").setSelectionRange(0, 0);
  state.files = [];
  renderFiles();
  $("compose-hint").textContent =
    mode === "new" ? "" : "The original message will be added below your text automatically.";
  syncReader();
  (to ? $("compose-body") : $("compose-to")).focus();
}

function closeCompose() {
  if (!state.draft) return;
  state.draft = null;
  hideSuggestions();
  syncReader();
}

function draftHasContent() {
  return $("compose-body").value !== state.draft.initialBody || state.files.length > 0;
}

/* Files for a message. The server accepts a whole message up to its limit, usually 20–35 MB */

const MAX_FILES_BYTES = 20 * 1024 * 1024;

function renderFiles() {
  $("compose-files").replaceChildren(
    ...state.files.map((file, index) => {
      const item = el("li", "", `${file.name} · ${formatSize(file.size)} ✕`);
      item.title = "Remove file";
      item.addEventListener("click", () => {
        state.files.splice(index, 1);
        renderFiles();
      });
      return item;
    }),
  );
}

$("compose-attach").addEventListener("click", () => $("compose-file-input").click());

$("compose-file-input").addEventListener("change", (event) => {
  const files = [...state.files, ...event.target.files];
  event.target.value = "";
  if (files.reduce((sum, file) => sum + file.size, 0) > MAX_FILES_BYTES) {
    return showError("The files total more than 20 MB — the server will not accept such a message");
  }
  state.files = files;
  renderFiles();
});

function readAsBase64(file) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result.slice(reader.result.indexOf(",") + 1));
    reader.onerror = () => reject(new Error(`Could not read the file "${file.name}"`));
    reader.readAsDataURL(file);
  });
}

/* Signature: plain text, stored in the app settings on this computer */

function getSignature() {
  try {
    return localStorage.getItem("signature") || "";
  } catch {
    return "";
  }
}

$("signature-open").addEventListener("click", () => {
  $("signature-text").value = getSignature();
  $("signature-dialog").showModal();
});

$("signature-cancel").addEventListener("click", () => $("signature-dialog").close());

$("signature-form").addEventListener("submit", () => {
  const signature = $("signature-text").value.trimEnd();
  try {
    localStorage.setItem("signature", signature);
    showStatus(signature ? "Signature saved" : "Signature removed");
  } catch {
    showError("Could not save the signature");
  }
});

/* Recipient suggestions: as you type, look in the company address book and personal contacts */

const suggest = { input: null, items: [], active: 0, timer: null, token: 0 };

// Recipients are separated by commas; the suggestion applies to the last, unfinished one
function lastRecipient(input) {
  const value = input.value;
  const start = Math.max(value.lastIndexOf(","), value.lastIndexOf(";")) + 1;
  return { start, text: value.slice(start).trim() };
}

function hideSuggestions() {
  clearTimeout(suggest.timer);
  suggest.token++;
  suggest.items = [];
  $("suggestions").hidden = true;
}

function renderSuggestions() {
  const list = $("suggestions");
  list.replaceChildren(
    ...suggest.items.map((person, index) => {
      const item = el("li", index === suggest.active ? "active" : "");
      item.setAttribute("role", "option");
      item.append(el("span", "", person.name || person.email), el("span", "email", person.email));
      // mousedown, not click: otherwise the field loses focus and the list closes before the pick
      item.addEventListener("mousedown", (event) => {
        event.preventDefault();
        acceptSuggestion(index);
      });
      return item;
    }),
  );
  const rect = suggest.input.getBoundingClientRect();
  list.style.left = `${rect.left}px`;
  list.style.top = `${rect.bottom + 2}px`;
  list.style.width = `${rect.width}px`;
  list.hidden = suggest.items.length === 0;
  list.querySelector(".active")?.scrollIntoView({ block: "nearest" });
}

function acceptSuggestion(index) {
  const person = suggest.items[index];
  const input = suggest.input;
  if (!person || !input) return;
  const { start } = lastRecipient(input);
  // A comma in a name would split the recipient list
  const name = person.name.replace(/[,;<>]/g, " ").replace(/\s+/g, " ").trim();
  const entry = name && name !== person.email ? `${name} <${person.email}>` : person.email;
  input.value = `${input.value.slice(0, start)}${start ? " " : ""}${entry}, `;
  hideSuggestions();
  input.focus();
}

for (const input of [$("compose-to"), $("compose-cc")]) {
  input.addEventListener("input", () => {
    hideSuggestions();
    const { text } = lastRecipient(input);
    if (text.length < 2 || text.includes("<")) return;
    const token = suggest.token;
    suggest.timer = setTimeout(async () => {
      let people;
      try {
        people = await invoke("resolve_names", { query: text });
      } catch {
        return;
      }
      if (token !== suggest.token) return;
      suggest.input = input;
      suggest.items = people.slice(0, 8);
      suggest.active = 0;
      renderSuggestions();
    }, 250);
  });
  input.addEventListener("blur", hideSuggestions);
}

// Enter in an address or subject field must not send the message — only ⌘Enter or the button
for (const input of [$("compose-to"), $("compose-cc"), $("compose-subject")]) {
  input.addEventListener("keydown", (event) => {
    const open = suggest.input === input && suggest.items.length > 0;
    if (open && (event.key === "ArrowDown" || event.key === "ArrowUp")) {
      event.preventDefault();
      const step = event.key === "ArrowDown" ? 1 : -1;
      suggest.active = (suggest.active + step + suggest.items.length) % suggest.items.length;
      renderSuggestions();
    } else if (open && (event.key === "Enter" || event.key === "Tab")) {
      event.preventDefault();
      event.stopPropagation();
      acceptSuggestion(suggest.active);
    } else if (open && event.key === "Escape") {
      event.stopPropagation();
      hideSuggestions();
    } else if (event.key === "Enter" && !event.metaKey) {
      event.preventDefault();
    }
  });
}

function parseAddresses(text) {
  return text
    .split(/[,;\n]/)
    .map((part) => (part.match(/<([^>]+)>/)?.[1] ?? part).trim())
    .filter(Boolean);
}

$("compose").addEventListener("submit", async (event) => {
  event.preventDefault();
  const to = parseAddresses($("compose-to").value);
  const cc = parseAddresses($("compose-cc").value);
  const invalid = [...to, ...cc].find((address) => !/^[^\s@]+@[^\s@]+$/.test(address));
  if (to.length === 0) return showError("Add a recipient");
  if (invalid) return showError(`Invalid address: ${invalid}`);
  const button = $("compose-send");
  button.disabled = true;
  button.textContent = "Sending…";
  try {
    const attachments = await Promise.all(
      state.files.map(async (file) => ({ name: file.name, contentBase64: await readAsBase64(file) })),
    );
    await invoke("send_message", {
      outgoing: {
        attachments,
        mode: state.draft.mode,
        to,
        cc,
        subject: $("compose-subject").value.trim(),
        bodyHtml: `<div>${escapeHtml($("compose-body").value).replace(/\n/g, "<br>")}</div>`,
        referenceId: state.draft.referenceId,
      },
    });
    closeCompose();
    showStatus("Sent");
  } catch (error) {
    showError(error);
  } finally {
    button.disabled = false;
    button.textContent = "Send";
  }
});

$("compose-new").addEventListener("click", () => openCompose("new"));
$("compose-cancel").addEventListener("click", closeCompose);
$("reply").addEventListener("click", () => openCompose("reply"));
$("reply-all").addEventListener("click", () => openCompose("replyAll"));
$("forward").addEventListener("click", () => openCompose("forward"));

/* Keyboard: J/K or arrows move through messages, R refreshes, N composes */

document.addEventListener("keydown", (event) => {
  if ($("app").hidden) return;
  if (state.draft) {
    if (event.key === "Escape") {
      // Esc is easy to hit by accident, so it does not close a message in progress
      if (draftHasContent()) showStatus("This message has text — use Cancel to close it");
      else closeCompose();
    } else if (event.key === "Enter" && event.metaKey) {
      $("compose").requestSubmit();
    }
    return;
  }
  if (event.metaKey || event.ctrlKey || event.altKey || event.target.closest?.("input, textarea, select")) return;
  // Shortcuts are matched by physical key, so they work in any keyboard layout
  if (event.code === "KeyN") {
    event.preventDefault();
    openCompose("new");
    return;
  }
  if (event.code === "Slash") {
    event.preventDefault();
    $("search").focus();
    return;
  }
  if (event.code === "KeyJ" || event.key === "ArrowDown") {
    event.preventDefault();
    openMessage(Math.min(state.selected + 1, state.messages.length - 1));
  } else if (event.code === "KeyK" || event.key === "ArrowUp") {
    event.preventDefault();
    if (state.selected > 0) openMessage(state.selected - 1);
  } else if (event.code === "KeyR") {
    event.preventDefault();
    refresh();
  }
});

async function refresh() {
  showStatus("Refreshing…");
  await loadFolders();
}

$("refresh").addEventListener("click", refresh);

/* Column widths: borders can be dragged, and the chosen width is remembered */

for (const splitter of document.querySelectorAll(".splitter")) {
  const name = splitter.dataset.var;
  const min = Number(splitter.dataset.min);
  const max = Number(splitter.dataset.max);
  try {
    const saved = localStorage.getItem(name);
    if (saved) $("app").style.setProperty(name, saved);
  } catch {}
  splitter.addEventListener("pointerdown", (event) => {
    event.preventDefault();
    // Pointer capture: otherwise the message frame steals mouse movement and the drag breaks off
    splitter.setPointerCapture(event.pointerId);
    splitter.classList.add("dragging");
    const startX = event.clientX;
    const startWidth = splitter.parentElement.getBoundingClientRect().width;
    let width = startWidth;
    const onMove = (move) => {
      width = Math.round(Math.min(max, Math.max(min, startWidth + move.clientX - startX)));
      $("app").style.setProperty(name, `${width}px`);
    };
    const onUp = () => {
      splitter.classList.remove("dragging");
      splitter.removeEventListener("pointermove", onMove);
      try {
        localStorage.setItem(name, `${width}px`);
      } catch {}
    };
    splitter.addEventListener("pointermove", onMove);
    splitter.addEventListener("pointerup", onUp, { once: true });
    splitter.addEventListener("pointercancel", onUp, { once: true });
  });
}

/* Startup */

(async () => {
  try {
    const session = await invoke("restore_session");
    if (session.loggedIn) await showMain();
    else showLogin(session.account, session.error);
  } catch (error) {
    showLogin(null, String(error));
  }
})();
