use linkify::{LinkFinder, LinkKind};
use rusqlite::{params, Connection};
use serenity::{
    async_trait,
    builder::{CreateEmbed, CreateEmbedFooter, CreateMessage},
    model::{
        id::{ChannelId, GuildId, RoleId},
        prelude::*,
    },
    prelude::*,
};
use std::{
    collections::HashMap,
    env,
    error::Error,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::RwLock;
use tracing::{error, info};
use url::Url;

const LOG_CHANNEL_ID: u64 = 1488275200792461333;
const CATENARY_AUTHOR_ID: u64 = 1443445776297758760;

fn ignore_message_author(author_id: u64, is_webhook: bool) -> bool {
    is_webhook || author_id == CATENARY_AUTHOR_ID
}

fn ignore_message_delete(channel_id: ChannelId, author_id: u64, is_webhook: bool) -> bool {
    channel_id.get() == LOG_CHANNEL_ID || ignore_message_author(author_id, is_webhook)
}

// Discord can refresh signed attachment URLs without changing the attachments.
fn same_attachments(before: &[String], after: &[String]) -> bool {
    before.len() == after.len()
        && before.iter().zip(after).all(|(a, b)| {
            a.split('?').next() == b.split('?').next()
        })
}

fn has_message_edit(
    before: &StoredMessage,
    content: Option<&str>,
    attachments: Option<&[String]>,
) -> bool {
    content.is_some_and(|text| text != before.content.as_str())
        || attachments.is_some_and(|files| !same_attachments(&before.attachments, files))
}
const WARNING: &str = "x.com urls are prohibited on LA Metro";

fn is_prohibited_host(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    let host_lower = host.to_ascii_lowercase();

    host_lower == "x.com"
        || host_lower.ends_with(".x.com")
        || host_lower == "twitter.com"
        || host_lower.ends_with(".twitter.com")
}

fn contains_prohibited_link(content: &str) -> bool {
    let mut finder = LinkFinder::new();
    finder.url_must_have_scheme(false);
    finder.links(content).any(|link| {
        if link.kind() != &LinkKind::Url {
            return false;
        }
        let text = link.as_str();
        let candidate = if text.contains("://") {
            text.to_owned()
        } else {
            format!("https://{text}")
        };
        Url::parse(&candidate)
            .ok()
            .and_then(|url| url.host_str().map(is_prohibited_host))
            .unwrap_or(false)
    })
}

#[derive(Clone, Debug)]
struct TrackedInvite {
    code: String,
    uses: u64,
    inviter: String,
}

#[derive(Clone, Debug)]
struct StoredMessage {
    author_tag: String,
    author_id: u64,
    content: String,
    attachments: Vec<String>,
    // None denotes a legacy database record whose origin was not recorded.
    is_webhook: Option<bool>,
}

impl StoredMessage {
    fn from_message(message: &Message) -> Self {
        Self {
            author_tag: message.author.tag(),
            author_id: message.author.id.get(),
            content: message.content.clone(),
            attachments: message.attachments.iter().map(|a| a.url.clone()).collect(),
            is_webhook: Some(message.webhook_id.is_some()),
        }
    }
}

struct Handler {
    bot_id: AtomicU64,
    // Store guild invites to track who invited whom upon member join
    invites: Arc<RwLock<HashMap<GuildId, HashMap<String, TrackedInvite>>>>,
    // SQLite connection for disk storage of messages and file attachments
    db: Arc<Mutex<Connection>>,
}

impl Handler {
    fn new(db_conn: Connection) -> Self {
        Self {
            bot_id: AtomicU64::new(0),
            invites: Arc::new(RwLock::new(HashMap::new())),
            db: Arc::new(Mutex::new(db_conn)),
        }
    }

    fn init_db(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        let conn = self.db.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS stored_messages (
                message_id INTEGER PRIMARY KEY,
                channel_id INTEGER NOT NULL,
                author_tag TEXT NOT NULL,
                author_id INTEGER NOT NULL,
                content TEXT NOT NULL,
                attachments TEXT NOT NULL,
                created_at TEXT NOT NULL,
                is_webhook INTEGER
            )",
            [],
        )?;
        // Existing installations must retain their stored messages.
        let has_webhook_column = {
            let mut stmt = conn.prepare("PRAGMA table_info(stored_messages)")?;
            let names = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()?;
            names.iter().any(|name| name == "is_webhook")
        };
        if !has_webhook_column {
            conn.execute("ALTER TABLE stored_messages ADD COLUMN is_webhook INTEGER", [])?;
        }
        Ok(())
    }

    fn save_message_to_db(&self, msg: &Message) {
        let attachments_str = msg
            .attachments
            .iter()
            .map(|a| a.url.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        if let Ok(conn) = self.db.lock() {
            if let Err(e) = conn.execute(
                "INSERT OR REPLACE INTO stored_messages (message_id, channel_id, author_tag, author_id, content, attachments, created_at, is_webhook)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    msg.id.get() as i64,
                    msg.channel_id.get() as i64,
                    msg.author.tag(),
                    msg.author.id.get() as i64,
                    msg.content,
                    attachments_str,
                    chrono::Utc::now().to_rfc3339(),
                    msg.webhook_id.is_some()
                ],
            ) {
                error!(error = %e, "Failed to insert message into SQLite DB");
            }
        }
    }

    fn update_message_in_db(
        &self,
        message_id: MessageId,
        content: Option<&str>,
        attachments: Option<&[String]>,
        is_webhook: Option<bool>,
    ) {
        let attachments = attachments.map(|files| files.join("\n"));
        if let Ok(conn) = self.db.lock() {
            if let Err(e) = conn.execute(
                "UPDATE stored_messages
                 SET content = COALESCE(?1, content),
                     attachments = COALESCE(?2, attachments),
                     is_webhook = COALESCE(?3, is_webhook)
                 WHERE message_id = ?4",
                params![content, attachments, is_webhook, message_id.get() as i64],
            ) {
                error!(error = %e, "Failed to update message in SQLite DB");
            }
        }
    }

    fn fetch_message_from_db(&self, message_id: MessageId) -> Option<StoredMessage> {
        if let Ok(conn) = self.db.lock() {
            let mut stmt = conn
                .prepare("SELECT author_tag, author_id, content, attachments, is_webhook FROM stored_messages WHERE message_id = ?1")
                .ok()?;

            let result = stmt
                .query_row(params![message_id.get() as i64], |row| {
                    let author_tag: String = row.get(0)?;
                    let author_id: i64 = row.get(1)?;
                    let content: String = row.get(2)?;
                    let attachments_raw: String = row.get(3)?;

                    let attachments = if attachments_raw.is_empty() {
                        Vec::new()
                    } else {
                        attachments_raw.lines().map(|s| s.to_string()).collect()
                    };

                    Ok(StoredMessage {
                        author_tag,
                        author_id: author_id as u64,
                        content,
                        attachments,
                        is_webhook: row.get(4)?,
                    })
                })
                .ok();
            result
        } else {
            None
        }
    }

    async fn send_log_embed(&self, ctx: &Context, embed: CreateEmbed) {
        let channel = ChannelId::new(LOG_CHANNEL_ID);
        let builder = CreateMessage::new().embed(embed);
        if let Err(e) = channel.send_message(&ctx.http, builder).await {
            error!(error = %e, "Failed to send log embed");
        }
    }

    async fn moderate(&self, ctx: &Context, message: &Message) {
        if message.guild_id.is_none()
            || message.author.id.get() == self.bot_id.load(Ordering::Relaxed)
            || !contains_prohibited_link(&message.content)
        {
            return;
        }

        if let Err(cause) = message.delete(&ctx.http).await {
            error!(message_id = %message.id, channel_id = %message.channel_id,
                error = %cause, "Could not delete prohibited message");
            return;
        }
        if let Err(cause) = message.channel_id.say(&ctx.http, WARNING).await {
            error!(channel_id = %message.channel_id, error = %cause, "Could not send warning");
        }
    }

    async fn cache_guild_invites(&self, ctx: &Context, guild_id: GuildId) {
        if let Ok(guild_invites) = guild_id.invites(&ctx.http).await {
            let mut map = HashMap::new();
            for inv in guild_invites {
                let inviter_name = inv
                    .inviter
                    .as_ref()
                    .map(|u| u.tag())
                    .unwrap_or_else(|| "Unknown".to_string());
                map.insert(
                    inv.code.clone(),
                    TrackedInvite {
                        code: inv.code,
                        uses: inv.uses,
                        inviter: inviter_name,
                    },
                );
            }
            self.invites.write().await.insert(guild_id, map);
        }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        self.bot_id.store(ready.user.id.get(), Ordering::Relaxed);
        info!(bot = %ready.user.name, "LA Metro faregate connected");

        // Cache initial guild invites
        for guild in ready.guilds {
            self.cache_guild_invites(&ctx, guild.id).await;
        }
    }

    // --- Message Moderation & Delete Logging ---
    async fn message(&self, ctx: Context, message: Message) {
        // Save incoming message & file attachments to SQLite disk store
        self.save_message_to_db(&message);
        self.moderate(&ctx, &message).await;
    }

    async fn message_delete(
        &self,
        ctx: Context,
        channel_id: ChannelId,
        deleted_message_id: MessageId,
        _guild_id: Option<GuildId>,
    ) {
        // A deletion in the log channel must not generate another log.
        if ignore_message_delete(channel_id, 0, false) {
            return;
        }

        let stored = self.fetch_message_from_db(deleted_message_id);
        let cached = {
            ctx.cache
                .message(channel_id, deleted_message_id)
                .map(|msg| StoredMessage::from_message(&msg))
        };

        // Check both sources, including the cache for pre-migration database rows.
        if stored.iter().chain(cached.iter()).any(|msg| {
            ignore_message_delete(channel_id, msg.author_id, msg.is_webhook == Some(true))
        }) {
            return;
        }

        let (author_tag, author_id, content, attachments) =
            if let Some(record) = stored.or(cached) {
                (
                    record.author_tag,
                    record.author_id,
                    if record.content.is_empty() {
                        "*No text content*".to_string()
                    } else {
                        record.content
                    },
                    record.attachments,
                )
            } else {
                (
                    "Unknown User".to_string(),
                    0,
                    "*Message content not found in database or cache*".to_string(),
                    vec![],
                )
            };

        let mut desc = format!("**Message deleted in <#{}>**\n\n{}", channel_id.get(), content);
        if !attachments.is_empty() {
            desc.push_str("\n\n**Attached File URLs:**\n");
            for url in attachments {
                desc.push_str(&format!("{}\n", url));
            }
        }

        let embed = CreateEmbed::new()
            .author(serenity::builder::CreateEmbedAuthor::new(author_tag))
            .title("🗑️ Message Deleted")
            .description(desc)
            .field("Channel", format!("<#{}>", channel_id.get()), true)
            .field("Author", format!("<@{}>", author_id), true)
            .footer(CreateEmbedFooter::new(format!(
                "ID: {} • {}",
                author_id,
                chrono::Utc::now().format("%d/%m/%Y %H:%M")
            )));

        self.send_log_embed(&ctx, embed).await;
    }

    // --- Message Edit Logging ---
    async fn message_update(
        &self,
        ctx: Context,
        old_if_available: Option<Message>,
        new: Option<Message>,
        event: MessageUpdateEvent,
    ) {
        // Run moderation rule
        if event
            .content
            .as_deref()
            .is_some_and(contains_prohibited_link)
        {
            if let Ok(message) = event.channel_id.message(&ctx.http, event.id).await {
                self.moderate(&ctx, &message).await;
            }
        }

        // Gateway MESSAGE_UPDATE is partial. In particular, Discord sends
        // embed, pin and other metadata updates which are not text/file edits.
        if event.content.is_none() && event.attachments.is_none() {
            return;
        }

        let stored = self.fetch_message_from_db(event.id);
        let old = old_if_available.as_ref().map(StoredMessage::from_message);
        let current = new.as_ref().map(StoredMessage::from_message);
        let before = stored.as_ref().or(old.as_ref());
        let updated_content = event.content.as_deref();
        let updated_attachments = event.attachments.as_ref().map(|files| {
            files.iter().map(|file| file.url.clone()).collect::<Vec<_>>()
        });

        // Preserve the original snapshot even if it was only in Serenity's
        // cache, and refresh stored content for later deletion logs.
        if stored.is_none() {
            if let Some(message) = old_if_available.as_ref() {
                self.save_message_to_db(message);
            } else if let Some(message) = new.as_ref() {
                self.save_message_to_db(message);
            }
        }

        let is_webhook = event
            .webhook_id
            .map(|id| id.is_some())
            .or_else(|| current.as_ref().and_then(|msg| msg.is_webhook))
            .or_else(|| old.as_ref().and_then(|msg| msg.is_webhook))
            .or_else(|| stored.as_ref().and_then(|msg| msg.is_webhook));

        self.update_message_in_db(
            event.id,
            updated_content,
            updated_attachments.as_deref(),
            is_webhook,
        );

        let excluded = is_webhook == Some(true)
            || event.author.as_ref().is_some_and(|author| {
                ignore_message_author(author.id.get(), false)
            })
            || stored.iter().chain(old.iter()).chain(current.iter()).any(|msg| {
                ignore_message_author(msg.author_id, msg.is_webhook == Some(true))
            });
        if excluded {
            return;
        }

        // Without a previous snapshot, a repeated content field is not proof
        // of an edit. Seed the database when possible and wait for a real diff.
        let Some(before) = before else {
            if new.is_none() {
                if let Ok(message) = event.channel_id.message(&ctx.http, event.id).await {
                    self.save_message_to_db(&message);
                }
            }
            return;
        };

        if !has_message_edit(before, updated_content, updated_attachments.as_deref()) {
            return;
        }

        // Old databases did not record webhook IDs. Resolve the origin for an
        // actual edit when neither the update nor the cached message has it.
        if is_webhook.is_none() {
            let Ok(message) = event.channel_id.message(&ctx.http, event.id).await else {
                // Do not claim an edit came from a loggable author if unknown.
                return;
            };
            self.save_message_to_db(&message);
            if ignore_message_author(message.author.id.get(), message.webhook_id.is_some()) {
                return;
            }
        }

        let author_tag = event
            .author
            .as_ref()
            .map(|author| author.tag())
            .unwrap_or_else(|| before.author_tag.clone());
        let author_id = event
            .author
            .as_ref()
            .map(|author| author.id.get())
            .unwrap_or(before.author_id);
        let new_content = updated_content.unwrap_or(&before.content);

        let mut desc = format!(
            "**Before edit**\n{}\n\n**After edit**\n{}",
            before.content, new_content
        );

        if let Some(files) = updated_attachments.as_deref().filter(|files| {
            !same_attachments(&before.attachments, files)
        }) {
            let format_files = |files: &[String]| {
                if files.is_empty() {
                    "*None*".to_string()
                } else {
                    files.join("\n")
                }
            };
            desc.push_str(&format!(
                "\n\n**Attachments before**\n{}\n\n**Attachments after**\n{}",
                format_files(&before.attachments),
                format_files(files)
            ));
        } else if !before.attachments.is_empty() {
            desc.push_str("\n\n**Attached File URLs:**\n");
            desc.push_str(&before.attachments.join("\n"));
        }

        let embed = CreateEmbed::new()
            .author(serenity::builder::CreateEmbedAuthor::new(author_tag))
            .title("✏️ Message Edited")
            .description(desc)
            .field("Message author", format!("<@{}>", author_id), true)
            .field("Channel", format!("<#{}>", event.channel_id.get()), true)
            .field(
                "Jump to message",
                format!(
                    "[Jump](https://discord.com/channels/{}/{}/{})",
                    event.guild_id.unwrap_or_default(),
                    event.channel_id,
                    event.id
                ),
                false,
            )
            .footer(CreateEmbedFooter::new(format!(
                "ID: {} • {}",
                author_id,
                chrono::Utc::now().format("%d/%m/%Y %H:%M")
            )));

        self.send_log_embed(&ctx, embed).await;
    }

    // --- Reaction Removed Logging ---
    async fn reaction_remove(&self, ctx: Context, removed_reaction: Reaction) {
        let user_id = removed_reaction.user_id.unwrap_or_default();
        let emoji = removed_reaction.emoji.as_data();

        let embed = CreateEmbed::new()
            .title("➖ Reaction Removed")
            .description(format!(
                "Reaction **{}** was removed from message in <#{}>",
                emoji, removed_reaction.channel_id
            ))
            .field("User", format!("<@{}>", user_id), true)
            .field(
                "Jump to message",
                format!(
                    "[Jump](https://discord.com/channels/{}/{}/{})",
                    removed_reaction.guild_id.unwrap_or_default(),
                    removed_reaction.channel_id,
                    removed_reaction.message_id
                ),
                false,
            )
            .footer(CreateEmbedFooter::new(format!(
                "ID: {} • {}",
                user_id,
                chrono::Utc::now().format("%d/%m/%Y %H:%M")
            )));

        self.send_log_embed(&ctx, embed).await;
    }

    // --- Role Given / Role Taken / Nickname Updated Logging ---
    async fn guild_member_update(
        &self,
        ctx: Context,
        old_if_available: Option<Member>,
        new: Option<Member>,
        _event: GuildMemberUpdateEvent,
    ) {
        let member = match new {
            Some(m) => m,
            None => return,
        };

        if let Some(old) = old_if_available {
            // 1. Role Changes
            let old_roles: std::collections::HashSet<&RoleId> = old.roles.iter().collect();
            let new_roles: std::collections::HashSet<&RoleId> = member.roles.iter().collect();

            for added in new_roles.difference(&old_roles) {
                let embed = CreateEmbed::new()
                    .title("🛡️ Role Given")
                    .description(format!("Role <@&{}> was given to <@{}>", added, member.user.id))
                    .field("Target User", format!("<@{}>", member.user.id), true)
                    .footer(CreateEmbedFooter::new(format!(
                        "ID: {} • {}",
                        member.user.id,
                        chrono::Utc::now().format("%d/%m/%Y %H:%M")
                    )));
                self.send_log_embed(&ctx, embed).await;
            }

            for removed in old_roles.difference(&new_roles) {
                let embed = CreateEmbed::new()
                    .title("🛡️ Role Taken")
                    .description(format!("Role <@&{}> was taken from <@{}>", removed, member.user.id))
                    .field("Target User", format!("<@{}>", member.user.id), true)
                    .footer(CreateEmbedFooter::new(format!(
                        "ID: {} • {}",
                        member.user.id,
                        chrono::Utc::now().format("%d/%m/%Y %H:%M")
                    )));
                self.send_log_embed(&ctx, embed).await;
            }

            // 2. Nickname Changes
            if old.nick != member.nick {
                let old_nick = old.nick.as_deref().unwrap_or("*None*");
                let new_nick = member.nick.as_deref().unwrap_or("*None*");

                let embed = CreateEmbed::new()
                    .author(serenity::builder::CreateEmbedAuthor::new(member.user.tag()))
                    .title("🏷️ Nickname Updated")
                    .description(format!(
                        "**Before:** {}\n**After:** {}",
                        old_nick, new_nick
                    ))
                    .field("User", format!("<@{}>", member.user.id), true)
                    .footer(CreateEmbedFooter::new(format!(
                        "ID: {} • {}",
                        member.user.id,
                        chrono::Utc::now().format("%d/%m/%Y %H:%M")
                    )));
                self.send_log_embed(&ctx, embed).await;
            }
        }
    }

    // --- User Joining Server (with Invite Tracking) ---
    async fn guild_member_addition(&self, ctx: Context, member: Member) {
        let guild_id = member.guild_id;
        let mut used_invite_code = "Unknown".to_string();
        let mut inviter_name = "Unknown".to_string();

        if let Ok(current_invites) = guild_id.invites(&ctx.http).await {
            let mut lock = self.invites.write().await;
            if let Some(cached_map) = lock.get_mut(&guild_id) {
                for inv in current_invites {
                    if let Some(cached) = cached_map.get(&inv.code) {
                        if inv.uses > cached.uses {
                            used_invite_code = inv.code.clone();
                            inviter_name = cached.inviter.clone();
                            break;
                        }
                    }
                }
            }
        }

        // Refresh cached invite snapshot for next join
        self.cache_guild_invites(&ctx, guild_id).await;

        let embed = CreateEmbed::new()
            .author(serenity::builder::CreateEmbedAuthor::new(member.user.tag()))
            .title("📥 User Joined Server")
            .description(format!("User <@{}> joined the server.", member.user.id))
            .field("Invitation Code", used_invite_code, true)
            .field("Created By", inviter_name, true)
            .footer(CreateEmbedFooter::new(format!(
                "ID: {} • {}",
                member.user.id,
                chrono::Utc::now().format("%d/%m/%Y %H:%M")
            )));

        self.send_log_embed(&ctx, embed).await;
    }

    // --- User Leaving Server ---
    async fn guild_member_removal(
        &self,
        ctx: Context,
        _guild_id: GuildId,
        user: User,
        _member_data: Option<Member>,
    ) {
        let embed = CreateEmbed::new()
            .author(serenity::builder::CreateEmbedAuthor::new(user.tag()))
            .title("📤 User Left Server")
            .description(format!("User **{}** (<@{}>) left the server.", user.tag(), user.id))
            .footer(CreateEmbedFooter::new(format!(
                "ID: {} • {}",
                user.id,
                chrono::Utc::now().format("%d/%m/%Y %H:%M")
            )));

        self.send_log_embed(&ctx, embed).await;
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "la_metro_faregate=info".into()),
        )
        .init();
    let token = env::var("DISCORD_TOKEN")
        .map_err(|_| "Set DISCORD_TOKEN in the environment or a .env file")?;

    // Open/Create SQLite database file on disk
    let conn = Connection::open("bot_store.db")?;
    let handler = Handler::new(conn);
    handler.init_db()?;

    // Enable all required Gateway Intents for logging
    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILD_MEMBERS
        | GatewayIntents::GUILD_PRESENCES
        | GatewayIntents::GUILD_MODERATION
        | GatewayIntents::GUILD_INVITES
        | GatewayIntents::GUILD_MESSAGE_REACTIONS;

    let mut client = Client::builder(token, intents)
        .event_handler(handler)
        .await?;
    let shards = client.shard_manager.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            shards.shutdown_all().await;
        }
    });
    client.start().await?;
    Ok(())
}

#[cfg(test)]
mod logging_tests {
    use super::*;

    fn stored_message() -> StoredMessage {
        StoredMessage {
            author_tag: "passenger".to_string(),
            author_id: 42,
            content: "https://example.org/train".to_string(),
            attachments: vec!["https://cdn.discordapp.com/attachments/1/2/photo.png?ex=old".to_string()],
            is_webhook: Some(false),
        }
    }

    #[test]
    fn ignores_embed_only_updates_and_unchanged_content() {
        let before = stored_message();
        assert!(!has_message_edit(&before, None, None));
        assert!(!has_message_edit(&before, Some(&before.content), None));
        assert!(!has_message_edit(&before, Some(&before.content), Some(&before.attachments)));
    }

    #[test]
    fn logs_actual_text_edits_including_clearing_text() {
        let before = stored_message();
        assert!(has_message_edit(&before, Some("updated"), None));
        assert!(has_message_edit(&before, Some(""), None));
    }

    #[test]
    fn logs_attachment_removal_but_not_signed_url_refresh() {
        let before = stored_message();
        let refreshed = vec!["https://cdn.discordapp.com/attachments/1/2/photo.png?ex=new".to_string()];
        assert!(!has_message_edit(&before, None, Some(&refreshed)));
        assert!(has_message_edit(&before, None, Some(&[])));
    }

    #[test]
    fn filters_webhooks_and_catenary_but_not_other_authors() {
        assert!(ignore_message_author(42, true));
        assert!(ignore_message_author(CATENARY_AUTHOR_ID, false));
        assert!(!ignore_message_author(42, false));
        let other_channel = ChannelId::new(123);
        assert!(ignore_message_delete(other_channel, 42, true));
        assert!(ignore_message_delete(other_channel, CATENARY_AUTHOR_ID, false));
        assert!(!ignore_message_delete(other_channel, 42, false));
    }

    #[test]
    fn deletes_in_moderation_log_channel_do_not_generate_logs() {
        assert!(ignore_message_delete(ChannelId::new(LOG_CHANNEL_ID), 42, false));
        assert!(ignore_message_delete(ChannelId::new(LOG_CHANNEL_ID), 0, false));
        assert!(!ignore_message_delete(ChannelId::new(123), 42, false));
    }

    #[test]
    fn migrates_legacy_database_and_persists_webhook_metadata() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE stored_messages (
                message_id INTEGER PRIMARY KEY,
                channel_id INTEGER NOT NULL,
                author_tag TEXT NOT NULL,
                author_id INTEGER NOT NULL,
                content TEXT NOT NULL,
                attachments TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            INSERT INTO stored_messages VALUES (1, 123, 'legacy', 42, 'before', '', 'old');",
        ).unwrap();
        let handler = Handler::new(conn);
        handler.init_db().unwrap();
        handler.init_db().unwrap();
        let legacy = handler.fetch_message_from_db(MessageId::new(1)).unwrap();
        assert_eq!(legacy.content, "before");
        assert_eq!(legacy.is_webhook, None);

        let mut message = Message::default();
        message.id = MessageId::new(2);
        message.channel_id = ChannelId::new(123);
        message.author.id = UserId::new(42);
        message.webhook_id = Some(WebhookId::new(42));
        message.content = "webhook".to_string();
        handler.save_message_to_db(&message);

        let webhook = handler.fetch_message_from_db(message.id).unwrap();
        assert_eq!(webhook.is_webhook, Some(true));
        assert!(ignore_message_delete(message.channel_id, webhook.author_id, webhook.is_webhook == Some(true)));

        handler.update_message_in_db(message.id, Some("edited"), None, None);
        let edited = handler.fetch_message_from_db(message.id).unwrap();
        assert_eq!(edited.content, "edited");
        assert_eq!(edited.is_webhook, Some(true));

        handler.update_message_in_db(MessageId::new(1), None, Some(&[]), Some(false));
        let migrated = handler.fetch_message_from_db(MessageId::new(1)).unwrap();
        assert_eq!(migrated.content, "before");
        assert_eq!(migrated.is_webhook, Some(false));
    }
}
