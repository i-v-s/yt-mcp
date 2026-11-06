use anyhow::{Context, Result, bail};
use reqwest::{
    Client, Response, Url,
    header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, LOCATION},
    redirect::Policy,
};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{sync::Arc, time::Duration};

use crate::config::Config;

const ISSUE_SUMMARY: &str =
    "id,idReadable,summary,created,updated,resolved,project(id,name,shortName)";
const CUSTOM_FIELDS: &str =
    "customFields(id,name,$type,value(id,name,login,fullName,minutes,presentation,isResolved))";
const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct YouTrack {
    inner: Arc<Inner>,
}

struct Inner {
    client: Client,
    config: Config,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
pub struct Page {
    /// Размер страницы: от 1 до 100, по умолчанию 20.
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 100))]
    pub limit: u32,
    /// Количество пропускаемых элементов, по умолчанию 0.
    #[serde(default)]
    pub offset: u32,
}

fn default_limit() -> u32 {
    20
}

impl Default for Page {
    fn default() -> Self {
        Self {
            limit: default_limit(),
            offset: 0,
        }
    }
}

impl Page {
    pub fn validate(self) -> Result<()> {
        if !(1..=100).contains(&self.limit) || self.offset > i32::MAX as u32 - 100 {
            bail!("limit должен быть от 1 до 100; offset — от 0 до 2147483547");
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct PageResult {
    items: Vec<Value>,
    limit: u32,
    offset: u32,
    count: usize,
    next_offset: Option<u32>,
}

#[derive(Deserialize)]
pub(crate) struct Attachment {
    pub id: String,
    pub name: String,
    pub size: u64,
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
    pub url: Option<String>,
    #[serde(default)]
    pub removed: bool,
}

pub(crate) fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 200
        || !id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        bail!("Некорректный ID: допустимы буквы, цифры, дефис и подчёркивание");
    }
    Ok(())
}

impl YouTrack {
    pub fn new(config: Config) -> Result<Self> {
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", config.token))
            .map_err(|_| anyhow::anyhow!("token содержит недопустимые символы"))?;
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);
        let client = Client::builder()
            .default_headers(headers)
            .redirect(Policy::none())
            .timeout(Duration::from_secs(config.timeout_secs))
            .connect_timeout(Duration::from_secs(config.timeout_secs.min(10)))
            .user_agent(concat!("yt-mcp/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| anyhow::anyhow!("Не удалось создать HTTP-клиент"))?;
        Ok(Self {
            inner: Arc::new(Inner { client, config }),
        })
    }

    pub(crate) fn config(&self) -> &Config {
        &self.inner.config
    }

    fn api_url(&self, segments: &[&str]) -> Result<Url> {
        let mut url = self.config().url.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Некорректный базовый URL"))?
            .pop_if_empty()
            .push("api")
            .extend(segments);
        Ok(url)
    }

    fn check_origin(&self, url: &Url) -> Result<()> {
        if url.origin() != self.config().url.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            bail!("Запрос или редирект за пределы origin YouTrack запрещён");
        }
        Ok(())
    }

    async fn get_response(&self, mut url: Url, accept: &str) -> Result<Response> {
        for _ in 0..=5 {
            self.check_origin(&url)?;
            // Единственный способ отправки HTTP-запросов в приложении — GET.
            let response = self
                .inner
                .client
                .get(url.clone())
                .header(ACCEPT, accept)
                .send()
                .await
                .map_err(http_error)?;
            let status = response.status();
            if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .context("YouTrack вернул некорректный редирект")?;
                url = url
                    .join(location)
                    .map_err(|_| anyhow::anyhow!("YouTrack вернул некорректный редирект"))?;
                continue;
            }
            if !status.is_success() {
                let message = match status.as_u16() {
                    400 => "некорректный запрос или поисковое выражение",
                    401 => "ошибка авторизации: проверьте token",
                    403 => "недостаточно прав доступа",
                    404 => "объект не найден или недоступен",
                    429 => "превышен лимит запросов",
                    500..=599 => "ошибка сервера YouTrack",
                    _ => "запрос отклонён",
                };
                bail!("YouTrack HTTP {}: {message}", status.as_u16());
            }
            return Ok(response);
        }
        bail!("YouTrack вернул слишком много редиректов")
    }

    async fn api_get(
        &self,
        segments: &[&str],
        fields: &str,
        params: &[(&str, String)],
    ) -> Result<Value> {
        let mut url = self.api_url(segments)?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("fields", fields);
            for (key, value) in params {
                query.append_pair(key, value);
            }
        }
        let mut response = self.get_response(url, "application/json").await?;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(http_error)? {
            if body.len().saturating_add(chunk.len()) > MAX_JSON_BYTES {
                bail!("JSON-ответ YouTrack превышает 16 МиБ; уменьшите размер страницы");
            }
            body.extend_from_slice(&chunk);
        }
        // Не выводим тело ответа: оно может содержать секреты или HTML страницы входа.
        serde_json::from_slice(&body)
            .map_err(|_| anyhow::anyhow!("YouTrack вернул некорректный JSON"))
    }

    async fn list(
        &self,
        segments: &[&str],
        fields: &str,
        page: Page,
        mut params: Vec<(&str, String)>,
    ) -> Result<Value> {
        page.validate()?;
        params.extend([
            ("$top", page.limit.to_string()),
            ("$skip", page.offset.to_string()),
        ]);
        let value = self.api_get(segments, fields, &params).await?;
        let items: Vec<Value> = serde_json::from_value(value)
            .map_err(|_| anyhow::anyhow!("YouTrack вернул некорректный список"))?;
        if items.len() > page.limit as usize {
            bail!("YouTrack вернул больше элементов, чем запрошено");
        }
        let count = items.len();
        // Даже короткая страница может быть ограничена настройками YouTrack.
        let next_offset = (count > 0).then_some(page.offset + count as u32);
        Ok(serde_json::to_value(PageResult {
            items,
            limit: page.limit,
            offset: page.offset,
            count,
            next_offset,
        })?)
    }

    pub async fn search_issues(&self, query: &str, page: Page) -> Result<Value> {
        self.list(
            &["issues"],
            &format!("{ISSUE_SUMMARY},{CUSTOM_FIELDS},tags(id,name)"),
            page,
            vec![("query", query.to_owned())],
        )
        .await
    }

    pub async fn get_issue(&self, issue_id: &str) -> Result<Value> {
        validate_id(issue_id)?;
        let value = self
            .api_get(
                &["issues", issue_id],
                &format!(
                    "{ISSUE_SUMMARY},description,reporter(id,login,fullName),tags(id,name),{CUSTOM_FIELDS}"
                ),
                &[],
            )
            .await?;
        require_object(value)
    }

    pub async fn list_issue_comments(&self, issue_id: &str, page: Page) -> Result<Value> {
        validate_id(issue_id)?;
        self.list(
            &["issues", issue_id, "comments"],
            "id,text,created,updated,deleted,author(id,login,fullName),attachments(id,name,size,mimeType)",
            page,
            vec![],
        )
        .await
    }

    pub async fn list_issue_links(&self, issue_id: &str, page: Page) -> Result<Value> {
        validate_id(issue_id)?;
        self.list(
            &["issues", issue_id, "links"],
            "id,direction,linkType(id,name,sourceToTarget,targetToSource,directed)",
            page,
            vec![],
        )
        .await
    }

    pub async fn list_linked_issues(
        &self,
        issue_id: &str,
        link_id: &str,
        page: Page,
    ) -> Result<Value> {
        validate_id(issue_id)?;
        validate_id(link_id)?;
        self.list(
            &["issues", issue_id, "links", link_id, "issues"],
            ISSUE_SUMMARY,
            page,
            vec![],
        )
        .await
    }

    pub async fn list_projects(&self, page: Page) -> Result<Value> {
        self.list(
            &["admin", "projects"],
            "id,name,shortName,description,archived",
            page,
            vec![],
        )
        .await
    }

    pub async fn list_project_fields(&self, project_id: &str, page: Page) -> Result<Value> {
        validate_id(project_id)?;
        self.list(
            &["admin", "projects", project_id, "customFields"],
            "id,$type,field(id,name,fieldType(id,valueType,isMultiValue)),canBeEmpty,emptyFieldText,isPublic",
            page,
            vec![],
        )
        .await
    }

    pub async fn get_current_user(&self) -> Result<Value> {
        require_object(
            self.api_get(&["users", "me"], "id,login,fullName,guest", &[])
                .await?,
        )
    }

    pub async fn list_issue_attachments(&self, issue_id: &str, page: Page) -> Result<Value> {
        validate_id(issue_id)?;
        self.list(
            &["issues", issue_id, "attachments"],
            "id,name,size,mimeType,created,updated,author(id,login,fullName),comment(id)",
            page,
            vec![],
        )
        .await
    }

    pub(crate) async fn attachment(
        &self,
        issue_id: &str,
        attachment_id: &str,
    ) -> Result<Attachment> {
        validate_id(issue_id)?;
        validate_id(attachment_id)?;
        let value = self
            .api_get(
                &["issues", issue_id, "attachments", attachment_id],
                "id,name,size,mimeType,url,removed",
                &[],
            )
            .await?;
        let attachment: Attachment = serde_json::from_value(value)
            .map_err(|_| anyhow::anyhow!("Некорректные метаданные вложения"))?;
        if attachment.id != attachment_id || attachment.removed {
            bail!("Вложение удалено или недоступно");
        }
        Ok(attachment)
    }

    pub(crate) async fn attachment_response(&self, location: &str) -> Result<Response> {
        let url = self
            .config()
            .url
            .join(location)
            .map_err(|_| anyhow::anyhow!("Некорректный URL вложения"))?;
        self.get_response(url, "application/octet-stream").await
    }
}

fn require_object(value: Value) -> Result<Value> {
    if !value.is_object() {
        bail!("YouTrack вернул некорректный объект");
    }
    Ok(value)
}

pub(crate) fn http_error(error: reqwest::Error) -> anyhow::Error {
    // Текст reqwest::Error содержит URL, в том числе подпись вложения.
    if error.is_timeout() {
        anyhow::anyhow!("Истекло время ожидания ответа YouTrack")
    } else if error.is_connect() {
        anyhow::anyhow!("Не удалось подключиться к YouTrack")
    } else {
        anyhow::anyhow!("Сетевая ошибка при чтении ответа YouTrack")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_cannot_change_routes() {
        for valid in ["PROJECT-123", "2-42", "80-1s", "ПРОЕКТ-12"] {
            validate_id(valid).unwrap();
        }
        for invalid in [
            "",
            ".",
            "..",
            "../users/me",
            "A/B",
            "A?x=1",
            "%2e%2e",
            "A\\B",
        ] {
            assert!(validate_id(invalid).is_err());
        }
        assert!(
            Page {
                limit: 0,
                offset: 0
            }
            .validate()
            .is_err()
        );
        assert!(
            Page {
                limit: 101,
                offset: 0
            }
            .validate()
            .is_err()
        );
        assert!(
            Page {
                limit: 20,
                offset: u32::MAX
            }
            .validate()
            .is_err()
        );
    }
}
