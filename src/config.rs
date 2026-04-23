use anyhow::{Context, Result};

#[derive(Clone)]
pub struct Config {
    pub discord_token: String,
    pub discord_application_id: u64,
    pub discord_test_guild_id: Option<u64>,
    pub database_url: String,
    pub realmeye_user_agent: String,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn mask(s: &str) -> String {
            if s.len() <= 4 {
                return "***".to_string();
            }
            format!("{}…({})", &s[..4], s.len())
        }
        f.debug_struct("Config")
            .field("discord_token", &mask(&self.discord_token))
            .field("discord_application_id", &self.discord_application_id)
            .field("discord_test_guild_id", &self.discord_test_guild_id)
            .field("database_url", &mask(&self.database_url))
            .field("realmeye_user_agent", &self.realmeye_user_agent)
            .finish()
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        let discord_token = std::env::var("DISCORD_TOKEN").context("DISCORD_TOKEN must be set")?;
        let discord_application_id: u64 = std::env::var("DISCORD_APPLICATION_ID")
            .context("DISCORD_APPLICATION_ID must be set")?
            .parse()
            .context("DISCORD_APPLICATION_ID must be a valid u64")?;
        let discord_test_guild_id = std::env::var("DISCORD_TEST_GUILD_ID")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<u64>())
            .transpose()
            .context("DISCORD_TEST_GUILD_ID must be a valid u64 if set")?;
        let database_url =
            std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
        let realmeye_user_agent = std::env::var("REALMEYE_USER_AGENT")
            .unwrap_or_else(|_| "starship-bot/0.1".to_string());

        Ok(Config {
            discord_token,
            discord_application_id,
            discord_test_guild_id,
            database_url,
            realmeye_user_agent,
        })
    }
}
