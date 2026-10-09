# mini-mail

A minimal mail client for macOS that talks to an on-premises Microsoft Exchange server over EWS.
Built for personal use: the core is Rust, the interface is plain HTML, CSS and JavaScript, and
Tauri 2 packages it into an app.

Tested against Exchange 2013 with Basic authentication over HTTPS.

## Features

- Mail folder tree with counters; create folders from the right-click menu.
- Message list ordered newest first or unread first, loaded 50 at a time.
- HTML messages with inline images, dark mode, links that open in the browser.
- New message, reply, reply all, forward; file attachments; a persistent signature.
- Recipient suggestions from the company address book and personal contacts.
- Attachments download to the Downloads folder.
- Search in the open folder (Outlook syntax: `from:ivan`, `subject:report`).
- Delete (moves to Deleted Items), move between folders by right-click or drag and drop,
  mark all as read.
- Meeting invitations: time, place, organizer, and accept / tentative / decline.
- New-mail notifications and the unread count on the Dock icon.

## How it works

| Part | Where | What it does |
| --- | --- | --- |
| EWS client | `src-tauri/src/ews.rs` | SOAP requests to Exchange and response parsing |
| Commands | `src-tauri/src/commands.rs` | Sign-in, keychain, files, new-mail check |
| Interface | `ui/` | Three columns: folders, messages, reading or writing |

The app is online-only: messages are fetched on demand and live only in memory. What is stored
on disk:

- the server address and username, in the app settings file;
- the password, only in the macOS keychain;
- column widths, sort order and the signature, in the window's local storage.

The app does not duplicate the calendar or contacts: macOS Calendar and Contacts can work with
Exchange on their own (System Settings → Internet Accounts). A meeting accepted in mini-mail lands
on the server, and Calendar picks it up from there.

## Build and run

You need Rust (stable), Node.js and the Xcode command line tools.

```bash
npm install
```

Run in development mode:

```bash
npm run dev
```

Run the tests (interface syntax, the message frame script hash check, and the core tests):

```bash
npm test
```

Build and install into Applications:

```bash
npm run install-app
```

If Rust was installed with Homebrew (`brew install rustup`), it is not on `PATH` by default; add
`export PATH="/opt/homebrew/opt/rustup/bin:$PATH"` to `~/.zshrc`.

After every rebuild the keychain asks once for access to the password: macOS ties it to a specific
build, and the app is only signed locally. Choose Always Allow.

## First sign-in

- **Server address** — the EWS endpoint, usually `https://mail-server/EWS/Exchange.asmx`.
- **Username** — as `DOMAIN\username` or `username@domain`; a bare username may be rejected.
- **Password** — your domain password.

## Keyboard

| Key | Action |
| --- | --- |
| J / K or ↓ / ↑ | Next / previous message |
| N | New message |
| R | Refresh |
| / | Search the folder |
| ⌘Enter | Send |
| Esc | Close an empty message, clear the search |

Shortcuts are matched by physical key, so they work in any keyboard layout.

## Security

- HTTPS only: without encryption Basic authentication would send the password in the clear.
- A message is shown in an isolated frame: it has its own origin with no access to app data or
  commands, and a CSP blocks foreign scripts and remote images.
- Hovering a link shows its real address at the bottom of the window.
- Nothing is deleted permanently: Delete moves a message to Deleted Items.
- If the server stops accepting the password, the background mail check stops, so repeated
  attempts cannot lock the domain account.

## Limitations

- Exchange over EWS with Basic authentication only. No Exchange Online, NTLM or Kerberos.
- One account.
- No offline mode: without a connection the app shows nothing.
- Messages are written as plain text, without formatting.
- Search covers the open folder, not the whole mailbox.
- Attached messages (a message attached to a message) cannot be downloaded.
- New mail is checked once a minute and only while the app is running.
- Folders cannot be renamed or deleted.
- The interface is in English.
