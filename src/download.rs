use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::Path;
use tokio::io::AsyncWriteExt;

use crate::youtrack::{YouTrack, http_error};

impl YouTrack {
    pub async fn download_attachment(&self, issue_id: &str, attachment_id: &str) -> Result<Value> {
        let attachment = self.attachment(issue_id, attachment_id).await?;
        let limit = self.config().max_attachment_bytes;
        if attachment.size > limit {
            bail!("Вложение превышает max_attachment_bytes ({limit} байт)");
        }
        let location = attachment.url.context("URL вложения отсутствует")?;
        let mut response = self.attachment_response(&location).await?;
        if response.content_length().is_some_and(|size| size > limit) {
            bail!("Вложение превышает max_attachment_bytes ({limit} байт)");
        }

        tokio::fs::create_dir_all(&self.config().download_dir)
            .await
            .context("Не удалось создать каталог загрузок")?;
        let root = tokio::fs::canonicalize(&self.config().download_dir)
            .await
            .context("Не удалось определить каталог загрузок")?;
        let safe_name = safe_filename(&attachment.name);
        // Временный файл удаляется при ошибке и при отмене будущего.
        // Имена создаются эксклюзивно; исходное имя не используется как путь.
        let temporary = tempfile::Builder::new()
            .prefix("yt-")
            .suffix(&format!("-{safe_name}"))
            .tempfile_in(&root)
            .context("Не удалось создать файл загрузки")?;
        let mut file = tokio::fs::File::from_std(
            temporary
                .reopen()
                .context("Не удалось открыть файл загрузки")?,
        );
        let mut size = 0u64;
        while let Some(chunk) = response.chunk().await.map_err(http_error)? {
            size = size
                .checked_add(chunk.len() as u64)
                .context("Некорректный размер вложения")?;
            if size > limit {
                bail!("Вложение превышает max_attachment_bytes ({limit} байт)");
            }
            file.write_all(&chunk)
                .await
                .context("Не удалось записать вложение")?;
        }
        file.flush()
            .await
            .context("Не удалось завершить запись вложения")?;
        drop(file);
        let (file, path) = temporary
            .keep()
            .map_err(|_| anyhow::anyhow!("Не удалось сохранить вложение"))?;
        drop(file);
        Ok(json!({
            "path": path,
            "name": attachment.name,
            "attachment_id": attachment.id,
            "issue_id": issue_id,
            "mime_type": attachment.mime_type,
            "size": size,
        }))
    }
}

fn safe_filename(name: &str) -> String {
    let basename = name.rsplit(['/', '\\']).next().unwrap_or("");
    let basename = Path::new(basename)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("attachment");
    let mut safe = String::new();
    for c in basename.trim_start_matches('.').chars() {
        let c = if c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') {
            '_'
        } else {
            c
        };
        if safe.len() + c.len_utf8() > 120 {
            break;
        }
        safe.push(c);
    }
    if safe.trim().is_empty() {
        "attachment".into()
    } else {
        safe
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_filenames_are_reduced_to_basenames() {
        for name in ["../../secret", r"C:\..\secret", "/secret", "..", "", "."] {
            let safe = safe_filename(name);
            assert!(!safe.is_empty());
            assert!(!safe.contains(['/', '\\']));
            assert!(!safe.starts_with('.'));
        }
        assert_eq!(safe_filename("../../отчёт.pdf"), "отчёт.pdf");
        assert!(safe_filename(&"я".repeat(400)).len() <= 120);
        assert_eq!(safe_filename("bad:\0file.txt"), "bad__file.txt");
    }
}
