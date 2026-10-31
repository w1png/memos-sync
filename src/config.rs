use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use crate::api::normalize_url;

pub struct Config {
    pub url: String,
    pub token: String,
    pub data_path: PathBuf,
}

impl Config {
    pub fn from_sources(
        url: Option<String>,
        token: Option<String>,
        data_path: Option<PathBuf>,
    ) -> Result<Self> {
        // A fully specified command is independent of the current .env file.
        if url.is_some() && token.is_some() && data_path.is_some() {
            return Self::from_values(url, token, data_path);
        }
        let url = url.or_else(|| std::env::var("MEMOS_URL").ok());
        let token = token.or_else(|| std::env::var("TOKEN").ok());
        let data_path = data_path.or_else(|| std::env::var_os("DATA_PATH").map(PathBuf::from));
        let file = if url.is_none() || token.is_none() || data_path.is_none() {
            read_dotenv(Path::new(".env"), false)?
        } else {
            BTreeMap::new()
        };
        Self::from_values(
            url.or_else(|| file.get("MEMOS_URL").cloned()),
            token.or_else(|| file.get("TOKEN").cloned()),
            data_path.or_else(|| file.get("DATA_PATH").map(PathBuf::from)),
        )
    }

    pub fn from_dotenv(path: &Path) -> Result<Self> {
        let file = read_dotenv(path, true)?;
        Self::from_values(
            file.get("MEMOS_URL").cloned(),
            file.get("TOKEN").cloned(),
            file.get("DATA_PATH").map(PathBuf::from),
        )
    }

    fn from_values(
        url: Option<String>,
        token: Option<String>,
        data_path: Option<PathBuf>,
    ) -> Result<Self> {
        let url = normalize_url(&url.context("Memos URL is required (--url or MEMOS_URL)")?)?;
        let token = token.context("Token is required (--token or TOKEN)")?;
        ensure!(!token.trim().is_empty(), "TOKEN must not be empty");
        let path = data_path.unwrap_or_else(|| PathBuf::from("./data"));
        ensure!(!path.as_os_str().is_empty(), "DATA_PATH must not be empty");
        Ok(Self {
            url,
            token,
            data_path: path,
        })
    }
}

fn read_dotenv(path: &Path, required: bool) -> Result<BTreeMap<String, String>> {
    let values = match dotenvy::from_path_iter(path) {
        Ok(values) => values,
        Err(dotenvy::Error::Io(error))
            if !required && error.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(BTreeMap::new());
        }
        // Parser errors may contain the input line, including a credential.
        Err(_) => anyhow::bail!("Cannot read configuration file {}", path.display()),
    };
    let mut file = BTreeMap::new();
    for value in values {
        let (key, value) =
            value.map_err(|_| anyhow::anyhow!("Invalid configuration file {}", path.display()))?;
        file.entry(key).or_insert(value);
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(path: Option<&str>) -> Result<Config> {
        Config::from_values(
            Some("https://example.com/".into()),
            Some("secret".into()),
            path.map(PathBuf::from),
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

    #[test]
    fn service_configuration_is_read_from_the_selected_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".env");
        std::fs::write(
            &path,
            "MEMOS_URL=https://file.example\nTOKEN=file-token\nDATA_PATH=./my-notes\n",
        )
        .unwrap();
        let config = Config::from_dotenv(&path).unwrap();
        assert_eq!(config.url, "https://file.example");
        assert_eq!(config.token, "file-token");
        assert_eq!(config.data_path, PathBuf::from("./my-notes"));
        std::fs::write(
            &path,
            "MEMOS_URL=https://file.example\nTOKEN=\"secret-unclosed",
        )
        .unwrap();
        let error = Config::from_dotenv(&path).err().unwrap();
        assert!(!format!("{error:#}").contains("secret-unclosed"));
        assert!(Config::from_dotenv(&temp.path().join("missing")).is_err());
    }
}
