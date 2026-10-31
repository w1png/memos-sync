use std::path::PathBuf;

use anyhow::{Context, Result, ensure};

use crate::api::normalize_url;

pub struct Config {
    pub url: String,
    pub token: String,
    pub data_path: PathBuf,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_values(
            std::env::var("MEMOS_URL").ok(),
            std::env::var("TOKEN").ok(),
            std::env::var("DATA_PATH").ok(),
        )
    }

    fn from_values(
        url: Option<String>,
        token: Option<String>,
        data_path: Option<String>,
    ) -> Result<Self> {
        let url = normalize_url(&url.context("MEMOS_URL is required")?)?;
        let token = token.context("TOKEN is required")?;
        ensure!(!token.trim().is_empty(), "TOKEN must not be empty");
        let path = data_path.unwrap_or_else(|| "./data".to_owned());
        ensure!(!path.trim().is_empty(), "DATA_PATH must not be empty");
        Ok(Self {
            url,
            token,
            data_path: PathBuf::from(path),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(path: Option<&str>) -> Result<Config> {
        Config::from_values(
            Some("https://example.com/".into()),
            Some("secret".into()),
            path.map(str::to_owned),
        )
    }

    #[test]
    fn defaults_and_overrides() {
        let config = parse(None).unwrap();
        assert_eq!(config.url, "https://example.com");
        assert_eq!(config.data_path, PathBuf::from("./data"));
        let config = parse(Some("/tmp/notes")).unwrap();
        assert_eq!(config.data_path, PathBuf::from("/tmp/notes"));
    }

    #[test]
    fn required_settings_are_validated() {
        assert!(Config::from_values(None, Some("secret".into()), None).is_err());
        assert!(
            Config::from_values(Some("https://example.com".into()), Some(" ".into()), None,)
                .is_err()
        );
        assert!(parse(Some("")).is_err());
    }
}
