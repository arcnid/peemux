use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

// ─── public types ──────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Contact {
    pub display_name: String,
    pub user_id: String,
    pub presence: Presence,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Presence {
    Available,
    Busy,
    DoNotDisturb,
    Away,
    BeRightBack,
    Offline,
    Unknown,
}

impl Presence {
    fn from_str(s: &str) -> Self {
        match s {
            "Available" => Self::Available,
            "Busy" => Self::Busy,
            "DoNotDisturb" => Self::DoNotDisturb,
            "Away" => Self::Away,
            "BeRightBack" => Self::BeRightBack,
            "Offline" | "PresenceUnknown" => Self::Offline,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChatMessage {
    pub chat_id: String,
    pub sender_name: String,
    pub body: String,
    pub created: String,
}

/// Events sent from the Teams background poller to the main TUI thread.
pub enum TeamsEvent {
    ContactsUpdated(Vec<Contact>),
    NewMessage(ChatMessage),
    AuthRequired { uri: String, code: String },
    AuthComplete,
    Error(String),
}

// ─── config ────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct TeamsConfig {
    pub client_id: String,
    pub tenant_id: String,
    pub poll_interval: Duration,
}

impl TeamsConfig {
    pub fn from_file() -> Option<Self> {
        let path = dirs_config_path()?.join("config.toml");
        let text = fs::read_to_string(path).ok()?;
        let doc: toml::Value = text.parse().ok()?;
        let teams = doc.get("teams")?;
        let client_id = teams.get("client_id")?.as_str()?.to_string();
        let tenant_id = teams.get("tenant_id")?.as_str()?.to_string();
        let poll_secs = teams
            .get("poll_interval_secs")
            .and_then(|v| v.as_integer())
            .unwrap_or(30) as u64;
        if client_id.is_empty() || tenant_id.is_empty() {
            return None;
        }
        Some(TeamsConfig {
            client_id,
            tenant_id,
            poll_interval: Duration::from_secs(poll_secs),
        })
    }
}

fn dirs_config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "peemux").map(|d| d.config_dir().to_path_buf())
}

// ─── token store ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
struct TokenStore {
    access_token: String,
    refresh_token: String,
    expires_at: u64,
}

impl TokenStore {
    fn path() -> Option<PathBuf> {
        dirs_config_path().map(|p| p.join("teams-token.json"))
    }

    fn load() -> Option<Self> {
        let path = Self::path()?;
        let text = fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn save(&self) -> Result<()> {
        let path = Self::path().ok_or_else(|| anyhow!("no config dir"))?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        fs::write(&path, &json)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    fn is_expired(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now >= self.expires_at.saturating_sub(300) // 5-min buffer
    }
}

// ─── Graph API response types ──────────────────────────────────────────────

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

#[derive(Deserialize)]
struct TokenErrorResponse {
    error: String,
}

#[derive(Deserialize)]
struct GraphPresenceResponse {
    value: Vec<GraphPresenceEntry>,
}

#[derive(Deserialize)]
struct GraphPresenceEntry {
    id: String,
    availability: Option<String>,
}

#[derive(Deserialize)]
struct GraphPeopleResponse {
    value: Vec<GraphPerson>,
}

#[derive(Deserialize)]
struct GraphPerson {
    id: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct GraphChatsResponse {
    value: Vec<GraphChat>,
}

#[derive(Deserialize)]
struct GraphChat {
    id: String,
    #[serde(rename = "lastMessagePreview")]
    last_message_preview: Option<GraphMessagePreview>,
}

#[derive(Deserialize)]
struct GraphMessagePreview {
    body: Option<GraphMessageBody>,
    from: Option<GraphMessageFrom>,
    #[serde(rename = "createdDateTime")]
    created_date_time: Option<String>,
}

#[derive(Deserialize)]
struct GraphMessageBody {
    content: Option<String>,
}

#[derive(Deserialize)]
struct GraphMessageFrom {
    user: Option<GraphUser>,
}

#[derive(Deserialize)]
struct GraphUser {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct GraphMessagesResponse {
    value: Vec<GraphMessage>,
}

#[derive(Deserialize)]
struct GraphMessage {
    id: String,
    body: Option<GraphMessageBody>,
    from: Option<GraphMessageFrom>,
    #[serde(rename = "createdDateTime")]
    created_date_time: Option<String>,
    #[serde(rename = "chatId")]
    chat_id: Option<String>,
}

// ─── Teams client ──────────────────────────────────────────────────────────

const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";
const SCOPES: &str = "User.Read Presence.Read.All Chat.ReadWrite People.Read offline_access";

pub struct TeamsClient {
    config: TeamsConfig,
    http: reqwest::Client,
    token: Option<TokenStore>,
    last_check: Option<String>, // ISO timestamp of last message check
}

impl TeamsClient {
    pub fn new(config: TeamsConfig) -> Self {
        TeamsClient {
            config,
            http: reqwest::Client::new(),
            token: TokenStore::load(),
            last_check: None,
        }
    }

    fn auth_url(&self, path: &str) -> String {
        format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/{}",
            self.config.tenant_id, path
        )
    }

    pub fn is_authenticated(&self) -> bool {
        self.token.as_ref().map(|t| !t.is_expired()).unwrap_or(false)
    }

    /// Start device code flow. Returns (verification_uri, user_code).
    pub async fn start_device_code(&self) -> Result<(String, String, String, u64)> {
        let resp = self
            .http
            .post(self.auth_url("devicecode"))
            .form(&[
                ("client_id", self.config.client_id.as_str()),
                ("scope", SCOPES),
            ])
            .send()
            .await?
            .json::<DeviceCodeResponse>()
            .await?;
        Ok((
            resp.verification_uri,
            resp.user_code,
            resp.device_code,
            resp.interval.unwrap_or(5),
        ))
    }

    /// Poll for token after device code flow initiated. Blocks until auth completes.
    pub async fn poll_for_token(&mut self, device_code: &str, interval: u64) -> Result<()> {
        loop {
            tokio::time::sleep(Duration::from_secs(interval)).await;
            let resp = self
                .http
                .post(self.auth_url("token"))
                .form(&[
                    ("client_id", self.config.client_id.as_str()),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("device_code", device_code),
                ])
                .send()
                .await?;

            let status = resp.status();
            let body = resp.text().await?;

            if status.is_success() {
                let token_resp: TokenResponse = serde_json::from_str(&body)?;
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let store = TokenStore {
                    access_token: token_resp.access_token,
                    refresh_token: token_resp.refresh_token.unwrap_or_default(),
                    expires_at: now + token_resp.expires_in,
                };
                store.save()?;
                self.token = Some(store);
                return Ok(());
            }

            let err: TokenErrorResponse = serde_json::from_str(&body).unwrap_or(TokenErrorResponse {
                error: "unknown".into(),
            });
            match err.error.as_str() {
                "authorization_pending" => continue,
                "slow_down" => {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
                _ => return Err(anyhow!("device code auth failed: {}", err.error)),
            }
        }
    }

    /// Refresh the access token using the stored refresh token.
    pub async fn refresh_token(&mut self) -> Result<()> {
        let refresh = self
            .token
            .as_ref()
            .map(|t| t.refresh_token.clone())
            .unwrap_or_default();
        if refresh.is_empty() {
            return Err(anyhow!("no refresh token"));
        }
        let resp = self
            .http
            .post(self.auth_url("token"))
            .form(&[
                ("client_id", self.config.client_id.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh),
                ("scope", SCOPES),
            ])
            .send()
            .await?
            .json::<TokenResponse>()
            .await?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let store = TokenStore {
            access_token: resp.access_token,
            refresh_token: resp.refresh_token.unwrap_or(refresh),
            expires_at: now + resp.expires_in,
        };
        store.save()?;
        self.token = Some(store);
        Ok(())
    }

    async fn ensure_token(&mut self) -> Result<String> {
        if let Some(ref t) = self.token {
            if !t.is_expired() {
                return Ok(t.access_token.clone());
            }
        }
        self.refresh_token().await?;
        self.token
            .as_ref()
            .map(|t| t.access_token.clone())
            .ok_or_else(|| anyhow!("no token after refresh"))
    }

    // ─── Graph API calls ───────────────────────────────────────────────────

    /// Get relevant people (contacts) for the signed-in user.
    pub async fn get_people(&mut self) -> Result<Vec<(String, String)>> {
        let token = self.ensure_token().await?;
        let resp = self
            .http
            .get(format!("{GRAPH_BASE}/me/people?$top=20"))
            .bearer_auth(&token)
            .send()
            .await?
            .json::<GraphPeopleResponse>()
            .await?;
        Ok(resp
            .value
            .into_iter()
            .filter_map(|p| {
                Some((p.id?, p.display_name.unwrap_or_else(|| "?".into())))
            })
            .collect())
    }

    /// Get presence for a batch of user IDs.
    pub async fn get_presences(&mut self, user_ids: &[String]) -> Result<Vec<(String, Presence)>> {
        if user_ids.is_empty() {
            return Ok(vec![]);
        }
        let token = self.ensure_token().await?;
        let body = serde_json::json!({ "ids": user_ids });
        let resp = self
            .http
            .post(format!("{GRAPH_BASE}/communications/getPresencesByUserId"))
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?
            .json::<GraphPresenceResponse>()
            .await?;
        Ok(resp
            .value
            .into_iter()
            .map(|p| {
                let presence = Presence::from_str(p.availability.as_deref().unwrap_or("Unknown"));
                (p.id, presence)
            })
            .collect())
    }

    /// Get recent chats and check for new messages since last check.
    pub async fn check_new_messages(&mut self) -> Result<Vec<ChatMessage>> {
        let token = self.ensure_token().await?;
        let resp = self
            .http
            .get(format!(
                "{GRAPH_BASE}/me/chats?$expand=lastMessagePreview&$top=10&$orderby=lastMessagePreview/createdDateTime desc"
            ))
            .bearer_auth(&token)
            .send()
            .await?
            .json::<GraphChatsResponse>()
            .await?;

        let mut new_msgs = Vec::new();
        for chat in resp.value {
            if let Some(preview) = chat.last_message_preview {
                let created = preview.created_date_time.unwrap_or_default();
                if let Some(ref last) = self.last_check {
                    if created <= *last {
                        continue;
                    }
                }
                let sender = preview
                    .from
                    .and_then(|f| f.user)
                    .and_then(|u| u.display_name)
                    .unwrap_or_else(|| "Unknown".into());
                let body = preview
                    .body
                    .and_then(|b| b.content)
                    .unwrap_or_default();
                // Strip HTML tags from message body (Graph returns HTML).
                let body = strip_html(&body);
                new_msgs.push(ChatMessage {
                    chat_id: chat.id,
                    sender_name: sender,
                    body,
                    created: created.clone(),
                });
            }
        }
        // Update last_check to the newest message timestamp.
        if let Some(newest) = new_msgs.first() {
            self.last_check = Some(newest.created.clone());
        } else if self.last_check.is_none() {
            // First run — set to now so we don't replay old messages.
            let now = chrono_now_iso();
            self.last_check = Some(now);
        }
        Ok(new_msgs)
    }

    /// Send a message to a chat.
    pub async fn send_message(&mut self, chat_id: &str, text: &str) -> Result<()> {
        let token = self.ensure_token().await?;
        let body = serde_json::json!({
            "body": {
                "content": text,
                "contentType": "text"
            }
        });
        let resp = self
            .http
            .post(format!("{GRAPH_BASE}/me/chats/{chat_id}/messages"))
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let err = resp.text().await.unwrap_or_default();
            return Err(anyhow!("send failed: {err}"));
        }
        Ok(())
    }
}

// ─── background poller ─────────────────────────────────────────────────────

/// Spawn the Teams background poller on a tokio runtime. Returns the event
/// receiver that the main TUI loop drains.
pub fn spawn_poller(config: TeamsConfig) -> mpsc::Receiver<TeamsEvent> {
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime for teams poller");

        rt.block_on(async move {
            let mut client = TeamsClient::new(config.clone());

            // If no stored token, kick off device code auth.
            if !client.is_authenticated() {
                match client.start_device_code().await {
                    Ok((uri, code, device_code, interval)) => {
                        let _ = tx.send(TeamsEvent::AuthRequired {
                            uri: uri.clone(),
                            code: code.clone(),
                        });
                        match client.poll_for_token(&device_code, interval).await {
                            Ok(()) => {
                                let _ = tx.send(TeamsEvent::AuthComplete);
                            }
                            Err(e) => {
                                let _ = tx.send(TeamsEvent::Error(format!("auth: {e}")));
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(TeamsEvent::Error(format!("device code: {e}")));
                        return;
                    }
                }
            } else {
                let _ = tx.send(TeamsEvent::AuthComplete);
            }

            // Main poll loop.
            loop {
                // Refresh token if needed.
                if let Some(ref t) = client.token {
                    if t.is_expired() {
                        if let Err(e) = client.refresh_token().await {
                            let _ = tx.send(TeamsEvent::Error(format!("refresh: {e}")));
                        }
                    }
                }

                // Fetch contacts + presence.
                match client.get_people().await {
                    Ok(people) => {
                        let ids: Vec<String> = people.iter().map(|(id, _)| id.clone()).collect();
                        let presences = client.get_presences(&ids).await.unwrap_or_default();
                        let contacts: Vec<Contact> = people
                            .into_iter()
                            .map(|(id, name)| {
                                let presence = presences
                                    .iter()
                                    .find(|(pid, _)| *pid == id)
                                    .map(|(_, p)| *p)
                                    .unwrap_or(Presence::Unknown);
                                Contact {
                                    display_name: name,
                                    user_id: id,
                                    presence,
                                }
                            })
                            .collect();
                        let _ = tx.send(TeamsEvent::ContactsUpdated(contacts));
                    }
                    Err(e) => {
                        let _ = tx.send(TeamsEvent::Error(format!("people: {e}")));
                    }
                }

                // Check for new messages.
                match client.check_new_messages().await {
                    Ok(msgs) => {
                        for msg in msgs {
                            let _ = tx.send(TeamsEvent::NewMessage(msg));
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(TeamsEvent::Error(format!("messages: {e}")));
                    }
                }

                tokio::time::sleep(config.poll_interval).await;
            }
        });
    });

    rx
}

// ─── helpers ───────────────────────────────────────────────────────────────

fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.trim().to_string()
}

fn chrono_now_iso() -> String {
    // Placeholder far-future timestamp — the first poll cycle will overwrite
    // this with the actual newest message timestamp, so we don't replay old
    // messages on first connect.
    "2099-01-01T00:00:00Z".to_string()
}
