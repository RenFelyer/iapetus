//! Узлы: регистрация runner, открытые ключи и сертификаты, отзыв.

#[cfg(test)]
#[path = "../../../tests/server/registry/nodes.rs"]
mod tests;

use nerve::fingerprint::Fingerprint;
use nerve::protocol::Assignment;
use rusqlite::{Connection, OptionalExtension, Row, params};
use russh::keys::PublicKey;

use super::sessions::{assigned_session, assignment_from_row};
use super::{Entity, Error, Registry, begin, fingerprint_from_row};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::server) struct NodeInfo {
    pub fingerprint: Fingerprint,
    pub name: String,
    pub os: String,
    /// Клон, созданный от другого runner.
    pub temporary: bool,
    /// У клона — runner, от которого он создан.
    pub parent: Option<Fingerprint>,
    /// Отозван: запись остаётся, сертификат не продлевается и не выдаётся заново.
    pub revoked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::server) struct RunnerInfo {
    pub node: NodeInfo,
    pub session: Option<Fingerprint>,
    /// Номер идущего назначения; есть, когда есть `session`.
    pub assignment: Option<Assignment>,
    /// Номер последней пары runner, идущей или завершённой: им server сообщает `End`, когда runner свободен.
    pub last_assignment: Assignment,
}

/// Итог отзыва: отозванные узлы и сессии, потерявшие runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::server) struct Revocation {
    pub nodes: Vec<Fingerprint>,
    pub sessions: Vec<Fingerprint>,
}

/// Описание runner для регистрации.
#[derive(Debug, Clone)]
pub(in crate::server) struct NewNode {
    /// Придуман самим узлом.
    pub fingerprint: Fingerprint,
    pub public_key: PublicKey,
    pub name: String,
    pub os: String,
    /// Для клона — runner, от которого он создан.
    pub parent: Option<Fingerprint>,
}

/// Последний сертификат, выданный узлу; сам сертификат хранит узел. Секунды Unix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::server) struct CertificateRecord {
    pub issued_at: i64,
    pub valid_before: i64,
}

const NODE_COLUMNS: &str = "fingerprint, name, os, temporary, revoked, parent";

fn node_from_row(row: &Row<'_>) -> rusqlite::Result<NodeInfo> {
    Ok(NodeInfo {
        fingerprint: fingerprint_from_row(row, 0)?,
        name: row.get(1)?,
        os: row.get(2)?,
        temporary: row.get(3)?,
        revoked: row.get(4)?,
        parent: match row.get::<_, Option<String>>(5)? {
            Some(_) => Some(fingerprint_from_row(row, 5)?),
            None => None,
        },
    })
}

/// Ключ без комментария: один и тот же ключ записывается одной строкой.
fn public_key_text(key: &PublicKey) -> Result<String, Error> {
    PublicKey::from(key.key_data().clone())
        .to_openssh()
        .map_err(|_| Error::InvalidPublicKey)
}

pub(super) fn find_node(db: &Connection, fingerprint: &Fingerprint) -> Result<NodeInfo, Error> {
    db.query_row(
        &format!("SELECT {NODE_COLUMNS} FROM nodes WHERE fingerprint = ?1"),
        [fingerprint.as_str()],
        node_from_row,
    )
    .optional()?
    .ok_or(Error::NotFound(Entity::Node))
}

impl Registry {
    /// Регистрирует runner под его fingerprint и открытым ключом. Повтор с тем же ключом безопасен и обновляет
    /// имя и ОС; тот же fingerprint с другим ключом — отказ. Имена не уникальны (клоны одного каталога носят одно
    /// имя): runner различают по fingerprint. Родитель клона — зарегистрированный, не отозванный и не клон.
    pub(in crate::server) fn register_node(&mut self, node: &NewNode) -> Result<NodeInfo, Error> {
        let tx = begin(&mut self.db)?;
        if node.name.is_empty() {
            return Err(Error::InvalidName);
        }
        if let Some(parent) = &node.parent {
            let parent = find_node(&tx, parent).map_err(|e| match e {
                Error::NotFound(_) => Error::InvalidParent,
                other => other,
            })?;
            if parent.revoked || parent.temporary {
                return Err(Error::InvalidParent);
            }
        }
        let key = public_key_text(&node.public_key)?;
        let session_taken = tx
            .query_row("SELECT 1 FROM sessions WHERE fingerprint = ?1", [node.fingerprint.as_str()], |_| {
                Ok(())
            })
            .optional()?;
        if session_taken.is_some() {
            return Err(Error::FingerprintTaken);
        }
        let existing: Option<String> = tx
            .query_row(
                "SELECT public_key FROM nodes WHERE fingerprint = ?1",
                [node.fingerprint.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(stored_key) = existing {
            if stored_key != key {
                return Err(Error::FingerprintTaken);
            }
            if find_node(&tx, &node.fingerprint)?.revoked {
                return Err(Error::Revoked);
            }
            tx.execute(
                "UPDATE nodes SET name = ?1, os = ?2 WHERE fingerprint = ?3",
                params![node.name, node.os, node.fingerprint.as_str()],
            )?;
        } else {
            let key_used = tx
                .query_row("SELECT 1 FROM nodes WHERE public_key = ?1", [&key], |_| Ok(()))
                .optional()?;
            if key_used.is_some() {
                return Err(Error::KeyTaken);
            }
            tx.execute(
                "INSERT INTO nodes (fingerprint, name, os, public_key, temporary, parent) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    node.fingerprint.as_str(),
                    node.name,
                    node.os,
                    key,
                    node.parent.is_some(),
                    node.parent.as_ref().map(Fingerprint::as_str),
                ],
            )?;
        }
        let info = find_node(&tx, &node.fingerprint)?;
        tx.commit()?;
        Ok(info)
    }

    /// Открытый ключ узла: по нему проверяется подпись запроса продления.
    pub(in crate::server) fn node_public_key(&self, fingerprint: &Fingerprint) -> Result<PublicKey, Error> {
        self.active_node(fingerprint)?;
        let text: String = self
            .db
            .query_row(
                "SELECT public_key FROM nodes WHERE fingerprint = ?1",
                [fingerprint.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(Error::NotFound(Entity::Node))?;
        PublicKey::from_openssh(&text).map_err(|_| Error::InvalidPublicKey)
    }

    /// Запоминает последний сертификат, выданный узлу; прежняя запись заменяется.
    pub(in crate::server) fn record_certificate(&mut self, fingerprint: &Fingerprint, record: CertificateRecord) -> Result<(), Error> {
        let tx = begin(&mut self.db)?;
        find_node(&tx, fingerprint)?;
        tx.execute(
            "INSERT INTO node_certificates (node, issued_at, valid_before) VALUES (?1, ?2, ?3) \
             ON CONFLICT(node) DO UPDATE SET issued_at = excluded.issued_at, valid_before = excluded.valid_before",
            params![fingerprint.as_str(), record.issued_at, record.valid_before],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Узел, которому ещё можно входить: он есть в реестре и не отозван.
    pub(in crate::server) fn active_node(&self, fingerprint: &Fingerprint) -> Result<NodeInfo, Error> {
        let node = find_node(&self.db, fingerprint)?;
        if node.revoked {
            return Err(Error::Revoked);
        }
        Ok(node)
    }

    /// Сессия, в которую пускают вошедший runner: та, которой он назначен. Цель он не выбирает.
    pub(in crate::server) fn runner_destination(&self, runner: &Fingerprint) -> Result<Fingerprint, Error> {
        self.active_node(runner)?;
        assigned_session(&self.db, runner)?.ok_or(Error::NotLinked)
    }

    pub(in crate::server) fn node(&self, fingerprint: &Fingerprint) -> Result<NodeInfo, Error> {
        find_node(&self.db, fingerprint)
    }

    /// Переименование не трогает fingerprint, а с ним ключи окружения и кэш.
    pub(in crate::server) fn rename_node(&mut self, fingerprint: &Fingerprint, name: &str) -> Result<(), Error> {
        let tx = begin(&mut self.db)?;
        find_node(&tx, fingerprint)?;
        if name.is_empty() {
            return Err(Error::InvalidName);
        }
        tx.execute(
            "UPDATE nodes SET name = ?1 WHERE fingerprint = ?2",
            params![name, fingerprint.as_str()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Отзывает узел вместе с его клонами: запись остаётся с пометкой, продлить сертификат и зарегистрироваться
    /// заново узел больше не сможет. Назначения снимаются; возвращает отозванные узлы (первым — сам) и сессии, которые
    /// потеряли runner.
    pub(in crate::server) fn revoke_node(&mut self, fingerprint: &Fingerprint) -> Result<Revocation, Error> {
        let tx = begin(&mut self.db)?;
        find_node(&tx, fingerprint)?;
        let mut nodes = vec![fingerprint.clone()];
        let mut stmt = tx.prepare("SELECT fingerprint FROM nodes WHERE parent = ?1 AND revoked = 0 ORDER BY fingerprint")?;
        let clones = stmt.query_map([fingerprint.as_str()], |row| fingerprint_from_row(row, 0))?;
        for clone in clones {
            nodes.push(clone?);
        }
        drop(stmt);
        let mut sessions = Vec::new();
        for node in &nodes {
            tx.execute("UPDATE nodes SET revoked = 1 WHERE fingerprint = ?1", [node.as_str()])?;
            sessions.extend(assigned_session(&tx, node)?);
            tx.execute("DELETE FROM assignments WHERE runner = ?1", [node.as_str()])?;
        }
        tx.commit()?;
        Ok(Revocation { nodes, sessions })
    }

    pub(in crate::server) fn runner(&self, fingerprint: &Fingerprint) -> Result<RunnerInfo, Error> {
        let node = find_node(&self.db, fingerprint)?;
        let last_assignment = self.db.query_row(
            "SELECT last_epoch FROM nodes WHERE fingerprint = ?1",
            [fingerprint.as_str()],
            |row| assignment_from_row(row, 0),
        )?;
        let assigned = self
            .db
            .query_row(
                "SELECT session, epoch FROM assignments WHERE runner = ?1",
                [fingerprint.as_str()],
                |row| Ok((fingerprint_from_row(row, 0)?, assignment_from_row(row, 1)?)),
            )
            .optional()?;
        let (session, assignment) = assigned.unzip();
        Ok(RunnerInfo {
            node,
            session,
            assignment,
            last_assignment,
        })
    }

    /// Все runner для `runner_list`.
    pub(in crate::server) fn runners(&self) -> Result<Vec<RunnerInfo>, Error> {
        let mut stmt = self.db.prepare(
            "SELECT n.fingerprint, n.name, n.os, n.temporary, n.revoked, n.parent, a.session, a.epoch, n.last_epoch \
             FROM nodes n LEFT JOIN assignments a ON a.runner = n.fingerprint ORDER BY n.fingerprint",
        )?;
        let rows = stmt.query_map([], |row| {
            let node = node_from_row(row)?;
            let (session, assignment) = match row.get::<_, Option<String>>(6)? {
                Some(_) => (Some(fingerprint_from_row(row, 6)?), Some(assignment_from_row(row, 7)?)),
                None => (None, None),
            };
            Ok(RunnerInfo {
                node,
                session,
                assignment,
                last_assignment: assignment_from_row(row, 8)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}
