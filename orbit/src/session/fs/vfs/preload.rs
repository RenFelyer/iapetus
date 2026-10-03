//! Предзагрузка: файл скачивается до того, как агент его откроет.

use std::path::{Component, Path};
use std::sync::Arc;

use super::Vfs;

/// Скачивает файл через ту из смонтированных ФС, чья точка монтирования — самый длинный префикс пути: проект внутри
/// `$HOME` лежит глубже него, и его файлы читает ФС проекта. Путь вне всех точек монтирования игнорируется.
pub(crate) async fn preload(vfses: &[Arc<Vfs>], path: &Path) {
    if let Some(vfs) = select(vfses, path) {
        vfs.preload(path).await;
    }
}

fn select<'a>(vfses: &'a [Arc<Vfs>], path: &Path) -> Option<&'a Arc<Vfs>> {
    vfses
        .iter()
        .filter_map(|vfs| Some((vfs.mounted_root()?, vfs)))
        .filter(|(root, _)| path.starts_with(root))
        .max_by_key(|(root, _)| root.components().count())
        .map(|(_, vfs)| vfs)
}

impl Vfs {
    /// Кладёт файл в кэш тем же путём, каким его откроет агент, поэтому проверка версии и watch не меняются.
    /// Путь вне рабочего каталога, относительный и скрытый от агента игнорируются; ошибки видны только в журнале.
    pub(crate) async fn preload(&self, path: &Path) {
        let Some(relative) = self.relative_to_root(path) else {
            return;
        };
        if self.is_hidden(&relative) || self.lock().inodes.at(&relative).is_some_and(|node| node.local.is_some()) {
            return;
        }
        if let Err(errno) = self.ensure_cached(&relative).await {
            tracing::debug!(path = relative, %errno, "предзагрузка файла не удалась");
        }
    }

    /// Путь файла относительно точки монтирования; `None` — путь не абсолютный, вне каталога или с `..`.
    fn relative_to_root(&self, path: &Path) -> Option<String> {
        let root = self.lock().cache.root().to_owned();
        let parts = path
            .strip_prefix(root)
            .ok()?
            .components()
            .map(|part| match part {
                Component::Normal(name) => name.to_str(),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        (!parts.is_empty()).then(|| parts.join("/"))
    }
}
