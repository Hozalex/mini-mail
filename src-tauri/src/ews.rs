//! Minimal Exchange Web Services client: SOAP over HTTPS with Basic auth.
//! Online-only: it caches nothing and writes nothing to disk.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use base64::Engine;
use roxmltree::{Document, Node};
use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, String>;

pub const PAGE_SIZE: u32 = 50;

/// Error text for a rejected sign-in. The background mail check compares against it to learn
/// that the password no longer works and to stop hitting the server.
pub const AUTH_ERROR: &str = "The server rejected the username or password";

const MAX_INLINE_IMAGES: usize = 30;
const MAX_INLINE_IMAGE_BYTES: u64 = 3 * 1024 * 1024;

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    url: String,
    username: String,
    password: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    pub id: String,
    pub name: String,
    /// inbox | drafts | sent | outbox | trash | junk | "" for ordinary folders
    pub kind: String,
    pub depth: u32,
    pub total: u32,
    pub unread: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageSummary {
    pub id: String,
    pub change_key: String,
    pub subject: String,
    pub from: String,
    pub to: String,
    pub date: String,
    pub preview: String,
    pub is_read: bool,
    pub has_attachments: bool,
    /// A meeting invitation, its cancellation, or a response to it
    pub is_meeting: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePage {
    pub items: Vec<MessageSummary>,
    pub total: u32,
    pub last: bool,
}

#[derive(Serialize)]
pub struct Address {
    pub name: String,
    pub email: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub content_type: String,
    pub size: u64,
    pub inline: bool,
    #[serde(skip)]
    content_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub change_key: String,
    pub subject: String,
    pub from: Option<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub date: String,
    /// Mailbox that received the message; used so "Reply all" does not reply to yourself
    pub received_by: String,
    pub is_read: bool,
    pub is_html: bool,
    pub body: String,
    pub attachments: Vec<Attachment>,
    /// Set for meeting invitations and cancellations
    pub meeting: Option<Meeting>,
    #[serde(skip)]
    item_type: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Meeting {
    /// false — the organizer cancelled the meeting, there is nothing to respond to
    pub is_request: bool,
    pub start: String,
    pub end: String,
    pub all_day: bool,
    pub location: String,
    pub organizer: String,
    /// Response already given: Accept | Tentative | Decline; anything else means none yet
    pub response: String,
    /// The organizer has sent a newer version of this invitation
    pub out_of_date: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Outgoing {
    /// new | reply | replyAll | forward
    pub mode: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body_html: String,
    /// Original message for a reply or a forward
    pub reference_id: Option<String>,
    #[serde(default)]
    pub attachments: Vec<OutgoingAttachment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutgoingAttachment {
    pub name: String,
    pub content_base64: String,
}

impl Client {
    pub fn new(url: &str, username: &str, password: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .cookie_store(true)
            .timeout(Duration::from_secs(180))
            .build()
            .map_err(|e| format!("Could not create the HTTP client: {e}"))?;
        Ok(Self {
            http,
            url: url.to_string(),
            username: username.to_string(),
            password: password.to_string(),
        })
    }

    /// Checks the server address and credentials with a request for the Inbox.
    pub async fn verify(&self) -> Result<()> {
        let raw = self
            .call(
                r#"<m:GetFolder><m:FolderShape><t:BaseShape>IdOnly</t:BaseShape></m:FolderShape><m:FolderIds><t:DistinguishedFolderId Id="inbox"/></m:FolderIds></m:GetFolder>"#,
            )
            .await?;
        with_doc(&raw, true, |_| Ok(()))
    }

    pub async fn list_folders(&self) -> Result<Vec<Folder>> {
        let well_known = self.well_known_folders().await?;
        let raw = self
            .call(
                r#"<m:FindFolder Traversal="Deep"><m:FolderShape><t:BaseShape>IdOnly</t:BaseShape><t:AdditionalProperties><t:FieldURI FieldURI="folder:DisplayName"/><t:FieldURI FieldURI="folder:ParentFolderId"/><t:FieldURI FieldURI="folder:FolderClass"/><t:FieldURI FieldURI="folder:TotalCount"/><t:FieldURI FieldURI="folder:UnreadCount"/></t:AdditionalProperties></m:FolderShape><m:IndexedPageFolderView MaxEntriesReturned="1000" Offset="0" BasePoint="Beginning"/><m:ParentFolderIds><t:DistinguishedFolderId Id="msgfolderroot"/></m:ParentFolderIds></m:FindFolder>"#,
            )
            .await?;
        let raw_folders = with_doc(&raw, true, |doc| Ok(parse_folders(doc)))?;
        Ok(build_folder_tree(raw_folders, &well_known))
    }

    /// Messages of a folder, newest first. A non-empty `query` searches the folder
    /// (AQS syntax, as in Outlook: words, `from:name`, `subject:text`).
    pub async fn list_messages(
        &self,
        folder_id: &str,
        offset: u32,
        query: &str,
        unread_first: bool,
    ) -> Result<MessagePage> {
        let folder = format!(r#"<t:FolderId Id="{}"/>"#, escape(folder_id));
        if !unread_first || !query.is_empty() {
            return self.find_items(&folder, offset, PAGE_SIZE, query, "").await;
        }
        // The server cannot sort by read state but can filter by it,
        // so the list is stitched from two queries: unread first, then read
        let mut page = self
            .find_items(&folder, offset, PAGE_SIZE, "", &is_read_restriction(false))
            .await?;
        let unread_total = page.total;
        let received = page.items.len() as u32;
        if received == PAGE_SIZE {
            page.last = false;
            return Ok(page);
        }
        let read_offset = (offset + received).saturating_sub(unread_total);
        let read = self
            .find_items(&folder, read_offset, PAGE_SIZE - received, "", &is_read_restriction(true))
            .await?;
        page.items.extend(read.items);
        page.total = unread_total + read.total;
        page.last = read.last;
        Ok(page)
    }

    /// The few most recent Inbox messages, for the new-mail check.
    pub async fn recent_inbox(&self) -> Result<MessagePage> {
        self.find_items(r#"<t:DistinguishedFolderId Id="inbox"/>"#, 0, 10, "", "").await
    }

    async fn find_items(
        &self,
        folder: &str,
        offset: u32,
        limit: u32,
        query: &str,
        restriction: &str,
    ) -> Result<MessagePage> {
        // The server orders search results itself and rejects a sort order alongside QueryString
        let (sort, query_string) = if query.is_empty() {
            (
                r#"<m:SortOrder><t:FieldOrder Order="Descending"><t:FieldURI FieldURI="item:DateTimeReceived"/></t:FieldOrder></m:SortOrder>"#,
                String::new(),
            )
        } else {
            ("", format!("<m:QueryString>{}</m:QueryString>", escape(query)))
        };
        let body = format!(
            r#"<m:FindItem Traversal="Shallow"><m:ItemShape><t:BaseShape>IdOnly</t:BaseShape><t:AdditionalProperties><t:FieldURI FieldURI="item:Subject"/><t:FieldURI FieldURI="item:DateTimeReceived"/><t:FieldURI FieldURI="item:HasAttachments"/><t:FieldURI FieldURI="item:DisplayTo"/><t:FieldURI FieldURI="item:Preview"/><t:FieldURI FieldURI="message:From"/><t:FieldURI FieldURI="message:IsRead"/></t:AdditionalProperties></m:ItemShape><m:IndexedPageItemView MaxEntriesReturned="{limit}" Offset="{offset}" BasePoint="Beginning"/>{restriction}{sort}<m:ParentFolderIds>{folder}</m:ParentFolderIds>{query_string}</m:FindItem>"#
        );
        let raw = self.call(&body).await?;
        with_doc(&raw, true, parse_message_page)
    }

    pub async fn get_message(&self, id: &str) -> Result<Message> {
        let body = format!(
            r#"<m:GetItem><m:ItemShape><t:BaseShape>IdOnly</t:BaseShape><t:BodyType>HTML</t:BodyType><t:AdditionalProperties><t:FieldURI FieldURI="item:Subject"/><t:FieldURI FieldURI="item:Body"/><t:FieldURI FieldURI="item:DateTimeReceived"/><t:FieldURI FieldURI="item:Attachments"/><t:FieldURI FieldURI="message:From"/><t:FieldURI FieldURI="message:ToRecipients"/><t:FieldURI FieldURI="message:CcRecipients"/><t:FieldURI FieldURI="message:ReceivedBy"/><t:FieldURI FieldURI="message:IsRead"/></t:AdditionalProperties></m:ItemShape><m:ItemIds><t:ItemId Id="{}"/></m:ItemIds></m:GetItem>"#,
            escape(id)
        );
        let raw = self.call(&body).await?;
        let mut message = with_doc(&raw, true, parse_message)?;
        let is_request = message.item_type == "MeetingRequest";
        if is_request || message.item_type == "MeetingCancellation" {
            // The message is readable without meeting details, so a failure here is not fatal
            message.meeting = self.meeting_details(id, is_request).await.ok();
        }
        if message.is_html {
            // Images from signatures and layout: without them the message still shows, just with gaps
            let _ = self.embed_inline_images(&mut message).await;
        }
        Ok(message)
    }

    pub async fn mark_read(&self, id: &str, change_key: &str, read: bool) -> Result<()> {
        let body = format!(
            r#"<m:UpdateItem MessageDisposition="SaveOnly" ConflictResolution="AlwaysOverwrite"><m:ItemChanges><t:ItemChange><t:ItemId Id="{}" ChangeKey="{}"/><t:Updates><t:SetItemField><t:FieldURI FieldURI="message:IsRead"/><t:Message><t:IsRead>{read}</t:IsRead></t:Message></t:SetItemField></t:Updates></t:ItemChange></m:ItemChanges></m:UpdateItem>"#,
            escape(id),
            escape(change_key)
        );
        let raw = self.call(&body).await?;
        with_doc(&raw, true, |_| Ok(()))
    }

    /// Meeting time, place and organizer. All properties are requested at once: invitations and
    /// cancellations have different fields, and the server rejects a request for a missing one.
    async fn meeting_details(&self, id: &str, is_request: bool) -> Result<Meeting> {
        let raw = self
            .call(&format!(
                r#"<m:GetItem><m:ItemShape><t:BaseShape>AllProperties</t:BaseShape></m:ItemShape><m:ItemIds><t:ItemId Id="{}"/></m:ItemIds></m:GetItem>"#,
                escape(id)
            ))
            .await?;
        with_doc(&raw, true, |doc| parse_meeting(doc, is_request))
    }

    /// Responds to an invitation: `accept`, `tentative` or `decline`. The organizer gets the reply,
    /// the meeting lands in the server calendar, and the server removes the invitation from the Inbox.
    pub async fn respond_to_meeting(&self, id: &str, response: &str) -> Result<()> {
        let tag = match response {
            "accept" => "AcceptItem",
            "tentative" => "TentativelyAcceptItem",
            "decline" => "DeclineItem",
            _ => return Err("Unknown invitation response".to_string()),
        };
        let raw = self
            .call(&format!(
                r#"<m:CreateItem MessageDisposition="SendAndSaveCopy"><m:Items><t:{tag}><t:ReferenceItemId Id="{}"/></t:{tag}></m:Items></m:CreateItem>"#,
                escape(id)
            ))
            .await?;
        with_doc(&raw, true, |_| Ok(()))
    }

    /// Finds people by the start of a name or address, in the company address book
    /// and in personal contacts on the server.
    pub async fn resolve_names(&self, query: &str) -> Result<Vec<Address>> {
        let raw = self
            .call(&format!(
                r#"<m:ResolveNames ReturnFullContactData="false" SearchScope="ActiveDirectoryContacts"><m:UnresolvedEntry>{}</m:UnresolvedEntry></m:ResolveNames>"#,
                escape(query)
            ))
            .await?;
        with_doc(&raw, false, parse_resolved_names)
    }

    pub async fn move_message(&self, id: &str, folder_id: &str) -> Result<()> {
        let raw = self
            .call(&format!(
                r#"<m:MoveItem><m:ToFolderId><t:FolderId Id="{}"/></m:ToFolderId><m:ItemIds><t:ItemId Id="{}"/></m:ItemIds></m:MoveItem>"#,
                escape(folder_id),
                escape(id)
            ))
            .await?;
        with_doc(&raw, true, |_| Ok(()))
    }

    /// Marks every message in a folder as read. No read receipts are sent.
    pub async fn mark_all_read(&self, folder_id: &str) -> Result<()> {
        let raw = self
            .call(&format!(
                r#"<m:MarkAllItemsAsRead><m:ReadFlag>true</m:ReadFlag><m:SuppressReadReceipts>true</m:SuppressReadReceipts><m:FolderIds><t:FolderId Id="{}"/></m:FolderIds></m:MarkAllItemsAsRead>"#,
                escape(folder_id)
            ))
            .await?;
        with_doc(&raw, true, |_| Ok(()))
    }

    /// Creates a mail folder inside `parent_id`, or at the top of the mailbox when it is not given.
    pub async fn create_folder(&self, parent_id: Option<&str>, name: &str) -> Result<()> {
        let parent = match parent_id {
            Some(id) => format!(r#"<t:FolderId Id="{}"/>"#, escape(id)),
            None => r#"<t:DistinguishedFolderId Id="msgfolderroot"/>"#.to_string(),
        };
        let raw = self
            .call(&format!(
                r#"<m:CreateFolder><m:ParentFolderId>{parent}</m:ParentFolderId><m:Folders><t:Folder><t:FolderClass>IPF.Note</t:FolderClass><t:DisplayName>{}</t:DisplayName></t:Folder></m:Folders></m:CreateFolder>"#,
                escape(name)
            ))
            .await?;
        with_doc(&raw, true, |_| Ok(()))
    }

    /// Moves a message to Deleted Items, where it can be recovered.
    pub async fn delete_message(&self, id: &str) -> Result<()> {
        let raw = self
            .call(&format!(
                r#"<m:DeleteItem DeleteType="MoveToDeletedItems"><m:ItemIds><t:ItemId Id="{}"/></m:ItemIds></m:DeleteItem>"#,
                escape(id)
            ))
            .await?;
        with_doc(&raw, true, |_| Ok(()))
    }

    /// Downloads an attachment: returns its file name and contents.
    pub async fn download_attachment(&self, id: &str) -> Result<(String, Vec<u8>)> {
        let raw = self
            .call(&format!(
                r#"<m:GetAttachment><m:AttachmentIds><t:AttachmentId Id="{}"/></m:AttachmentIds></m:GetAttachment>"#,
                escape(id)
            ))
            .await?;
        let (name, content) = with_doc(&raw, true, |doc| {
            let file = find(doc.root(), "FileAttachment")
                .ok_or("This is an attached message, not a file — such attachments are not supported yet")?;
            Ok((child_text(file, "Name"), child_text(file, "Content")))
        })?;
        // The server normally returns Base64 as one line; copy only when it does not
        let content = if content.contains(char::is_whitespace) {
            content.split_whitespace().collect()
        } else {
            content
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(content)
            .map_err(|e| format!("The server sent a corrupted attachment: {e}"))?;
        Ok((name, bytes))
    }

    /// Sends a message and keeps a copy in Sent Items. For a reply or a forward the server
    /// adds the quoted original and its attachments itself.
    pub async fn send(&self, outgoing: &Outgoing) -> Result<()> {
        if outgoing.to.is_empty() {
            return Err("Add at least one recipient".to_string());
        }
        let subject = format!("<t:Subject>{}</t:Subject>", escape(&outgoing.subject));
        let recipients = format!(
            "{}{}",
            recipients_xml("ToRecipients", &outgoing.to),
            recipients_xml("CcRecipients", &outgoing.cc)
        );
        let body = escape(&outgoing.body_html);
        let item = match (outgoing.mode.as_str(), &outgoing.reference_id) {
            ("new", _) => format!(
                r#"<t:Message>{subject}<t:Body BodyType="HTML">{body}</t:Body>{recipients}</t:Message>"#
            ),
            (mode @ ("reply" | "replyAll" | "forward"), Some(reference_id)) => {
                let tag = match mode {
                    "reply" => "ReplyToItem",
                    "replyAll" => "ReplyAllToItem",
                    _ => "ForwardItem",
                };
                format!(
                    r#"<t:{tag}>{subject}{recipients}<t:ReferenceItemId Id="{}"/><t:NewBodyContent BodyType="HTML">{body}</t:NewBodyContent></t:{tag}>"#,
                    escape(reference_id)
                )
            }
            _ => return Err("Unknown message type".to_string()),
        };
        if outgoing.attachments.is_empty() {
            let raw = self
                .call(&format!(
                    r#"<m:CreateItem MessageDisposition="SendAndSaveCopy"><m:SavedItemFolderId><t:DistinguishedFolderId Id="sentitems"/></m:SavedItemFolderId><m:Items>{item}</m:Items></m:CreateItem>"#
                ))
                .await?;
            return with_doc(&raw, true, |_| Ok(()));
        }

        // With files a message is sent in three steps: draft, attachments, send
        let files = attachments_xml(&outgoing.attachments)?;
        let raw = self
            .call(&format!(
                r#"<m:CreateItem MessageDisposition="SaveOnly"><m:SavedItemFolderId><t:DistinguishedFolderId Id="drafts"/></m:SavedItemFolderId><m:Items>{item}</m:Items></m:CreateItem>"#
            ))
            .await?;
        let draft_id = with_doc(&raw, true, |doc| {
            find(doc.root(), "ItemId")
                .and_then(|n| n.attribute("Id"))
                .map(str::to_string)
                .ok_or_else(|| "The server did not return the draft id".to_string())
        })?;
        let in_drafts = |error: String| format!("{error}. The message was not sent and is still in Drafts");
        let raw = self
            .call(&format!(
                r#"<m:CreateAttachment><m:ParentItemId Id="{}"/><m:Attachments>{files}</m:Attachments></m:CreateAttachment>"#,
                escape(&draft_id)
            ))
            .await
            .map_err(in_drafts)?;
        // Adding attachments gives the draft a new version; the server will not send it without that key
        let change_key = with_doc(&raw, true, |doc| {
            doc.descendants()
                .filter_map(|n| n.attribute("RootItemChangeKey"))
                .last()
                .map(str::to_string)
                .ok_or_else(|| "The server did not confirm the attachments".to_string())
        })
        .map_err(in_drafts)?;
        let raw = self
            .call(&format!(
                r#"<m:SendItem SaveItemToFolder="true"><m:ItemIds><t:ItemId Id="{}" ChangeKey="{}"/></m:ItemIds><m:SavedItemFolderId><t:DistinguishedFolderId Id="sentitems"/></m:SavedItemFolderId></m:SendItem>"#,
                escape(&draft_id),
                escape(&change_key)
            ))
            .await
            .map_err(in_drafts)?;
        with_doc(&raw, true, |_| Ok(())).map_err(in_drafts)
    }

    /// Ids of the mailbox root and the standard folders; the tree and its order are built from them.
    async fn well_known_folders(&self) -> Result<HashMap<String, String>> {
        const NAMES: [(&str, &str); 7] = [
            ("msgfolderroot", "root"),
            ("inbox", "inbox"),
            ("drafts", "drafts"),
            ("sentitems", "sent"),
            ("outbox", "outbox"),
            ("deleteditems", "trash"),
            ("junkemail", "junk"),
        ];
        let ids: String = NAMES
            .iter()
            .map(|(name, _)| format!(r#"<t:DistinguishedFolderId Id="{name}"/>"#))
            .collect();
        let raw = self
            .call(&format!(
                r#"<m:GetFolder><m:FolderShape><t:BaseShape>IdOnly</t:BaseShape></m:FolderShape><m:FolderIds>{ids}</m:FolderIds></m:GetFolder>"#
            ))
            .await?;
        // Not strict: one missing standard folder must not break the whole list
        with_doc(&raw, false, |doc| {
            let mut map = HashMap::new();
            let responses = doc
                .descendants()
                .filter(|n| n.tag_name().name() == "GetFolderResponseMessage");
            for (response, (_, kind)) in responses.zip(NAMES) {
                if let Some(id) = find(response, "FolderId").and_then(|n| n.attribute("Id")) {
                    map.insert(id.to_string(), kind.to_string());
                }
            }
            Ok(map)
        })
    }

    async fn embed_inline_images(&self, message: &mut Message) -> Result<()> {
        let wanted: Vec<&Attachment> = message
            .attachments
            .iter()
            .filter(|a| {
                a.inline
                    && !a.content_id.is_empty()
                    && a.size <= MAX_INLINE_IMAGE_BYTES
                    && message.body.contains(&format!("cid:{}", a.content_id))
            })
            .take(MAX_INLINE_IMAGES)
            .collect();
        if wanted.is_empty() {
            return Ok(());
        }
        let ids: String = wanted
            .iter()
            .map(|a| format!(r#"<t:AttachmentId Id="{}"/>"#, escape(&a.id)))
            .collect();
        let raw = self
            .call(&format!(
                r#"<m:GetAttachment><m:AttachmentIds>{ids}</m:AttachmentIds></m:GetAttachment>"#
            ))
            .await?;
        let images = with_doc(&raw, false, |doc| {
            Ok(doc
                .descendants()
                .filter(|n| n.tag_name().name() == "FileAttachment")
                .filter_map(|n| {
                    let content_id = child_text(n, "ContentId");
                    let content_type = child_text(n, "ContentType");
                    let content = child_text(n, "Content");
                    (!content_id.is_empty() && content_type.starts_with("image/") && !content.is_empty())
                        .then(|| (content_id, format!("data:{content_type};base64,{content}")))
                })
                .collect::<Vec<_>>())
        })?;
        for (content_id, data_uri) in images {
            message.body = message.body.replace(&format!("cid:{content_id}"), &data_uri);
        }
        Ok(())
    }

    async fn call(&self, body: &str) -> Result<String> {
        let envelope = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/" xmlns:t="http://schemas.microsoft.com/exchange/services/2006/types" xmlns:m="http://schemas.microsoft.com/exchange/services/2006/messages"><soap:Header><t:RequestServerVersion Version="Exchange2013"/></soap:Header><soap:Body>{body}</soap:Body></soap:Envelope>"#
        );
        let response = self
            .http
            .post(&self.url)
            .basic_auth(&self.username, Some(&self.password))
            .header("Content-Type", "text/xml; charset=utf-8")
            .body(envelope)
            .send()
            .await
            .map_err(|e| format!("Cannot reach the server: {e}"))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| format!("Could not read the server response: {e}"))?;
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(AUTH_ERROR.to_string());
        }
        if !status.is_success() {
            let fault = with_doc(&text, false, |doc| {
                Ok(find(doc.root(), "faultstring").map(text_of).unwrap_or_default())
            })
            .unwrap_or_default();
            return Err(format!("Server error (HTTP {}) {fault}", status.as_u16()));
        }
        Ok(text)
    }
}

struct RawFolder {
    id: String,
    parent_id: String,
    name: String,
    class: String,
    total: u32,
    unread: u32,
}

fn parse_folders(doc: &Document) -> Vec<RawFolder> {
    let Some(folders) = find(doc.root(), "Folders") else {
        return Vec::new();
    };
    folders
        .children()
        .filter(|n| n.is_element())
        .map(|n| RawFolder {
            id: child_attr(n, "FolderId", "Id"),
            parent_id: child_attr(n, "ParentFolderId", "Id"),
            name: child_text(n, "DisplayName"),
            class: child_text(n, "FolderClass"),
            total: child_text(n, "TotalCount").parse().unwrap_or(0),
            unread: child_text(n, "UnreadCount").parse().unwrap_or(0),
        })
        .collect()
}

/// Keeps mail folders only and lays them out as a tree:
/// standard ones first (inbox, drafts…), then the rest alphabetically.
fn build_folder_tree(raw: Vec<RawFolder>, well_known: &HashMap<String, String>) -> Vec<Folder> {
    let raw: Vec<RawFolder> = raw
        .into_iter()
        .filter(|f| f.class.starts_with("IPF.Note"))
        .collect();
    let kind_of = |f: &RawFolder| well_known.get(&f.id).cloned().unwrap_or_default();
    let rank = |f: &RawFolder| match kind_of(f).as_str() {
        "inbox" => 0,
        "drafts" => 1,
        "sent" => 2,
        "outbox" => 3,
        "trash" => 5,
        "junk" => 6,
        _ => 4,
    };

    let mut children: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, folder) in raw.iter().enumerate() {
        children.entry(folder.parent_id.as_str()).or_default().push(index);
    }
    for list in children.values_mut() {
        list.sort_by_key(|&i| (rank(&raw[i]), raw[i].name.to_lowercase()));
    }

    let root_id = well_known
        .iter()
        .find(|(_, kind)| kind.as_str() == "root")
        .map(|(id, _)| id.as_str())
        .unwrap_or_default();

    let mut out = Vec::with_capacity(raw.len());
    let mut seen = HashSet::new();
    let mut stack: Vec<(usize, u32)> = children
        .get(root_id)
        .map(|list| list.iter().rev().map(|&i| (i, 0)).collect())
        .unwrap_or_default();
    while let Some((index, depth)) = stack.pop() {
        if !seen.insert(index) {
            continue;
        }
        let folder = &raw[index];
        out.push(to_folder(folder, kind_of(folder), depth));
        if let Some(list) = children.get(folder.id.as_str()) {
            stack.extend(list.iter().rev().map(|&i| (i, depth + 1)));
        }
    }
    // Folders whose parent is not a mail folder or is unknown go to the top level
    for (index, folder) in raw.iter().enumerate() {
        if !seen.contains(&index) {
            out.push(to_folder(folder, kind_of(folder), 0));
        }
    }
    out
}

fn to_folder(raw: &RawFolder, kind: String, depth: u32) -> Folder {
    // The server names standard folders in the mailbox language; the app shows them in English.
    // Folders the user created keep their own names.
    let name = match kind.as_str() {
        "inbox" => "Inbox",
        "drafts" => "Drafts",
        "sent" => "Sent Items",
        "outbox" => "Outbox",
        "trash" => "Deleted Items",
        "junk" => "Junk Email",
        _ => raw.name.as_str(),
    };
    Folder {
        id: raw.id.clone(),
        name: name.to_string(),
        kind,
        depth,
        total: raw.total,
        unread: raw.unread,
    }
}

fn parse_message_page(doc: &Document) -> Result<MessagePage> {
    let root = find(doc.root(), "RootFolder").ok_or("The server response has no message list")?;
    let items = child(root, "Items")
        .map(|items| {
            items
                .children()
                .filter(|n| n.is_element())
                .map(|n| MessageSummary {
                    id: child_attr(n, "ItemId", "Id"),
                    change_key: child_attr(n, "ItemId", "ChangeKey"),
                    subject: child_text(n, "Subject"),
                    from: child(n, "From")
                        .and_then(|from| find(from, "Name"))
                        .map(text_of)
                        .unwrap_or_default(),
                    to: child_text(n, "DisplayTo"),
                    date: child_text(n, "DateTimeReceived"),
                    preview: child_text(n, "Preview"),
                    is_read: child_text(n, "IsRead") != "false",
                    has_attachments: child_text(n, "HasAttachments") == "true",
                    is_meeting: n.tag_name().name().starts_with("Meeting"),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(MessagePage {
        items,
        total: root
            .attribute("TotalItemsInView")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        last: root.attribute("IncludesLastItemInRange") != Some("false"),
    })
}

fn parse_message(doc: &Document) -> Result<Message> {
    let item = find(doc.root(), "Items")
        .and_then(|items| items.children().find(|n| n.is_element()))
        .ok_or("The server did not return the message")?;
    let body = child(item, "Body");
    let attachments = child(item, "Attachments")
        .map(|list| {
            list.children()
                .filter(|n| n.is_element())
                .map(|n| Attachment {
                    id: child_attr(n, "AttachmentId", "Id"),
                    name: child_text(n, "Name"),
                    content_type: child_text(n, "ContentType"),
                    size: child_text(n, "Size").parse().unwrap_or(0),
                    inline: child_text(n, "IsInline") == "true",
                    content_id: child_text(n, "ContentId"),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Message {
        id: child_attr(item, "ItemId", "Id"),
        change_key: child_attr(item, "ItemId", "ChangeKey"),
        subject: child_text(item, "Subject"),
        from: child(item, "From").and_then(|n| mailboxes(n).into_iter().next()),
        to: child(item, "ToRecipients").map(mailboxes).unwrap_or_default(),
        cc: child(item, "CcRecipients").map(mailboxes).unwrap_or_default(),
        date: child_text(item, "DateTimeReceived"),
        received_by: child(item, "ReceivedBy")
            .and_then(|n| mailboxes(n).into_iter().next())
            .map(|a| a.email)
            .unwrap_or_default(),
        is_read: child_text(item, "IsRead") != "false",
        is_html: body.and_then(|b| b.attribute("BodyType")) == Some("HTML"),
        body: body.map(text_of).unwrap_or_default(),
        attachments,
        meeting: None,
        item_type: item.tag_name().name().to_string(),
    })
}

fn parse_meeting(doc: &Document, is_request: bool) -> Result<Meeting> {
    let item = find(doc.root(), "Items")
        .and_then(|items| items.children().find(|n| n.is_element()))
        .ok_or("The server did not return the meeting details")?;
    Ok(Meeting {
        is_request,
        start: child_text(item, "Start"),
        end: child_text(item, "End"),
        all_day: child_text(item, "IsAllDayEvent") == "true",
        location: child_text(item, "Location"),
        organizer: child(item, "Organizer")
            .and_then(|n| mailboxes(n).into_iter().next())
            .map(|a| if a.name.is_empty() { a.email } else { a.name })
            .unwrap_or_default(),
        response: child_text(item, "ResponseType"),
        out_of_date: child_text(item, "IsOutOfDate") == "true",
    })
}

fn mailboxes(node: Node) -> Vec<Address> {
    node.children()
        .filter(|n| n.tag_name().name() == "Mailbox")
        .map(|n| Address {
            name: child_text(n, "Name"),
            email: child_text(n, "EmailAddress"),
        })
        .collect()
}

fn is_read_restriction(read: bool) -> String {
    format!(
        r#"<m:Restriction><t:IsEqualTo><t:FieldURI FieldURI="message:IsRead"/><t:FieldURIOrConstant><t:Constant Value="{read}"/></t:FieldURIOrConstant></t:IsEqualTo></m:Restriction>"#
    )
}

fn recipients_xml(tag: &str, addresses: &[String]) -> String {
    if addresses.is_empty() {
        return String::new();
    }
    let mailboxes: String = addresses
        .iter()
        .map(|a| format!("<t:Mailbox><t:EmailAddress>{}</t:EmailAddress></t:Mailbox>", escape(a)))
        .collect();
    format!("<t:{tag}>{mailboxes}</t:{tag}>")
}

fn attachments_xml(attachments: &[OutgoingAttachment]) -> Result<String> {
    let mut xml = String::new();
    for attachment in attachments {
        let is_base64 = attachment
            .content_base64
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='));
        if !is_base64 {
            return Err(format!("Could not read the file \"{}\"", attachment.name));
        }
        xml.push_str(&format!(
            "<t:FileAttachment><t:Name>{}</t:Name><t:Content>{}</t:Content></t:FileAttachment>",
            escape(&attachment.name),
            attachment.content_base64
        ));
    }
    Ok(xml)
}

fn with_doc<T>(raw: &str, strict: bool, f: impl FnOnce(&Document) -> Result<T>) -> Result<T> {
    let xml = strip_invalid_char_refs(raw);
    let doc = Document::parse(&xml)
        .map_err(|e| format!("Could not parse the server response: {e}"))?;
    if strict {
        if let Some(error) = response_error(&doc) {
            return Err(error);
        }
    }
    f(&doc)
}

fn response_error(doc: &Document) -> Option<String> {
    let error = doc
        .descendants()
        .find(|n| n.attribute("ResponseClass") == Some("Error"))?;
    Some(format!(
        "The server returned an error: {} ({})",
        child_text(error, "MessageText"),
        child_text(error, "ResponseCode")
    ))
}

fn parse_resolved_names(doc: &Document) -> Result<Vec<Address>> {
    // The server reports "nobody found" as an error, though for suggestions it is just an empty result
    if find(doc.root(), "ResponseCode").map(text_of).as_deref() == Some("ErrorNameResolutionNoResults") {
        return Ok(Vec::new());
    }
    if let Some(error) = response_error(doc) {
        return Err(error);
    }
    Ok(doc
        .descendants()
        .filter(|n| n.tag_name().name() == "Mailbox")
        .map(|n| Address {
            name: child_text(n, "Name"),
            email: child_text(n, "EmailAddress"),
        })
        // Distribution lists and some address book entries have no regular address and cannot be mailed
        .filter(|a| a.email.contains('@'))
        .collect())
}

fn find<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    node.descendants()
        .find(|n| n.is_element() && n.tag_name().name() == name)
}

fn child<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    node.children()
        .find(|n| n.is_element() && n.tag_name().name() == name)
}

fn text_of(node: Node) -> String {
    node.children().filter_map(|n| n.text()).collect()
}

fn child_text(node: Node, name: &str) -> String {
    child(node, name).map(text_of).unwrap_or_default()
}

fn child_attr(node: Node, name: &str, attr: &str) -> String {
    child(node, name)
        .and_then(|n| n.attribute(attr))
        .unwrap_or_default()
        .to_string()
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            // Control characters (from pasted text, say) are not allowed in XML 1.0;
            // the server would reject the whole request
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
    out
}

/// Exchange sometimes puts references to control characters (`&#x1F;` and the like) into
/// message bodies. They are invalid in XML 1.0 and break parsing of the whole response.
fn strip_invalid_char_refs(xml: &str) -> Cow<'_, str> {
    if !xml.contains("&#") {
        return Cow::Borrowed(xml);
    }
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(pos) = rest.find("&#") {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        match tail.bytes().take(12).position(|b| b == b';') {
            Some(end) => {
                let number = &tail[2..end];
                let code = match number.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => number.parse().ok(),
                };
                let valid = matches!(
                    code,
                    Some(0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF)
                );
                if valid {
                    out.push_str(&tail[..=end]);
                }
                rest = &tail[end + 1..];
            }
            None => {
                out.push_str("&#");
                rest = &tail[2..];
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_only_invalid_char_refs() {
        assert_eq!(strip_invalid_char_refs("a&#x1F;b&#10;c&#x41;"), "ab&#10;c&#x41;");
        assert_eq!(strip_invalid_char_refs("café &# naïve"), "café &# naïve");
    }

    #[test]
    fn builds_tree_with_standard_folders_first() {
        let raw = |id: &str, parent: &str, name: &str, class: &str| RawFolder {
            id: id.into(),
            parent_id: parent.into(),
            name: name.into(),
            class: class.into(),
            total: 0,
            unread: 0,
        };
        let folders = vec![
            raw("a", "root", "Archive", "IPF.Note"),
            raw("cal", "root", "Calendar", "IPF.Appointment"),
            raw("in", "root", "Posteingang", "IPF.Note"),
            raw("sub", "in", "Project", "IPF.Note"),
            raw("lost", "cal", "Orphan", "IPF.Note"),
        ];
        let well_known = HashMap::from([
            ("root".to_string(), "root".to_string()),
            ("in".to_string(), "inbox".to_string()),
        ]);
        let tree = build_folder_tree(folders, &well_known);
        let shape: Vec<(&str, u32)> = tree.iter().map(|f| (f.id.as_str(), f.depth)).collect();
        assert_eq!(shape, [("in", 0), ("sub", 1), ("a", 0), ("lost", 0)]);
        // A standard folder is shown in English whatever the mailbox language; others keep their names
        let names: Vec<&str> = tree.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Inbox", "Project", "Archive", "Orphan"]);
    }

    #[test]
    fn parses_message_page() {
        let xml = r#"<Envelope><RootFolder TotalItemsInView="7" IncludesLastItemInRange="false"><Items>
            <Message><ItemId Id="i1" ChangeKey="k1"/><Subject>Subject</Subject><From><Mailbox><Name>Ivan</Name></Mailbox></From><IsRead>false</IsRead><HasAttachments>true</HasAttachments></Message>
            <MeetingRequest><ItemId Id="i2" ChangeKey="k2"/><Subject>Meeting</Subject></MeetingRequest>
        </Items></RootFolder></Envelope>"#;
        let page = with_doc(xml, true, parse_message_page).unwrap();
        assert_eq!((page.total, page.last, page.items.len()), (7, false, 2));
        assert!(!page.items[0].is_meeting && page.items[1].is_meeting);
        let item = &page.items[0];
        assert_eq!((item.id.as_str(), item.from.as_str()), ("i1", "Ivan"));
        assert!(!item.is_read && item.has_attachments);
    }

    #[test]
    fn escapes_markup_and_drops_control_chars() {
        assert_eq!(escape("a<b>&\"c\"\u{b}\td\n"), "a&lt;b&gt;&amp;&quot;c&quot;\td\n");
    }

    #[test]
    fn builds_recipients() {
        assert_eq!(recipients_xml("CcRecipients", &[]), "");
        assert_eq!(
            recipients_xml("ToRecipients", &["a&b@x.ru".to_string()]),
            "<t:ToRecipients><t:Mailbox><t:EmailAddress>a&amp;b@x.ru</t:EmailAddress></t:Mailbox></t:ToRecipients>"
        );
    }

    #[test]
    fn builds_attachments_and_rejects_non_base64() {
        let file = |content: &str| OutgoingAttachment {
            name: "a<b>.txt".into(),
            content_base64: content.into(),
        };
        assert_eq!(
            attachments_xml(&[file("0KLQtdGB0YI=")]).unwrap(),
            "<t:FileAttachment><t:Name>a&lt;b&gt;.txt</t:Name><t:Content>0KLQtdGB0YI=</t:Content></t:FileAttachment>"
        );
        assert!(attachments_xml(&[file("</t:Content>")]).is_err());
    }

    #[test]
    fn parses_meeting_request() {
        let xml = r#"<r><Items><MeetingRequest><ItemId Id="i"/><Subject>Standup</Subject>
            <Start>2026-10-12T07:00:00Z</Start><End>2026-10-12T08:00:00Z</End><Location>Room 3</Location>
            <Organizer><Mailbox><Name>Ivan</Name><EmailAddress>ivan@x.ru</EmailAddress></Mailbox></Organizer>
            <ResponseType>NoResponseReceived</ResponseType><IsOutOfDate>false</IsOutOfDate></MeetingRequest></Items></r>"#;
        let message = with_doc(xml, true, parse_message).unwrap();
        assert_eq!(message.item_type, "MeetingRequest");
        let meeting = with_doc(xml, true, |doc| parse_meeting(doc, true)).unwrap();
        assert_eq!(
            (meeting.start.as_str(), meeting.location.as_str(), meeting.organizer.as_str()),
            ("2026-10-12T07:00:00Z", "Room 3", "Ivan")
        );
        assert!(meeting.is_request && !meeting.all_day && !meeting.out_of_date);
    }

    #[test]
    fn parses_resolved_names() {
        let found = r#"<r><m ResponseClass="Warning"><ResponseCode>ErrorNameResolutionMultipleResults</ResponseCode><ResolutionSet>
            <Resolution><Mailbox><Name>Ivan Ivanov</Name><EmailAddress>ivanov@x.ru</EmailAddress></Mailbox></Resolution>
            <Resolution><Mailbox><Name>All staff</Name><EmailAddress>/o=Org/cn=all</EmailAddress></Mailbox></Resolution>
        </ResolutionSet></m></r>"#;
        let people = with_doc(found, false, parse_resolved_names).unwrap();
        assert_eq!(people.len(), 1);
        assert_eq!((people[0].name.as_str(), people[0].email.as_str()), ("Ivan Ivanov", "ivanov@x.ru"));

        let nobody = r#"<r><m ResponseClass="Error"><MessageText>None</MessageText><ResponseCode>ErrorNameResolutionNoResults</ResponseCode></m></r>"#;
        assert!(with_doc(nobody, false, parse_resolved_names).unwrap().is_empty());

        let denied = r#"<r><m ResponseClass="Error"><MessageText>Access denied</MessageText><ResponseCode>ErrorAccessDenied</ResponseCode></m></r>"#;
        assert!(with_doc(denied, false, parse_resolved_names).is_err());
    }

    #[test]
    fn reports_server_error() {
        let xml = r#"<r><m ResponseClass="Error"><MessageText>Access denied</MessageText><ResponseCode>ErrorAccessDenied</ResponseCode></m></r>"#;
        let error = with_doc(xml, true, |_| Ok(())).unwrap_err();
        assert!(error.contains("Access denied") && error.contains("ErrorAccessDenied"));
    }
}
