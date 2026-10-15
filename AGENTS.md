# Instructions for agents

mini-mail is a macOS mail client for on-premises Exchange (EWS). `README.md` covers what it does
and how to build it; this file covers what you need to know before changing the code.

## Stack and layout

- `src-tauri/src/ews.rs` — the EWS client: SOAP requests built from string templates, responses
  parsed with `roxmltree`. All traffic to the server lives here.
- `src-tauri/src/commands.rs` — Tauri commands, sign-in state, keychain, files, the background
  mail check and notifications.
- `src-tauri/src/lib.rs` — command registration. A new command must be added to `generate_handler!`.
- `ui/index.html`, `ui/style.css`, `ui/app.js` — the interface, with no framework and no bundler.
  Core commands are called through `window.__TAURI__.core.invoke`.
- `scripts/check-frame-hash.mjs` — the check described under "The message frame" below.

Everything in the repository is in English: interface text, error messages, comments, tests and
documentation. Mail data coming from the server is shown as is.

## Commands

```bash
npm test
```

Runs a syntax check of `ui/app.js`, the frame script hash check, and `cargo test`. Run it after
every change; the build must stay free of compiler warnings.

```bash
npm run install-app
```

Builds the app and installs it into `/Applications`. Afterwards the keychain asks the user once
for permission, so do not install without need, and warn the user when you do.

If `cargo` is not found and Rust was installed with Homebrew, add `/opt/homebrew/opt/rustup/bin`
to `PATH`.

## How to verify changes

- An agent has no real server: signing in needs the user's password, which only the user may
  enter. Do not claim a change was verified against the server unless the user did it.
- Cover response parsing and request building with unit tests in `ews.rs` on sample XML.
- The interface is checked in a browser on a throwaway page: a copy of `ui/` plus a stub script
  that replaces `window.__TAURI__` (`core.invoke` and `event.listen`) and returns fake data.
  Keep the stub outside the repository.
- The app runs in the Safari engine (WKWebView), not Chrome: behavior in a Chromium browser is
  strong evidence, not proof.

## What must not break

- **No mail on disk.** The app is online-only: message bodies and attachments are never cached.
  The one exception is a file the user chose to save to Downloads.
- **The password lives only in the keychain.** Never write it to files, logs or error messages.
- **HTTPS only.** The check in `login` exists because Basic authentication sends the password
  nearly in the clear.
- **Nothing is deleted permanently.** `DeleteItem` is only ever called with `MoveToDeletedItems`.
- **Every value placed into XML goes through `escape()`.** That covers ids, names and text.
- **Server data reaches the DOM only through `textContent`** (the `el` helper). Message HTML is
  shown exclusively in the isolated `#msg-body` frame.
- **Text being written is never lost silently.** Leaving the compose form with changes (opening
  another message, starting another one, Esc, "Save draft") saves it to the Drafts folder on the
  server first; if saving fails the form stays open. Only "Discard" throws a message away, and a
  saved draft then goes to Deleted Items.
- **A draft is an ordinary message in Drafts.** Opening one there loads it back into the form. A
  reply or forward draft carries the `MiniMailDraft` extended property (`mode|original id`); the
  real reply, with quote and threading, is built by the server only on send, after which the
  draft copy is moved to Deleted Items. A new-message draft is sent as itself.
- **Attached files are not stored in drafts.** They exist only in the open form and are uploaded
  on send.
- **An action applies to the message on screen.** Delete and invitation responses use
  `state.openId`, not the selected row: while the next message loads, these differ.
- **The background check stops on an authentication error.** Retrying with an old password locks
  the domain account. The signal is `ews::AUTH_ERROR`.
- **Shortcuts are matched by `event.code`,** so they work in any keyboard layout.

## The message frame

Message HTML goes into an `<iframe sandbox="allow-scripts">` without `allow-same-origin`: the frame
has its own origin, cannot see app data and cannot call app commands. Inside it a CSP blocks
everything except inline styles, `data:` images and one script of ours, `FRAME_SCRIPT`, which
reports link clicks through `postMessage`.

That script is allowed by hash, and the hash is written in three places: the `FRAME_SCRIPT` text
and `FRAME_SCRIPT_HASH` in `ui/app.js`, and `script-src` in `src-tauri/tauri.conf.json`. After any
edit to the script, update both hashes; `npm test` checks that they match.

Do not add `allow-same-origin` to the frame and do not loosen the CSP: this is the only barrier
between the markup of someone else's message and the app's commands. The `message` handler in
`app.js` accepts messages only from the frame's window and opens only `http`, `https` and `mailto`
links.

## Exchange 2013 quirks

- `FindItem` cannot sort by `message:IsRead` (`ErrorUnsupportedPathForSortGroup`). The unread-first
  order is assembled from two requests filtered on that field.
- `QueryString` in `FindItem` cannot be combined with `SortOrder` or `Restriction`.
- A request for a field that the item type does not have is rejected as a whole. That is why
  meeting details come from a separate request with `AllProperties`.
- "Nobody found" in `ResolveNames` arrives as the error `ErrorNameResolutionNoResults`.
- Message bodies sometimes contain references to control characters that are invalid in XML;
  `strip_invalid_char_refs` removes them before parsing.
- Every request is sent with `RequestServerVersion Version="Exchange2013"`; the server does not
  understand operations from newer versions.
- After an invitation response the server removes the invitation from the Inbox itself.
- Replies, forwards and invitation responses need the current `ChangeKey` on `ReferenceItemId`
  (`ErrorChangeKeyRequiredForWriteOperations`). The key changes whenever the item does, so
  `reference_item_id` fetches it right before the request.
