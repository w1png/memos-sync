use std::collections::HashSet;
use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Memo {
    pub name: String,
    pub content: String,
}

pub trait MemoApi {
    fn identity(&self) -> Result<String>;
    fn list_memos(&self) -> Result<Vec<Memo>>;
    fn get_memo(&self, name: &str) -> Result<Option<Memo>>;
    fn create_memo(&self, id: &str, content: &str) -> Result<Memo>;
    fn update_memo(&self, name: &str, content: &str) -> Result<Memo>;
}

pub struct HttpApi {
    client: Client,
    base_url: String,
    user: String,
}

#[derive(Deserialize)]
struct RemoteMemo {
    name: String,
    content: String,
    creator: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MemoPage {
    #[serde(default)]
    memos: Vec<RemoteMemo>,
    #[serde(default)]
    next_page_token: String,
}

#[derive(Deserialize)]
struct CurrentUser {
    user: User,
}

#[derive(Deserialize)]
struct User {
    name: String,
}

#[derive(Deserialize)]
struct Profile {
    version: String,
}

#[derive(Deserialize)]
struct ApiError {
    message: String,
}

const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;

pub fn normalize_url(value: &str) -> Result<String> {
    let url = Url::parse(value).context("MEMOS_URL must be an absolute HTTP or HTTPS URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "MEMOS_URL must be an absolute HTTP or HTTPS URL"
    );
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "MEMOS_URL must not contain credentials, a query, or a fragment"
    );
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

pub fn validate_name(name: &str) -> Result<()> {
    let id = name
        .strip_prefix("memos/")
        .context("Invalid memo resource name")?;
    validate_id(id)
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 36
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && id.as_bytes()[0].is_ascii_alphanumeric()
            && id.as_bytes()[id.len() - 1].is_ascii_alphanumeric(),
        "Invalid memo ID"
    );
    Ok(())
}

impl HttpApi {
    pub fn new(base_url: &str, token: &str) -> Result<Self> {
        let base_url = normalize_url(base_url)?;
        ensure!(!token.trim().is_empty(), "TOKEN is required");
        let mut headers = reqwest::header::HeaderMap::new();
        let mut auth = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| anyhow::anyhow!("TOKEN contains invalid header characters"))?;
        auth.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, auth);
        let client = Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .context("Could not initialize the Memos HTTP client")?;
        let mut api = Self {
            client,
            base_url,
            user: String::new(),
        };
        let profile: Profile = api.read_json(api.client.get(api.url("instance/profile")))?;
        ensure!(
            profile.version.starts_with("0.31."),
            "Unsupported Memos version {}; this client supports 0.31.x",
            profile.version
        );
        let current: CurrentUser = api.read_json(api.client.get(api.url("auth/me")))?;
        ensure!(
            current.user.name.starts_with("users/") && current.user.name.len() > 6,
            "Memos returned an invalid authenticated user"
        );
        api.user = current.user.name;
        Ok(api)
    }

    fn url(&self, route: &str) -> String {
        format!("{}/api/v1/{route}", self.base_url)
    }

    fn send(&self, request: RequestBuilder) -> Result<Response> {
        request
            .send()
            .map_err(|error| anyhow::anyhow!("Memos request failed: {}", error.without_url()))
    }

    fn require_success(response: Response) -> Result<Response> {
        let status = response.status();
        if !status.is_success() {
            let mut body = Vec::new();
            if response
                .take((MAX_ERROR_BODY_BYTES + 1) as u64)
                .read_to_end(&mut body)
                .is_ok()
                && body.len() <= MAX_ERROR_BODY_BYTES
                && let Ok(error) = serde_json::from_slice::<ApiError>(&body)
            {
                let message: String = error
                    .message
                    .chars()
                    .filter(|c| !c.is_control() || c.is_whitespace())
                    .collect();
                let message = message.split_whitespace().collect::<Vec<_>>().join(" ");
                if !message.is_empty() {
                    bail!("Memos API returned HTTP {status}: {message}");
                }
            }
            bail!("Memos API returned HTTP {status}");
        }
        Ok(response)
    }

    fn read_json<T: serde::de::DeserializeOwned>(&self, request: RequestBuilder) -> Result<T> {
        Self::require_success(self.send(request)?)?
            .json()
            .context("Memos returned an invalid JSON response")
    }

    fn own_memo(&self, remote: RemoteMemo) -> Result<Memo> {
        validate_name(&remote.name)?;
        ensure!(
            remote.creator == self.user,
            "Memos returned a memo owned by another user"
        );
        Ok(Memo {
            name: remote.name,
            content: remote.content,
        })
    }

    fn named_memo(&self, remote: RemoteMemo, expected: &str) -> Result<Memo> {
        ensure!(
            remote.name == expected,
            "Memos returned an unexpected memo identity"
        );
        self.own_memo(remote)
    }
}

impl MemoApi for HttpApi {
    fn identity(&self) -> Result<String> {
        Ok(format!("{}#{}", self.base_url, self.user))
    }

    fn list_memos(&self) -> Result<Vec<Memo>> {
        let filter = format!("creator == {}", serde_json::to_string(&self.user)?);
        let mut result = Vec::new();
        let mut names = HashSet::new();
        for state in ["NORMAL", "ARCHIVED"] {
            let mut page_token = String::new();
            let mut tokens = HashSet::new();
            loop {
                let page: MemoPage =
                    self.read_json(self.client.get(self.url("memos")).query(&[
                        ("pageSize", "1000"),
                        ("state", state),
                        ("filter", filter.as_str()),
                        ("pageToken", page_token.as_str()),
                    ]))?;
                for remote in page.memos {
                    let memo = self.own_memo(remote)?;
                    ensure!(
                        names.insert(memo.name.clone()),
                        "Memos listing changed or repeated a memo; retry sync"
                    );
                    result.push(memo);
                }
                if page.next_page_token.is_empty() {
                    break;
                }
                ensure!(
                    tokens.insert(page.next_page_token.clone()),
                    "Memos repeated a pagination token"
                );
                page_token = page.next_page_token;
            }
        }
        Ok(result)
    }

    fn get_memo(&self, name: &str) -> Result<Option<Memo>> {
        validate_name(name)?;
        let response = self
            .send(self.client.get(self.url(name)))
            .with_context(|| format!("Getting {name}"))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let remote: RemoteMemo = Self::require_success(response)
            .with_context(|| format!("Getting {name}"))?
            .json()
            .context("Memos returned an invalid memo")?;
        Ok(Some(self.named_memo(remote, name)?))
    }

    fn create_memo(&self, id: &str, content: &str) -> Result<Memo> {
        validate_id(id)?;
        let remote = self
            .read_json(
                self.client
                    .post(self.url("memos"))
                    .query(&[("memoId", id)])
                    .json(&serde_json::json!({"content": content, "visibility": "PRIVATE"})),
            )
            .with_context(|| format!("Creating memos/{id}"))?;
        self.named_memo(remote, &format!("memos/{id}"))
    }

    fn update_memo(&self, name: &str, content: &str) -> Result<Memo> {
        validate_name(name)?;
        let remote = self
            .read_json(
                self.client
                    .patch(self.url(name))
                    .query(&[("updateMask", "content")])
                    .json(&serde_json::json!({"name": name, "content": content})),
            )
            .with_context(|| format!("Updating {name}"))?;
        self.named_memo(remote, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use tiny_http::{Response, Server};

    fn with_server(
        responses: Vec<(u16, serde_json::Value)>,
        test: impl FnOnce(String),
    ) -> Vec<(String, String, String)> {
        let server = Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr());
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let mut request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .expect("HTTP request did not arrive");
                assert!(
                    request
                        .headers()
                        .iter()
                        .any(|header| header.field.equiv("Authorization")
                            && header.value.as_str() == "Bearer test-token")
                );
                let mut body_text = String::new();
                request.as_reader().read_to_string(&mut body_text).unwrap();
                requests.push((
                    request.method().to_string(),
                    request.url().to_owned(),
                    body_text,
                ));
                request
                    .respond(Response::from_string(body.to_string()).with_status_code(status))
                    .unwrap();
            }
            requests
        });
        test(url);
        let requests = handle.join().unwrap();
        if requests.len() >= 3 {
            assert!(!requests[2].1.contains("test-token"));
        }
        requests
    }

    fn initialization() -> Vec<(u16, serde_json::Value)> {
        vec![
            (200, serde_json::json!({"version":"0.31.0"})),
            (200, serde_json::json!({"user":{"name":"users/1"}})),
        ]
    }

    fn remote(id: &str) -> serde_json::Value {
        serde_json::json!({"name":format!("memos/{id}"), "content":"hello", "creator":"users/1"})
    }

    #[test]
    fn lists_all_pages_and_archived_notes() {
        let mut responses = initialization();
        responses.extend([
            (
                200,
                serde_json::json!({"memos":[remote("a")], "nextPageToken":"second"}),
            ),
            (200, serde_json::json!({"memos":[remote("b")]})),
            (200, serde_json::json!({"memos":[remote("c")]})),
        ]);
        let requests = with_server(responses, |url| {
            let api = HttpApi::new(&url, "test-token").unwrap();
            let memos = api.list_memos().unwrap();
            assert_eq!(
                memos.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
                ["memos/a", "memos/b", "memos/c"]
            );
            assert_eq!(api.identity().unwrap(), format!("{url}#users/1"));
        });
        assert!(requests[2].1.contains("state=NORMAL"));
        assert!(requests[2].1.contains("creator+%3D%3D+%22users%2F1%22"));
        assert!(requests[3].1.contains("pageToken=second"));
        assert!(requests[4].1.contains("state=ARCHIVED"));
    }

    #[test]
    fn failed_later_page_fails_the_whole_listing() {
        let mut responses = initialization();
        responses.extend([
            (
                200,
                serde_json::json!({"memos":[remote("a")], "nextPageToken":"next"}),
            ),
            (503, serde_json::json!({"message":"unavailable"})),
        ]);
        with_server(responses, |url| {
            let api = HttpApi::new(&url, "test-token").unwrap();
            assert!(api.list_memos().unwrap_err().to_string().contains("503"));
        });
    }

    #[test]
    fn only_not_found_means_deleted() {
        let mut responses = initialization();
        responses.extend([(404, serde_json::json!({})), (403, serde_json::json!({}))]);
        with_server(responses, |url| {
            let api = HttpApi::new(&url, "test-token").unwrap();
            assert!(api.get_memo("memos/a").unwrap().is_none());
            assert!(api.get_memo("memos/a").is_err());
        });
    }

    #[test]
    fn writes_use_persistent_ids_private_creation_and_content_only_updates() {
        let mut responses = initialization();
        responses.extend([(200, remote("new-id")), (200, remote("new-id"))]);
        let requests = with_server(responses, |url| {
            let api = HttpApi::new(&url, "test-token").unwrap();
            api.create_memo("new-id", "hello").unwrap();
            api.update_memo("memos/new-id", "hello").unwrap();
        });
        assert_eq!(requests[2].0, "POST");
        assert_eq!(requests[2].1, "/api/v1/memos?memoId=new-id");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&requests[2].2).unwrap(),
            serde_json::json!({"content":"hello", "visibility":"PRIVATE"})
        );
        assert_eq!(requests[3].0, "PATCH");
        assert_eq!(requests[3].1, "/api/v1/memos/new-id?updateMask=content");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&requests[3].2).unwrap(),
            serde_json::json!({"content":"hello", "name":"memos/new-id"})
        );
    }

    #[test]
    fn rejected_create_reports_server_message_without_request_credentials_or_content() {
        let mut responses = initialization();
        responses.push((400, serde_json::json!({"code":3, "message":"content too long (max 8192 characters)", "details":[]})));
        with_server(responses, |url| {
            let api = HttpApi::new(&url, "test-token").unwrap();
            let error = api
                .create_memo("test", "private note contents")
                .unwrap_err();
            let error = format!("{error:#}");
            assert!(error.contains("Creating memos/test"));
            assert!(error.contains("HTTP 400"));
            assert!(error.contains("content too long (max 8192 characters)"));
            assert!(!error.contains("test-token"));
            assert!(!error.contains("private note contents"));
        });
    }

    #[test]
    fn error_body_falls_back_to_status_for_html_invalid_json_and_oversized_payloads() {
        let oversized = serde_json::json!({"message":"must not appear", "details":"x".repeat(MAX_ERROR_BODY_BYTES)}).to_string();
        for body in [
            "<html>private upstream response</html>".to_owned(),
            "{broken JSON".to_owned(),
            "{\"message\":42}".to_owned(),
            "{\"details\":\"private upstream response\"}".to_owned(),
            oversized,
        ] {
            let server = Server::http("127.0.0.1:0").unwrap();
            let url = format!("http://{}", server.server_addr());
            let handle = thread::spawn(move || {
                let request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                request
                    .respond(Response::from_string(body).with_status_code(400))
                    .unwrap();
            });
            let response = Client::new().get(url).send().unwrap();
            let error = HttpApi::require_success(response).unwrap_err();
            assert_eq!(error.to_string(), "Memos API returned HTTP 400 Bad Request");
            handle.join().unwrap();
        }
    }

    #[test]
    fn get_and_update_include_operation_and_server_error() {
        let mut responses = initialization();
        responses.extend([
            (403, serde_json::json!({"message":"permission denied"})),
            (400, serde_json::json!({"message":"content too long"})),
        ]);
        with_server(responses, |url| {
            let api = HttpApi::new(&url, "test-token").unwrap();
            let get_error = format!("{:#}", api.get_memo("memos/test").unwrap_err());
            assert!(get_error.contains("Getting memos/test"));
            assert!(get_error.contains("permission denied"));
            let update_error = format!(
                "{:#}",
                api.update_memo("memos/test", "private contents")
                    .unwrap_err()
            );
            assert!(update_error.contains("Updating memos/test"));
            assert!(update_error.contains("content too long"));
            assert!(!update_error.contains("private contents"));
        });
    }

    #[test]
    fn redirects_are_not_followed_with_credentials() {
        with_server(vec![(302, serde_json::json!({}))], |url| {
            let error = HttpApi::new(&url, "test-token").err().unwrap();
            assert!(error.to_string().contains("302"));
            assert!(!format!("{error:#}").contains("test-token"));
        });
    }

    #[test]
    fn rejects_foreign_memos_and_unsupported_versions() {
        let mut responses = initialization();
        responses.push((
            200,
            serde_json::json!({"name":"memos/a","content":"hi","creator":"users/2"}),
        ));
        with_server(responses, |url| {
            let api = HttpApi::new(&url, "test-token").unwrap();
            assert!(api.get_memo("memos/a").is_err());
        });
        with_server(
            vec![(200, serde_json::json!({"version":"26.10.0"}))],
            |url| {
                assert!(HttpApi::new(&url, "test-token").is_err());
            },
        );
    }

    #[test]
    fn validates_names_and_urls() {
        for name in [
            "memos/../auth/me",
            "memos/",
            "memos/-bad",
            "memos/hello?x=y",
            "users/1",
        ] {
            assert!(validate_name(name).is_err(), "accepted {name}");
        }
        assert!(validate_name("memos/a-B23").is_ok());
        for url in [
            "file:///tmp/data",
            "https://user:password@example.com",
            "https://example.com?token=x",
            "https://example.com#x",
        ] {
            assert!(normalize_url(url).is_err());
        }
        assert_eq!(
            normalize_url("https://example.com/memos/").unwrap(),
            "https://example.com/memos"
        );
    }
}
