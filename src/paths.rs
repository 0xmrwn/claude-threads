use crate::error::{AppError, ErrorCode, internal, io_error};
use camino::Utf8PathBuf;
use serde::Serialize;
use std::env;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedPaths {
    pub claude_home: Utf8PathBuf,
    pub projects_root: Utf8PathBuf,
    pub history_path: Utf8PathBuf,
    /// Claude desktop app data directory (`~/Library/Application Support/Claude`
    /// on macOS). `None` when it cannot be resolved on this platform.
    pub desktop_home: Option<Utf8PathBuf>,
    /// Cowork session store: `{desktop_home}/local-agent-mode-sessions`.
    pub cowork_root: Option<Utf8PathBuf>,
    /// Desktop Code tab session metadata: `{desktop_home}/claude-code-sessions`.
    pub desktop_sessions_root: Option<Utf8PathBuf>,
    pub index_dir: Utf8PathBuf,
    pub index_path: Utf8PathBuf,
}

impl ResolvedPaths {
    pub fn discover() -> Result<Self, AppError> {
        let claude_home = if let Some(value) = env::var_os("CLAUDE_HOME") {
            utf8_from_path(PathBuf::from(value), "CLAUDE_HOME")?
        } else {
            let home = env::var_os("HOME")
                .ok_or_else(|| AppError::new(ErrorCode::ArchiveNotFound, "HOME is not set"))?;
            let mut path = PathBuf::from(home);
            path.push(".claude");
            utf8_from_path(path, "HOME/.claude")?
        };

        let desktop_home = if let Some(value) = env::var_os("CLAUDE_DESKTOP_HOME") {
            Some(utf8_from_path(PathBuf::from(value), "CLAUDE_DESKTOP_HOME")?)
        } else {
            directories::BaseDirs::new()
                .and_then(|dirs| Utf8PathBuf::from_path_buf(dirs.data_dir().join("Claude")).ok())
        };

        let projects_root = claude_home.join("projects");
        let history_path = claude_home.join("history.jsonl");
        let cowork_root = desktop_home
            .as_ref()
            .map(|home| home.join("local-agent-mode-sessions"));
        let desktop_sessions_root = desktop_home
            .as_ref()
            .map(|home| home.join("claude-code-sessions"));
        let index_dir = claude_home.join("claude-threads");
        let index_path = index_dir.join("index.sqlite");

        Ok(Self {
            claude_home,
            projects_root,
            history_path,
            desktop_home,
            cowork_root,
            desktop_sessions_root,
            index_dir,
            index_path,
        })
    }

    pub fn ensure_index_dir(&self) -> Result<(), AppError> {
        std::fs::create_dir_all(&self.index_dir)
            .map_err(|error| io_error("failed to create index directory", error))
    }
}

fn utf8_from_path(path: PathBuf, label: &str) -> Result<Utf8PathBuf, AppError> {
    Utf8PathBuf::from_path_buf(path)
        .map_err(|_| internal(format!("resolved {label} path is not valid UTF-8")))
}
