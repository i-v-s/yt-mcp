use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Request, State},
    http::{Response, StatusCode},
    response::IntoResponse,
};
use futures_util::{StreamExt, stream};
use rmcp::ServiceExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    convert::Infallible,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::{
    config::Config,
    server::YouTrackServer,
    youtrack::{Page, YouTrack},
};

const FILE_BYTES: &[u8] = b"\0\xff\r\n";

#[tokio::test]
#[ignore = "Требует настроенного YouTrack, пользовательского токена и сетевого доступа"]
async fn live_configured_youtrack() -> anyhow::Result<()> {
    use anyhow::Context;

    let path = Config::resolve_path(None)?;
    let client = YouTrack::new(Config::load(&path)?)?;
    println!("Конфигурация: {}", path.display());
    let page = Page {
        limit: 1,
        offset: 0,
    };
    client
        .get_current_user()
        .await
        .context("get_current_user")?;
    println!("get_current_user: OK");
    let projects = client.list_projects(page).await.context("list_projects")?;
    println!("list_projects: OK");
    if let Some(project_id) = projects["items"][0]["id"].as_str() {
        client
            .list_project_fields(project_id, page)
            .await
            .context("list_project_fields")?;
        println!("list_project_fields: OK");
    }
    let issues = client
        .search_issues("", page)
        .await
        .context("search_issues")?;
    println!("search_issues: OK");
    if let Some(issue_id) = issues["items"][0]["id"].as_str() {
        client.get_issue(issue_id).await.context("get_issue")?;
        println!("get_issue: OK");
        client
            .list_issue_comments(issue_id, page)
            .await
            .context("list_issue_comments")?;
        println!("list_issue_comments: OK");
        let links = client
            .list_issue_links(issue_id, page)
            .await
            .context("list_issue_links")?;
        println!("list_issue_links: OK");
        if let Some(link_id) = links["items"][0]["id"].as_str() {
            client
                .list_linked_issues(issue_id, link_id, page)
                .await
                .context("list_linked_issues")?;
            println!("list_linked_issues: OK");
        }
        client
            .list_issue_attachments(issue_id, page)
            .await
            .context("list_issue_attachments")?;
        println!("list_issue_attachments: OK");
    }
    Ok(())
}

#[derive(Clone, Default)]
enum ApiMode {
    #[default]
    Normal,
    Status(u16),
    InvalidJson,
    WrongShape,
    Slow,
}

#[derive(Clone, Default)]
enum FileMode {
    #[default]
    Normal,
    TooLarge,
    ChunkedTooLarge,
    Broken,
    Pending,
}

#[derive(Clone)]
struct Settings {
    api_mode: ApiMode,
    file_mode: FileMode,
    name: String,
    metadata_size: u64,
    location: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            api_mode: ApiMode::Normal,
            file_mode: FileMode::Normal,
            name: "../../отчёт.bin".into(),
            metadata_size: FILE_BYTES.len() as u64,
            location: "/youtrack/api/files/content?sign=private-sign".into(),
        }
    }
}

struct Captured {
    method: String,
    path: String,
    query: HashMap<String, String>,
    authorization: String,
}

#[derive(Clone)]
struct MockState {
    settings: Settings,
    captured: Arc<Mutex<Vec<Captured>>>,
}

struct Mock {
    base: reqwest::Url,
    state: MockState,
    task: tokio::task::JoinHandle<()>,
}

impl Mock {
    async fn start(settings: Settings) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/youtrack/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let state = MockState {
            settings,
            captured: Arc::new(Mutex::new(Vec::new())),
        };
        let router = Router::new()
            .fallback(mock_handler)
            .with_state(state.clone());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { base, state, task }
    }

    fn client(&self, download_dir: &Path, max_attachment_bytes: u64) -> YouTrack {
        YouTrack::new(Config {
            url: self.base.clone(),
            token: "test-secret".into(),
            download_dir: download_dir.to_owned(),
            timeout_secs: 1,
            max_attachment_bytes,
        })
        .unwrap()
    }

    fn assert_read_only(&self) {
        let requests = self.state.captured.lock().unwrap();
        assert!(!requests.is_empty());
        for request in requests.iter() {
            assert_eq!(request.method, "GET");
            assert_eq!(request.authorization, "Bearer test-secret");
            assert!(request.path.starts_with("/youtrack/api/"));
        }
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn mock_handler(State(state): State<MockState>, request: Request) -> Response<Body> {
    let path = request.uri().path().to_owned();
    let query: HashMap<String, String> =
        reqwest::Url::parse(&format!("http://localhost{}", request.uri()))
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
    state.captured.lock().unwrap().push(Captured {
        method: request.method().to_string(),
        path: path.clone(),
        query: query.clone(),
        authorization: request
            .headers()
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned(),
    });

    if path.contains("/api/files/") {
        if path.ends_with("/redirect") {
            return Response::builder()
                .status(302)
                .header("location", "/youtrack/api/files/content?sign=private-sign")
                .body(Body::empty())
                .unwrap();
        }
        if path.ends_with("/redirect-out") {
            return Response::builder()
                .status(302)
                .header("location", "http://localhost:9/private-sign")
                .body(Body::empty())
                .unwrap();
        }
        if path.ends_with("/loop") {
            return Response::builder()
                .status(302)
                .header("location", "/youtrack/api/files/loop?sign=private-sign")
                .body(Body::empty())
                .unwrap();
        }
        let body = match state.settings.file_mode {
            FileMode::Normal => Body::from(FILE_BYTES),
            FileMode::TooLarge => Body::from(vec![0u8; 20]),
            FileMode::ChunkedTooLarge => Body::from_stream(stream::iter([
                Ok::<_, Infallible>(Bytes::from_static(b"12345678")),
                Ok(Bytes::from_static(b"12345678")),
            ])),
            FileMode::Broken => Body::from_stream(
                stream::once(async { Ok::<_, std::io::Error>(Bytes::from_static(b"1234")) }).chain(
                    stream::once(async {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Err(std::io::Error::other("interrupted"))
                    }),
                ),
            ),
            FileMode::Pending => Body::from_stream(
                stream::once(async { Ok::<_, Infallible>(Bytes::from_static(FILE_BYTES)) })
                    .chain(stream::pending()),
            ),
        };
        return Response::new(body);
    }
    match state.settings.api_mode {
        ApiMode::Status(status) => {
            return (
                StatusCode::from_u16(status).unwrap(),
                "test-secret private-sign",
            )
                .into_response();
        }
        ApiMode::InvalidJson => return "invalid JSON: test-secret private-sign".into_response(),
        ApiMode::WrongShape => return axum::Json(json!("wrong shape")).into_response(),
        ApiMode::Slow => tokio::time::sleep(Duration::from_secs(2)).await,
        ApiMode::Normal => {}
    }
    let value = if path.ends_with("/users/me") {
        json!({"id":"1-1","login":"reader","fullName":"Reader","guest":false})
    } else if path.ends_with("/attachments/7-1") {
        json!({
            "id":"7-1", "name":state.settings.name,
            "size":state.settings.metadata_size, "mimeType":"application/octet-stream",
            "url":state.settings.location, "removed":false
        })
    } else if path.ends_with("/issues/DEMO-1") {
        json!({
            "id":"2-1", "idReadable":"DEMO-1", "summary":"Пример", "description":null,
            "customFields":[
                {"name":"Number","value":42},
                {"name":"Text","value":"значение"},
                {"name":"Empty","value":null},
                {"name":"Assignee","value":{"login":"reader"}},
                {"name":"Versions","value":[{"name":"1.0"},{"name":"2.0"}]},
                {"name":"Estimation","value":{"minutes":90,"presentation":"1h 30m"}}
            ]
        })
    } else if query.get("$skip").is_some_and(|v| v == "100") {
        json!([])
    } else {
        json!([{"id":"2-1","idReadable":"DEMO-1","summary":"Пример"}])
    };
    axum::Json(value).into_response()
}

#[tokio::test]
async fn all_api_routes_are_get_and_queries_are_encoded() {
    let mock = Mock::start(Settings::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let client = mock.client(dir.path(), 100);
    let page = Page {
        limit: 7,
        offset: 3,
    };
    let query = "project: {Русский проект} #Unresolved & text: \"a+b?\"";
    let result = client.search_issues(query, page).await.unwrap();
    assert_eq!(result["count"], 1);
    assert_eq!(result["next_offset"], 4);
    assert_eq!(result["offset"], 3);
    assert_eq!(result["limit"], 7);
    let issue = client.get_issue("DEMO-1").await.unwrap();
    assert_eq!(issue["customFields"][0]["value"], 42);
    assert_eq!(issue["customFields"][1]["value"], "значение");
    assert!(issue["customFields"][2]["value"].is_null());
    assert_eq!(issue["customFields"][3]["value"]["login"], "reader");
    assert_eq!(
        issue["customFields"][4]["value"].as_array().unwrap().len(),
        2
    );
    assert_eq!(issue["customFields"][5]["value"]["minutes"], 90);
    client.list_issue_comments("DEMO-1", page).await.unwrap();
    client.list_issue_links("DEMO-1", page).await.unwrap();
    client
        .list_linked_issues("DEMO-1", "80-1s", page)
        .await
        .unwrap();
    client.list_projects(page).await.unwrap();
    client.list_project_fields("0-1", page).await.unwrap();
    assert_eq!(client.get_current_user().await.unwrap()["login"], "reader");
    client.list_issue_attachments("DEMO-1", page).await.unwrap();
    let empty = client
        .search_issues(
            "",
            Page {
                limit: 20,
                offset: 100,
            },
        )
        .await
        .unwrap();
    assert!(empty["items"].as_array().unwrap().is_empty());
    assert!(empty["next_offset"].is_null());
    mock.assert_read_only();

    let requests = mock.state.captured.lock().unwrap();
    let first = &requests[0];
    assert_eq!(first.query.get("query").unwrap(), query);
    assert_eq!(first.query.get("$top").unwrap(), "7");
    assert_eq!(first.query.get("$skip").unwrap(), "3");
    assert!(first.query.get("fields").unwrap().contains("customFields"));
    let routes: Vec<_> = requests.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(
        routes,
        [
            "/youtrack/api/issues",
            "/youtrack/api/issues/DEMO-1",
            "/youtrack/api/issues/DEMO-1/comments",
            "/youtrack/api/issues/DEMO-1/links",
            "/youtrack/api/issues/DEMO-1/links/80-1s/issues",
            "/youtrack/api/admin/projects",
            "/youtrack/api/admin/projects/0-1/customFields",
            "/youtrack/api/users/me",
            "/youtrack/api/issues/DEMO-1/attachments",
            "/youtrack/api/issues",
        ]
    );
}

#[tokio::test]
async fn api_errors_are_useful_and_do_not_expose_secrets() {
    for mode in [
        ApiMode::Status(400),
        ApiMode::Status(401),
        ApiMode::Status(403),
        ApiMode::Status(404),
        ApiMode::Status(429),
        ApiMode::Status(500),
        ApiMode::InvalidJson,
        ApiMode::WrongShape,
        ApiMode::Slow,
    ] {
        let mock = Mock::start(Settings {
            api_mode: mode,
            ..Settings::default()
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let error = mock
            .client(dir.path(), 100)
            .get_current_user()
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(!message.contains("test-secret"));
        assert!(!message.contains("private-sign"));
        assert!(!message.contains("http://"));
        assert!(!message.is_empty());
        mock.assert_read_only();
    }
}

#[tokio::test]
async fn download_preserves_bytes_and_never_overwrites_files() {
    let mock = Mock::start(Settings::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("downloads");
    let client = mock.client(&root, 100);
    let first = client.download_attachment("DEMO-1", "7-1").await.unwrap();
    let second = client.download_attachment("DEMO-1", "7-1").await.unwrap();
    assert_ne!(first["path"], second["path"]);
    for result in [first, second] {
        let path = Path::new(result["path"].as_str().unwrap());
        assert!(path.is_absolute());
        assert_eq!(path.parent().unwrap(), root.canonicalize().unwrap());
        assert_eq!(std::fs::read(path).unwrap(), FILE_BYTES);
        assert_eq!(result["name"], "../../отчёт.bin");
        assert_eq!(result["size"], FILE_BYTES.len());
    }
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
    mock.assert_read_only();
}

#[tokio::test]
async fn oversized_or_broken_downloads_leave_no_files() {
    for settings in [
        Settings {
            metadata_size: 20,
            ..Settings::default()
        },
        Settings {
            file_mode: FileMode::TooLarge,
            ..Settings::default()
        },
        Settings {
            file_mode: FileMode::ChunkedTooLarge,
            ..Settings::default()
        },
        Settings {
            file_mode: FileMode::Broken,
            ..Settings::default()
        },
    ] {
        let mock = Mock::start(settings).await;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("downloads");
        assert!(
            mock.client(&root, 10)
                .download_attachment("DEMO-1", "7-1")
                .await
                .is_err()
        );
        assert!(!root.exists() || std::fs::read_dir(root).unwrap().count() == 0);
        mock.assert_read_only();
    }
}

#[tokio::test]
async fn download_cancellation_and_timeout_remove_partial_files() {
    let mock = Mock::start(Settings {
        file_mode: FileMode::Pending,
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("downloads");
    let client = mock.client(&root, 100);
    let cloned = client.clone();
    let task = tokio::spawn(async move { cloned.download_attachment("DEMO-1", "7-1").await });
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if root.exists() && std::fs::read_dir(&root).unwrap().count() > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    let error = client
        .download_attachment("DEMO-1", "7-1")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("время ожидания"));
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
}

#[tokio::test]
async fn attachment_urls_and_redirects_are_confined_to_the_origin() {
    for location in [
        "http://localhost:9/private-sign",
        "//localhost:9/private-sign",
        "/youtrack/api/files/redirect-out?sign=private-sign",
        "/youtrack/api/files/loop?sign=private-sign",
    ] {
        let mock = Mock::start(Settings {
            location: location.into(),
            ..Settings::default()
        })
        .await;
        let dir = tempfile::tempdir().unwrap();
        let error = mock
            .client(dir.path(), 100)
            .download_attachment("DEMO-1", "7-1")
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(!message.contains("private-sign"));
        assert!(!message.contains("test-secret"));
        assert!(message.contains("origin") || message.contains("редирект"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        mock.assert_read_only();
    }
    let mock = Mock::start(Settings {
        location: "/youtrack/api/files/redirect".into(),
        ..Settings::default()
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    mock.client(dir.path(), 100)
        .download_attachment("DEMO-1", "7-1")
        .await
        .unwrap();
    assert_eq!(mock.state.captured.lock().unwrap().len(), 3);
    mock.assert_read_only();
}

async fn read_message<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Value {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn mcp_handshake_tools_annotations_calls_and_eof() {
    let mock = Mock::start(Settings::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let server = YouTrackServer::new(mock.client(dir.path(), 100));
    let (client_io, server_io) = tokio::io::duplex(65536);
    let task = tokio::spawn(async move {
        server
            .serve(server_io)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap()
    });
    let (read, mut write) = tokio::io::split(client_io);
    let mut reader = BufReader::new(read);
    let initialize = json!({
        "jsonrpc":"2.0", "id":1, "method":"initialize",
        "params":{"protocolVersion":"2024-11-05","capabilities":{},
                  "clientInfo":{"name":"test","version":"1.0"}}
    });
    write
        .write_all(format!("{initialize}\n").as_bytes())
        .await
        .unwrap();
    let initialized = read_message(&mut reader).await;
    assert_eq!(initialized["result"]["serverInfo"]["name"], "yt-mcp");
    write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n")
        .await
        .unwrap();
    let tools = read_message(&mut reader).await;
    let tools = tools["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 10);
    for tool in tools {
        let download = tool["name"] == "download_attachment";
        assert_eq!(tool["annotations"]["readOnlyHint"], !download);
        assert_eq!(tool["annotations"]["destructiveHint"], false);
        assert_eq!(tool["annotations"]["idempotentHint"], !download);
        assert!(tool["inputSchema"].is_object());
    }
    let call = json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
        "params":{"name":"get_current_user","arguments":{}}});
    write
        .write_all(format!("{call}\n").as_bytes())
        .await
        .unwrap();
    let result = read_message(&mut reader).await;
    assert_eq!(result["id"], 3);
    assert_eq!(result["result"]["isError"], false);
    let user: Value =
        serde_json::from_str(result["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(user["login"], "reader");

    let call = json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
        "params":{"name":"search_issues","arguments":{"query":"for: me"}}});
    write
        .write_all(format!("{call}\n").as_bytes())
        .await
        .unwrap();
    let result = read_message(&mut reader).await;
    assert_eq!(result["result"]["isError"], false);
    let page: Value =
        serde_json::from_str(result["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(page["limit"], 20);
    assert_eq!(page["offset"], 0);

    let call = json!({"jsonrpc":"2.0","id":5,"method":"tools/call",
        "params":{"name":"get_issue","arguments":{"issue_id":"../users/me"}}});
    write
        .write_all(format!("{call}\n").as_bytes())
        .await
        .unwrap();
    let result = read_message(&mut reader).await;
    assert_eq!(result["result"]["isError"], true);

    write.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    mock.assert_read_only();
}
