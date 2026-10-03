//! Пользователь сессии: автор коммитов и `GITHUB_TOKEN`, которые worker передал при её создании. Токен зашифрован
//! тем же ключом, что значения окружения, и привязан к сессии через AAD.

#[cfg(test)]
#[path = "../../../tests/server/registry/identity.rs"]
mod tests;

use nerve::fingerprint::Fingerprint;
use nerve::protocol::{Author, SessionIdentity};
use rusqlite::{OptionalExtension, params};

use super::sessions::find_session;
use super::{Error, Registry, Token};

/// Имя и почта автора, зашифрованный токен.
type StoredIdentity = (Option<String>, Option<String>, Option<Vec<u8>>);

fn identity_aad(session: &Fingerprint) -> Vec<u8> {
    [b"github:".as_slice(), session.as_str().as_bytes()].concat()
}

impl Registry {
    /// Запоминает пользователя сессии; прежняя запись заменяется.
    pub(in crate::server) fn set_session_identity(&mut self, session: &Fingerprint, identity: &SessionIdentity) -> Result<(), Error> {
        find_session(&self.db, session)?;
        let sealed = match &identity.github_token {
            Some(token) => Some(self.cipher.seal(&Token::new(token), &identity_aad(session))?),
            None => None,
        };
        self.db.execute(
            "INSERT INTO session_identity (session, author_name, author_email, github_token_sealed) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(session) DO UPDATE SET author_name = excluded.author_name, author_email = excluded.author_email, \
             github_token_sealed = excluded.github_token_sealed",
            params![
                session.as_str(),
                identity.author.as_ref().map(|author| author.name.as_str()),
                identity.author.as_ref().map(|author| author.email.as_str()),
                sealed,
            ],
        )?;
        Ok(())
    }

    /// Пользователь сессии; без записи — пустой.
    pub(in crate::server) fn session_identity(&self, session: &Fingerprint) -> Result<SessionIdentity, Error> {
        let row: Option<StoredIdentity> = self
            .db
            .query_row(
                "SELECT author_name, author_email, github_token_sealed FROM session_identity WHERE session = ?1",
                [session.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((name, email, sealed)) = row else {
            return Ok(SessionIdentity::default());
        };
        let github_token = match sealed {
            Some(blob) => Some(self.cipher.open(&blob, &identity_aad(session))?.expose().to_owned()),
            None => None,
        };
        Ok(SessionIdentity {
            author: name.zip(email).map(|(name, email)| Author { name, email }),
            github_token,
        })
    }
}
