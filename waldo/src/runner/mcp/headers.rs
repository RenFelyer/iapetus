//! Заголовки сетевого сервера: `headers` из `.mcp.json` и вывод `headersHelper`.
//!
//! `headersHelper` — команда оболочки (`bash -c`), которая печатает в stdout JSON-объект «имя заголовка — строка».
//! Она запускается в каталоге проекта с окружением команд проекта, как stdio-сервер, плюс
//! `CLAUDE_CODE_MCP_SERVER_NAME` и `CLAUDE_CODE_MCP_SERVER_URL`; живёт не дольше `HELPER_TIMEOUT`. Запускается при
//! каждом подключении, результат не кэшируется; заголовки из вывода перекрывают одноимённые из `headers`. Значения
//! заголовков из вывода — секреты. В ошибках helper — только хвост stderr, но не его stdout.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use reqwest::header::HeaderMap;
use rustix::process::{Pid, Signal, kill_process_group};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::timeout;

use super::http::header_map;
use crate::runner::reaper;

/// Сколько helper печатает заголовки.
const HELPER_TIMEOUT: Duration = Duration::from_secs(10);

/// Сколько читать из stdout и stderr helper.
const MAX_OUTPUT: u64 = 1 << 20;

/// Сколько stderr helper попадает в текст ошибки.
const MAX_STDERR_IN_ERROR: usize = 500;

/// Как запустить `headersHelper` одного сервера.
#[derive(Debug)]
pub(super) struct Helper {
    pub(super) name: String,
    pub(super) command: String,
    pub(super) url: String,
    pub(super) dir: PathBuf,
    pub(super) vars: BTreeMap<OsString, OsString>,
}

/// Заголовки одного подключения и значения из вывода helper.
pub(super) struct Resolved {
    pub(super) headers: HeaderMap,
    pub(super) learned: Vec<Vec<u8>>,
}

/// Статические заголовки записи и, если есть, helper.
#[derive(Debug)]
pub(super) struct Headers {
    fixed: HeaderMap,
    helper: Option<Helper>,
}

impl Headers {
    pub(super) fn new(fixed: &BTreeMap<String, String>, helper: Option<Helper>) -> Result<Self, String> {
        Ok(Self {
            fixed: header_map(fixed)?,
            helper,
        })
    }

    /// Заголовки для нового подключения; helper запускается заново.
    pub(super) async fn resolve(&self) -> Result<Resolved, String> {
        let mut headers = self.fixed.clone();
        let mut learned = Vec::new();
        if let Some(helper) = &self.helper {
            let dynamic = helper.run().await?;
            let parsed = header_map(&dynamic).map_err(|reason| helper.failed(&reason))?;
            for (name, value) in &parsed {
                headers.insert(name.clone(), value.clone());
            }
            for value in dynamic.values() {
                learned.push(value.as_bytes().to_vec());
                // Сервер может вернуть только токен без схемы.
                if let Some((_, token)) = value.rsplit_once(' ') {
                    learned.push(token.as_bytes().to_vec());
                }
            }
        }
        Ok(Resolved { headers, learned })
    }
}

impl Helper {
    fn failed(&self, reason: &str) -> String {
        format!("headersHelper сервера {}: {reason}", self.name)
    }

    /// Запускает команду и разбирает её вывод. Ошибки не маскируются: их выводят места, которые знают секреты сервера.
    async fn run(&self) -> Result<BTreeMap<String, String>, String> {
        let mut child = Command::new("bash")
            .args(["-c", &self.command])
            .current_dir(&self.dir)
            .env_clear()
            .envs(&self.vars)
            .env("CLAUDE_CODE_MCP_SERVER_NAME", &self.name)
            .env("CLAUDE_CODE_MCP_SERVER_URL", &self.url)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Своя группа: по таймауту убивается и всё, что helper запустил.
            .process_group(0)
            .spawn()
            .map_err(|e| self.failed(&format!("не запущена: {e}")))?;
        let group = child.id().and_then(|pid| Pid::from_raw(i32::try_from(pid).ok()?));
        let (Some(group), Some(stdout), Some(stderr)) = (group, child.stdout.take(), child.stderr.take()) else {
            child.kill().await.unwrap_or_default();
            return Err(self.failed("у процесса нет pid, stdout или stderr"));
        };
        let _own = reaper::own_child(group.as_raw_nonzero().get().unsigned_abs());
        let finished = timeout(HELPER_TIMEOUT, async {
            let (out, err) = tokio::join!(read_all(stdout), read_all(stderr));
            (child.wait().await, out, err)
        })
        .await;
        kill_process_group(group, Signal::KILL).unwrap_or_default();
        let Ok((status, out, err)) = finished else {
            child.wait().await.unwrap_or_default();
            return Err(self.failed(&format!("не завершилась за {} с", HELPER_TIMEOUT.as_secs())));
        };
        let status = status.map_err(|e| self.failed(&format!("ожидание не удалось: {e}")))?;
        if !status.success() {
            let tail = String::from_utf8_lossy(&err[..err.len().min(MAX_STDERR_IN_ERROR)])
                .trim()
                .to_owned();
            return Err(self.failed(&format!("завершилась с {status}: {tail}")));
        }
        serde_json::from_slice(&out).map_err(|_| self.failed("вывод — не JSON-объект из строк"))
    }
}

async fn read_all(stream: impl AsyncRead + Unpin) -> Vec<u8> {
    let mut data = Vec::new();
    stream.take(MAX_OUTPUT).read_to_end(&mut data).await.unwrap_or_default();
    data
}

#[cfg(test)]
#[path = "../../../tests/runner/mcp/headers.rs"]
mod tests;
