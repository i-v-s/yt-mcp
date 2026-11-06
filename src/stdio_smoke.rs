#[cfg(test)]
mod tests {
    use std::process::Command;

    #[test]
    fn binary_stdio_end_to_end() {
        let output = Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/scripts/smoke_test.py"
            ))
            .arg("--binary")
            .arg(env!("CARGO_BIN_EXE_yt-mcp"))
            .output()
            .expect("Для smoke-теста нужен Python 3");
        assert!(
            output.status.success(),
            "Smoke-тест завершился с ошибкой:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
