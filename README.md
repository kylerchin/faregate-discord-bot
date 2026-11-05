# LA Metro faregate

A Rust Discord bot that deletes server messages containing x.com URLs, then posts exactly:

> x.com urls are prohibited on LA Metro

It checks new messages and content edits. Matching is case insensitive and includes bare domains, www.x.com, subdomains, Markdown links, and links in angle brackets. It checks the URL hostname, so unrelated hosts and paths such as example.org/x.com are allowed. Other bots and webhooks are checked; the bot ignores its own messages to prevent a warning loop.

Discord deletes whole messages: any text or attachments in the same message are removed too. There are no commands or exemptions. The bot moderates accessible channels in every server where it is installed. It does not scan old history, DMs, attachments, embed-only URLs, redirects, URL shorteners, or twitter.com links.

## Setup

1. Open your existing **LA Metro faregate** application in the [Discord Developer Portal](https://discord.com/developers/applications).
2. Under **Bot**, enable **Message Content Intent**, and obtain the bot token. Keep it private.
3. Install the bot to your server using the `bot` OAuth2 scope. Grant **View Channels**, **Manage Messages**, **Send Messages**, and **Read Message History**. For threads, also grant **Send Messages in Threads** and ensure the bot can access the thread. Channel overrides must allow these permissions. Administrator is unnecessary.
4. Install stable Rust using [rustup](https://rustup.rs/), then in this project directory:

   ```sh
   cp .env.example .env
   ```

5. Edit `.env` and replace the placeholder with your token, then start:

   ```sh
   cargo run --release
   ```

You can set `DISCORD_TOKEN` directly in the process environment instead of using `.env`. `RUST_LOG` controls logging; the default logs connection and moderation errors. The token and message contents are not logged by this application. Keep the process running on your computer or server; Ctrl+C shuts it down.

If deletion fails, the bot logs the error and sends no warning. If deletion succeeds but sending the warning fails, it logs that failure. Discord's HTTP rate limits are handled by Serenity.

## Validation

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

Build verification for this package: `cargo test` passed both test groups (19 URL cases), and `cargo fmt --check` passed. Clippy was unavailable in the build environment. Live Discord integration was not run because it requires your token and server.

To verify the Discord integration, post `https://x.com/example/status/123` in a test channel: the entire message should disappear and the warning should appear. Also edit a harmless message to contain that link. Check that `https://example.org/x.com` remains. These live checks require your bot token and server.

## References

- [Discord gateway intents](https://github.com/discord/discord-api-docs/blob/main/developers/events/gateway.mdx)
- [Discord message API](https://github.com/discord/discord-api-docs/blob/main/developers/resources/message.mdx)
- [Discord permissions](https://github.com/discord/discord-api-docs/blob/main/developers/topics/permissions.mdx)
- [Serenity event handlers](https://docs.rs/serenity/0.12.5/serenity/client/trait.EventHandler.html)
