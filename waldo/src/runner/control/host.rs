//! Окружение runner, которое он сообщает server при подключении: session воспроизводит его для агента.

use std::path::Path;

use nerve::protocol::{MAX_OS_RELEASE, RunnerHost};

/// Файлы с описанием ОС: по порядку, как их ищут программы.
const OS_RELEASE_FILES: [&str; 2] = ["/etc/os-release", "/usr/lib/os-release"];

/// Путь проекта, имя машины, описание ОС и `$HOME` этого процесса: канонический, с исходной записью, если она другая.
/// Чего нет или что не читается, остаётся пустым. `home_served` — runner отдаёт `$HOME` по файловым запросам.
pub(in crate::runner) fn collect(workdir: &Path, home_served: bool) -> RunnerHost {
    let hostname = rustix::system::uname().nodename().to_string_lossy().into_owned();
    let os_release = OS_RELEASE_FILES
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .filter(|text| text.len() <= MAX_OS_RELEASE)
        .unwrap_or_default();
    let origin = std::env::var("HOME").unwrap_or_default();
    let canonical = std::fs::canonicalize(&origin)
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok());
    let (home, home_origin) = match canonical {
        Some(canonical) if canonical != origin => (canonical, origin),
        _ => (origin, String::new()),
    };
    RunnerHost {
        project: workdir.to_string_lossy().into_owned(),
        hostname,
        os_release,
        home,
        home_origin,
        home_served,
    }
}
