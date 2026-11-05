use linkify::{LinkFinder, LinkKind};
use serenity::{async_trait, model::prelude::*, prelude::*};
use std::{
    env,
    error::Error,
    sync::atomic::{AtomicU64, Ordering},
};
use tracing::{error, info};
use url::Url;

const WARNING: &str = "x.com urls are prohibited on LA Metro";

fn contains_prohibited_link(content: &str) -> bool {
    let mut finder = LinkFinder::new();
    // Keep email recognition enabled so user@x.com is not treated as a URL.
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
            .and_then(|url| url.host_str().map(str::to_owned))
            .is_some_and(|host| {
                let host = host.trim_end_matches('.');
                host.eq_ignore_ascii_case("x.com") || host.to_ascii_lowercase().ends_with(".x.com") || host_lower == "twitter.com"
        || host_lower.ends_with(".twitter.com")
            })
    })
}

#[derive(Default)]
struct Handler {
    bot_id: AtomicU64,
}

impl Handler {
    async fn moderate(&self, ctx: &Context, message: &Message) {
        // Ignore DMs and our own warning, which itself contains x.com.
        // Messages from other bots and webhooks are also moderated.
        if message.guild_id.is_none()
            || message.author.id.get() == self.bot_id.load(Ordering::Relaxed)
            || !contains_prohibited_link(&message.content)
        {
            return;
        }

        if let Err(cause) = message.delete(&ctx.http).await {
            // Do not claim success or post duplicate warnings after a failed delete.
            error!(message_id = %message.id, channel_id = %message.channel_id,
                error = %cause, "Could not delete prohibited message");
            return;
        }
        if let Err(cause) = message.channel_id.say(&ctx.http, WARNING).await {
            error!(channel_id = %message.channel_id, error = %cause, "Could not send warning");
        }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        self.bot_id.store(ready.user.id.get(), Ordering::Relaxed);
        info!(bot = %ready.user.name, "LA Metro faregate connected");
    }

    async fn message(&self, ctx: Context, message: Message) {
        self.moderate(&ctx, &message).await;
    }

    async fn message_update(
        &self,
        ctx: Context,
        _old: Option<Message>,
        _new: Option<Message>,
        event: MessageUpdateEvent,
    ) {
        // Ignore embed-only updates; fetch content edits even without a message cache.
        if !event
            .content
            .as_deref()
            .is_some_and(contains_prohibited_link)
        {
            return;
        }
        match event.channel_id.message(&ctx.http, event.id).await {
            Ok(message) => self.moderate(&ctx, &message).await,
            Err(cause) => {
                error!(message_id = %event.id, error = %cause, "Could not fetch edited message")
            }
        }
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
    let intents = GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_x_hosts() {
        for text in [
            "https://x.com/user/status/123",
            "HTTP://X.COM/test",
            "x.com/test",
            "www.x.com/test",
            "https://mobile.x.com/test",
            "<https://x.com/test>",
            "[news](https://x.com/test)",
            "(https://x.com/test).",
            "https://x.com./test",
            "https://example.org@x.com/test",
        ] {
            assert!(contains_prohibited_link(text), "missed {text}");
        }
    }

    #[test]
    fn leaves_other_hosts_and_email_alone() {
        for text in [
            "hello",
            "https://example.com",
            "https://notx.com/test",
            "https://x.com.example.org/test",
            "https://example.org/x.com/test",
            "https://example.org/?url=https://x.com/test",
            "someone@x.com",
            "https://x.com@example.org/test",
            "https://twitter.com/test",
        ] {
            assert!(!contains_prohibited_link(text), "false positive: {text}");
        }
    }
}
