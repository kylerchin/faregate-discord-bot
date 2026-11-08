use linkify::{LinkFinder, LinkKind};
use serenity::{
    async_trait,
    builder::{CreateEmbed, CreateEmbedFooter, CreateMessage},
    model::{
        guild::AuditLogEntry,
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
        Arc,
    },
};
use tokio::sync::RwLock;
use tracing::{error, info};
use url::Url;

const LOG_CHANNEL_ID: u64 = 1488275200792461333;
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

#[derive(Default)]
struct Handler {
    bot_id: AtomicU64,
    // Store guild invites to track who invited whom upon member join
    invites: Arc<RwLock<HashMap<GuildId, HashMap<String, TrackedInvite>>>>,
}

impl Handler {
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
        self.moderate(&ctx, &message).await;
    }

    async fn message_delete(
        &self,
        ctx: Context,
        channel_id: ChannelId,
        deleted_message_id: MessageId,
        guild_id: Option<GuildId>,
    ) {
        let cached = ctx.cache.message(channel_id, deleted_message_id);

        let (author_tag, author_id, content, attachments) = match cached {
            Some(msg) => (
                msg.author.tag(),
                msg.author.id.get(),
                if msg.content.is_empty() {
                    "*No text content*".to_string()
                } else {
                    msg.content.clone()
                },
                msg.attachments
                    .iter()
                    .map(|a| a.url.clone())
                    .collect::<Vec<_>>(),
            ),
            None => (
                "Unknown User".to_string(),
                0,
                "*Message content not in cache*".to_string(),
                vec![],
            ),
        };

        let mut desc = format!("**Message deleted in <#{}>**\n\n{}", channel_id.get(), content);
        if !attachments.is_empty() {
            desc.push_str("\n\n**Attachments:**\n");
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

        // Handle logging
        let old_content = old_if_available
            .map(|m| m.content)
            .unwrap_or_else(|| "*Content before edit not cached*".to_string());
        let new_content = event
            .content
            .clone()
            .unwrap_or_else(|| "*No content*".to_string());
        let author_tag = event
            .author
            .as_ref()
            .map(|a| a.tag())
            .unwrap_or_else(|| "Unknown User".to_string());
        let author_id = event
            .author
            .as_ref()
            .map(|a| a.id.get())
            .unwrap_or(0);

        let mut desc = format!(
            "{}\n\n**After edit**\n{}",
            old_content, new_content
        );

        if let Some(msg) = new {
            if !msg.attachments.is_empty() {
                desc.push_str("\n\n**Attachments:**\n");
                for att in msg.attachments {
                    desc.push_str(&format!("{}\n", att.url));
                }
            }
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
        event: GuildMemberUpdateEvent,
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
        guild_id: GuildId,
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

    // Enable all required Gateway Intents for logging
    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILD_MEMBERS
        | GatewayIntents::GUILD_PRESENCES
        | GatewayIntents::GUILD_MODERATION
        | GatewayIntents::GUILD_INVITES
        | GatewayIntents::GUILD_MESSAGE_REACTIONS;

    let mut client = Client::builder(token, intents)
        .event_handler(Handler::default())
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
