//! Изменение, ответ на которое потерян: результат узнаётся по состоянию runner после переподключения.
//!
//! Сверка решает одно из трёх: изменение уже выполнено (успех без повтора), не выполнено (повтор) или состояние
//! иное (`Conflict`). Журнала операций на runner нет.

use nerve::protocol::{Attr, Error, Expect, FsReply, FsRequest, FsResponse, Kind};

use super::{SETTLE_ATTEMPTS, Seen, Vfs};

/// Что запрос должен был изменить на runner: по этому состояние после обрыва узнаёт, выполнен ли он.
#[derive(Debug)]
pub(super) enum Intent<'a> {
    /// Запись `written` (sha256) поверх версии `base`.
    Write { path: &'a str, base: Expect, written: [u8; 32] },
    /// Перенос файла с хешем `from_hash` в цель, какой её ждёт `to`.
    Rename {
        from: &'a str,
        to: &'a str,
        from_hash: [u8; 32],
        expect_to: Expect,
    },
    /// Удаление; `seen` — что агент знал о пути.
    Remove { path: &'a str, seen: Option<Seen> },
}

/// Решение сверки.
pub(super) enum Verdict {
    /// Изменение выполнено; ответ, который должен был прийти.
    Done(FsReply),
    /// Изменение не выполнено, состояние прежнее: запрос повторяется.
    Repeat,
}

/// Почему сверка не дала решения.
pub(super) enum Interrupt {
    /// Ответ на саму сверку потерян: она повторяется после переподключения.
    Lost,
    Failed(Error),
}

impl From<Error> for Interrupt {
    fn from(error: Error) -> Self {
        Self::Failed(error)
    }
}

pub(super) type Probe<T> = Result<T, Interrupt>;

impl Vfs {
    /// Запрос, ответ на который мог потеряться. Потерян — [`Vfs::settle`].
    /// Запрос строится по `request` при каждой отправке, чтобы не держать копию большого содержимого про запас.
    pub(super) async fn call_settled(&self, request: impl Fn() -> FsRequest, intent: Intent<'_>) -> FsResponse {
        match self.remote.call(request()).await {
            Ok(response) => response,
            Err(error) if error.reply_may_be_lost() => {
                tracing::warn!(%error, ?intent, "ответ потерян: результат узнаётся по состоянию на runner");
                self.settle(&request, &intent).await
            }
            Err(error) => {
                tracing::warn!(?intent, %error, "запрос не отправлен");
                Err(Error::Io(error.to_string()))
            }
        }
    }

    /// Сверяет состояние и при необходимости повторяет запрос. Отказ повтора из `reprobe` не доказывает, что первая
    /// попытка не удалась (состояние мог сменить кто-то другой или она удалась), поэтому после него сверка идёт снова.
    async fn settle(&self, request: &impl Fn() -> FsRequest, intent: &Intent<'_>) -> FsResponse {
        for _ in 0..SETTLE_ATTEMPTS {
            if !self.remote.wait_open().await {
                return Err(Error::Io("канал данных не восстановился: результат неизвестен".to_owned()));
            }
            match self.verdict(intent).await {
                Ok(Verdict::Done(reply)) => return Ok(reply),
                Ok(Verdict::Repeat) => {}
                Err(Interrupt::Lost) => continue,
                Err(Interrupt::Failed(error)) => return Err(error),
            }
            tracing::info!(?intent, "изменение не дошло до runner: повтор");
            match self.remote.call(request()).await {
                Ok(Err(error)) if reprobe(intent).contains(&error) => {}
                Ok(response) => return response,
                Err(error) if error.reply_may_be_lost() => {}
                Err(error) => return Err(Error::Io(error.to_string())),
            }
        }
        Err(Error::Io("результат не установлен: связь с runner рвётся".to_owned()))
    }

    async fn verdict(&self, intent: &Intent<'_>) -> Probe<Verdict> {
        match *intent {
            Intent::Write { path, base, written } => match (self.probe_hash(path).await?, base) {
                (Some((attr, sha256)), _) if sha256 == written => Ok(Verdict::Done(FsReply::Written { attr, sha256 })),
                (Some((_, sha256)), Expect::Hash(old)) if sha256 == old => Ok(Verdict::Repeat),
                (None, Expect::Absent) => Ok(Verdict::Repeat),
                _ => Err(Interrupt::Failed(Error::Conflict)),
            },
            Intent::Rename {
                from,
                to,
                from_hash,
                expect_to,
            } => {
                let source = self.probe_hash(from).await?;
                let target = self.probe_hash(to).await?;
                match (source, target, expect_to) {
                    // Источника нет, а на цели его содержимое: перенос выполнен.
                    (None, Some((attr, hash)), _) if hash == from_hash => Ok(Verdict::Done(FsReply::Renamed(attr))),
                    (Some((_, hash)), None, Expect::Absent) if hash == from_hash => Ok(Verdict::Repeat),
                    (Some((_, hash)), Some((_, current)), Expect::Hash(wanted)) if hash == from_hash && current == wanted => {
                        Ok(Verdict::Repeat)
                    }
                    _ => Err(Interrupt::Failed(Error::Conflict)),
                }
            }
            Intent::Remove { path, seen } => {
                let Some(attr) = self.probe_stat(path).await? else {
                    return Ok(Verdict::Done(FsReply::Removed));
                };
                // Агент видел файл с этим хешем: если там другой, удалять чужое нельзя.
                match seen {
                    Some(Seen::Hash(hash)) if attr.kind == Kind::File => match self.probe_hash(path).await? {
                        None => Ok(Verdict::Done(FsReply::Removed)),
                        Some((_, current)) if current == hash => Ok(Verdict::Repeat),
                        Some(_) => Err(Interrupt::Failed(Error::Conflict)),
                    },
                    Some(Seen::Hash(_) | Seen::Absent) => Err(Interrupt::Failed(Error::Conflict)),
                    None => Ok(Verdict::Repeat),
                }
            }
        }
    }

    /// Хеш и атрибуты файла на runner; `None` — файла нет.
    pub(super) async fn probe_hash(&self, path: &str) -> Probe<Option<(Attr, [u8; 32])>> {
        match self.remote.call(FsRequest::Hash { path: path.to_owned() }).await {
            Ok(Ok(FsReply::Hashed { attr, sha256 })) => Ok(Some((attr, sha256))),
            Ok(Err(Error::NotFound)) => Ok(None),
            Ok(Ok(_)) => Err(Error::Io("runner ответил на сверку хеша не хешем".to_owned()).into()),
            Ok(Err(error)) => Err(error.into()),
            Err(error) if error.reply_may_be_lost() => Err(Interrupt::Lost),
            Err(error) => Err(Error::Io(error.to_string()).into()),
        }
    }

    /// Атрибуты пути на runner; `None` — пути нет.
    pub(super) async fn probe_stat(&self, path: &str) -> Probe<Option<Attr>> {
        match self.remote.call(FsRequest::Stat { path: path.to_owned() }).await {
            Ok(Ok(FsReply::Stat(attr))) => Ok(Some(attr)),
            Ok(Err(Error::NotFound)) => Ok(None),
            Ok(Ok(_)) => Err(Error::Io("runner ответил на сверку пути не атрибутами".to_owned()).into()),
            Ok(Err(error)) => Err(error.into()),
            Err(error) if error.reply_may_be_lost() => Err(Interrupt::Lost),
            Err(error) => Err(Error::Io(error.to_string()).into()),
        }
    }
}

/// Отказы повторного запроса, после которых сверка идёт снова: они не доказывают, что первая попытка не удалась.
fn reprobe(intent: &Intent<'_>) -> &'static [Error] {
    match intent {
        Intent::Write { .. } => &[Error::Conflict],
        Intent::Rename { .. } => &[Error::Conflict, Error::NotFound],
        Intent::Remove { .. } => &[Error::NotFound],
    }
}
