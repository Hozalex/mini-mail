use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::async_runtime::JoinHandle;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_notification::NotificationExt;

use crate::ews::{
    self, Address, Client, Draft, Folder, Message, MessagePage, MessageSummary, Outgoing,
};

const KEYCHAIN_SERVICE: &str = "mini-mail";
const NEW_MAIL_CHECK_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct AppState {
    client: RwLock<Option<Client>>,
    /// Background new-mail check for the current sign-in
    watcher: Mutex<Option<JoinHandle<()>>>,
}

impl AppState {
    fn sign_in(&self, app: &AppHandle, client: Client) {
        *self.client.write().unwrap() = Some(client.clone());
        let watcher = tauri::async_runtime::spawn(watch_inbox(app.clone(), client));
        if let Some(previous) = self.watcher.lock().unwrap().replace(watcher) {
            previous.abort();
        }
    }

    fn sign_out(&self) {
        *self.client.write().unwrap() = None;
        if let Some(watcher) = self.watcher.lock().unwrap().take() {
            watcher.abort();
        }
    }
}

/// Once a minute looks at the latest Inbox messages: shows a notification for new unread ones
/// and tells the window that the list should be refreshed.
async fn watch_inbox(app: AppHandle, client: Client) {
    // The server returns dates in one format (UTC, ISO 8601), so the strings compare as times
    let mut newest_seen = String::new();
    let mut primed = false;
    loop {
        let result = client.recent_inbox().await;
        if matches!(&result, Err(error) if error == ews::AUTH_ERROR) {
            // The password was changed or revoked. Do not retry: the domain locks the account
            // after several failed sign-ins in a row
            let _ = app.emit("auth:failed", ());
            return;
        }
        if let Ok(page) = result {
            if primed {
                let fresh: Vec<&MessageSummary> =
                    page.items.iter().filter(|m| m.date > newest_seen).collect();
                if !fresh.is_empty() {
                    notify_new_mail(&app, &fresh);
                    let _ = app.emit("mail:new", ());
                }
            }
            if let Some(newest) = page.items.iter().map(|m| &m.date).max() {
                if *newest > newest_seen {
                    newest_seen = newest.clone();
                }
            }
            primed = true;
        }
        tokio::time::sleep(NEW_MAIL_CHECK_INTERVAL).await;
    }
}

fn notify_new_mail(app: &AppHandle, fresh: &[&MessageSummary]) {
    let unread: Vec<&&MessageSummary> = fresh.iter().filter(|m| !m.is_read).collect();
    let show = |title: &str, body: &str| {
        let _ = app.notification().builder().title(title).body(body).show();
    };
    let subject = |m: &MessageSummary| {
        if m.subject.is_empty() { "(no subject)".to_string() } else { m.subject.clone() }
    };
    if unread.len() > 3 {
        let subjects: Vec<String> = unread.iter().take(3).map(|m| subject(m)).collect();
        show(&format!("New messages: {}", unread.len()), &subjects.join("\n"));
    } else {
        for message in unread {
            let from = if message.from.is_empty() { "New message" } else { &message.from };
            show(from, &subject(message));
        }
    }
}

/// The server address and username live in a settings file; the password only in the macOS keychain.
#[derive(Serialize, Deserialize, Clone)]
pub struct Account {
    url: String,
    username: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    account: Option<Account>,
    logged_in: bool,
    error: Option<String>,
}

#[tauri::command]
pub async fn restore_session(app: AppHandle, state: State<'_, AppState>) -> Result<Session, String> {
    let Some(account) = read_account(&app) else {
        return Ok(Session { account: None, logged_in: false, error: None });
    };
    let Ok(password) = keychain_entry(&account).and_then(|entry| entry.get_password()) else {
        return Ok(Session { account: Some(account), logged_in: false, error: None });
    };
    let client = Client::new(&account.url, &account.username, &password)?;
    match client.verify().await {
        Ok(()) => {
            state.sign_in(&app, client);
            Ok(Session { account: Some(account), logged_in: true, error: None })
        }
        Err(error) => Ok(Session { account: Some(account), logged_in: false, error: Some(error) }),
    }
}

#[tauri::command]
pub async fn login(
    app: AppHandle,
    state: State<'_, AppState>,
    url: String,
    username: String,
    password: String,
) -> Result<Account, String> {
    let account = Account {
        url: url.trim().to_string(),
        username: username.trim().to_string(),
    };
    // Basic auth sends the password nearly in the clear, so HTTPS only
    if !account.url.starts_with("https://") {
        return Err("The server address must start with https://".to_string());
    }
    if account.username.is_empty() || password.is_empty() {
        return Err("Enter the username and password".to_string());
    }
    let client = Client::new(&account.url, &account.username, &password)?;
    client.verify().await?;

    write_account(&app, &account)?;
    // A keychain item is tied to the build that created it: macOS lets another build change it
    // only after a password prompt, but delete and recreate it without one
    let save = keychain_entry(&account).and_then(|entry| {
        let _ = entry.delete_credential();
        entry.set_password(&password)
    });
    if let Err(error) = save {
        eprintln!("mini-mail: the password was not saved to the keychain: {error}");
    }
    state.sign_in(&app, client);
    Ok(account)
}

#[tauri::command]
pub fn logout(app: AppHandle, state: State<'_, AppState>) {
    state.sign_out();
    if let Some(account) = read_account(&app) {
        if let Ok(entry) = keychain_entry(&account) {
            let _ = entry.delete_credential();
        }
    }
}

#[tauri::command]
pub async fn list_folders(state: State<'_, AppState>) -> Result<Vec<Folder>, String> {
    client(&state)?.list_folders().await
}

#[tauri::command]
pub async fn list_messages(
    state: State<'_, AppState>,
    folder_id: String,
    offset: u32,
    query: String,
    unread_first: bool,
) -> Result<MessagePage, String> {
    client(&state)?
        .list_messages(&folder_id, offset, query.trim(), unread_first)
        .await
}

#[tauri::command]
pub async fn create_folder(
    state: State<'_, AppState>,
    parent_id: Option<String>,
    name: String,
) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Enter a folder name".to_string());
    }
    client(&state)?.create_folder(parent_id.as_deref(), name).await
}

/// Saves an attachment to Downloads, reveals it in Finder and returns the file name.
#[tauri::command]
pub async fn download_attachment(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<String, String> {
    let (name, bytes) = client(&state)?.download_attachment(&id).await?;
    let dir = app
        .path()
        .download_dir()
        .map_err(|e| format!("Downloads folder not found: {e}"))?;
    let path = write_new_file(&dir, &safe_file_name(&name), &bytes)
        .map_err(|e| format!("Could not save the file: {e}"))?;
    let _ = std::process::Command::new("open").arg("-R").arg(&path).spawn();
    Ok(path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default())
}

/// The attachment name comes from the sender, so anything that could move the file out of
/// Downloads or make it hidden is stripped from it.
fn safe_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c == '/' || c == ':' || c == '\\' || c.is_control() { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_string();
    if cleaned.is_empty() {
        "attachment".to_string()
    } else {
        cleaned
    }
}

/// Writes a file without overwriting existing ones: on a name clash appends " (2)", " (3)"…
fn write_new_file(dir: &std::path::Path, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::Write;
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
        _ => (name, String::new()),
    };
    for attempt in 1..1000 {
        let file_name = if attempt == 1 {
            name.to_string()
        } else {
            format!("{stem} ({attempt}){extension}")
        };
        let path = dir.join(file_name);
        match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(bytes)?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::other("too many files with this name"))
}

#[tauri::command]
pub async fn get_message(state: State<'_, AppState>, id: String) -> Result<Message, String> {
    client(&state)?.get_message(&id).await
}

#[tauri::command]
pub async fn mark_read(
    state: State<'_, AppState>,
    id: String,
    change_key: String,
    read: bool,
) -> Result<(), String> {
    client(&state)?.mark_read(&id, &change_key, read).await
}

#[tauri::command]
pub async fn delete_message(state: State<'_, AppState>, id: String) -> Result<(), String> {
    client(&state)?.delete_message(&id).await
}

#[tauri::command]
pub async fn resolve_names(state: State<'_, AppState>, query: String) -> Result<Vec<Address>, String> {
    client(&state)?.resolve_names(query.trim()).await
}

#[tauri::command]
pub async fn move_message(
    state: State<'_, AppState>,
    id: String,
    folder_id: String,
) -> Result<(), String> {
    client(&state)?.move_message(&id, &folder_id).await
}

#[tauri::command]
pub async fn mark_all_read(state: State<'_, AppState>, folder_id: String) -> Result<(), String> {
    client(&state)?.mark_all_read(&folder_id).await
}

#[tauri::command]
pub async fn respond_to_meeting(
    state: State<'_, AppState>,
    id: String,
    response: String,
) -> Result<(), String> {
    client(&state)?.respond_to_meeting(&id, &response).await
}

/// Inbox unread count on the app's Dock icon.
#[tauri::command]
pub fn set_badge(app: AppHandle, count: i64) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_badge_count((count > 0).then_some(count));
    }
}

#[tauri::command]
pub async fn send_message(
    state: State<'_, AppState>,
    outgoing: Outgoing,
    draft_id: Option<String>,
) -> Result<(), String> {
    client(&state)?.send(&outgoing, draft_id.as_deref()).await
}

/// Saves the message being written to Drafts and returns the draft id.
#[tauri::command]
pub async fn save_draft(
    state: State<'_, AppState>,
    outgoing: Outgoing,
    draft_id: Option<String>,
) -> Result<String, String> {
    client(&state)?.save_draft(&outgoing, draft_id.as_deref()).await
}

#[tauri::command]
pub async fn get_draft(state: State<'_, AppState>, id: String) -> Result<Draft, String> {
    client(&state)?.get_draft(&id).await
}

/// Opens a link from a message in the default browser.
#[tauri::command]
pub fn open_link(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("Only http and https links can be opened".to_string());
    }
    std::process::Command::new("open")
        .arg(&url)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Could not open the link: {e}"))
}

fn client(state: &State<'_, AppState>) -> ews::Result<Client> {
    state
        .client
        .read()
        .unwrap()
        .clone()
        .ok_or_else(|| "Not signed in".to_string())
}

fn keychain_entry(account: &Account) -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, &format!("{} @ {}", account.username, account.url))
}

fn account_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|dir| dir.join("account.json"))
        .map_err(|e| format!("Settings folder not found: {e}"))
}

fn read_account(app: &AppHandle) -> Option<Account> {
    let text = fs::read_to_string(account_path(app).ok()?).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_account(app: &AppHandle, account: &Account) -> Result<(), String> {
    let path = account_path(app)?;
    let save = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&path, serde_json::to_string_pretty(account)?)
    };
    save().map_err(|e| format!("Could not save the settings: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_attachment_names() {
        assert_eq!(safe_file_name("../../.ssh/authorized_keys"), "_.._.ssh_authorized_keys");
        assert_eq!(safe_file_name("résumé.pdf"), "résumé.pdf");
        assert_eq!(safe_file_name(" .. "), "attachment");
    }

    #[test]
    fn never_overwrites_existing_files() {
        let dir = std::env::temp_dir().join(format!("mini-mail-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let first = write_new_file(&dir, "a.txt", b"1").unwrap();
        let second = write_new_file(&dir, "a.txt", b"2").unwrap();
        assert_eq!(first.file_name().unwrap(), "a.txt");
        assert_eq!(second.file_name().unwrap(), "a (2).txt");
        assert_eq!(fs::read(&first).unwrap(), b"1");
        fs::remove_dir_all(&dir).unwrap();
    }
}
