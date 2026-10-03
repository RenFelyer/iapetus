//! Шаблон контейнера orbit session: разбор тела `POST /containers/create` и `POST /volumes/create`
//! и сборка тела заново. Наружу идёт только то, что структуры `Create*` и `Volume*` явно
//! перечисляют; неизвестное, повторное и пропущенное поле — отказ.

use std::collections::BTreeMap;
use std::fmt;

use anyhow::{bail, ensure};
use nerve::agent::{AGENT_ROOT, Agent};
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use super::{Denied, is_name};

pub(super) const ROLE_LABEL: &str = "iapetus.role";
pub(super) const ROLE_VALUE: &str = "session";
const LABEL_PREFIX: &str = "iapetus.";
const ENV_PREFIX: &str = "IAPETUS_";
/// Метка с агентом контейнера; от неё зависит, в какой каталог монтируется том состояния.
const AGENT_LABEL: &str = "iapetus.agent";
const FUSE: &str = "/dev/fuse";
/// Единственные capabilities контейнера поверх `CapDrop=ALL`; без любой из них путь агента ломается:
/// `SYS_ADMIN` — mount(2) FUSE; `SETPCAP` — убрать `CAP_SYS_ADMIN` из bounding set дочернего процесса;
/// `SETUID` и `SETGID` — смена uid, gid и групп. Без `CHOWN` и `DAC_OVERRIDE`: точка монтирования вне
/// корня состояния агента, конфиги пишет пользователь агента, сокет оболочки создан сразу с нужным владельцем.
const CAP_ADD: [&str; 4] = ["SYS_ADMIN", "SETUID", "SETGID", "SETPCAP"];
const CAP_DROP: [&str; 1] = ["ALL"];
/// `/work` — кэш файлов, `/run/secrets` — файлы секретов сессии: оба в оперативной памяти, недоступны для запуска файлов,
/// закрыты для всех, кроме root. Размер `/run/secrets` фиксирован, размер `/work` задаёт создатель контейнера
/// десятичным числом байт, не больше [`Limits::work`].
const SECRETS_DIR: (&str, &str) = ("/run/secrets", "rw,noexec,nosuid,nodev,mode=0700,size=1m");
const WORK_DIR: &str = "/work";
const WORK_OPTIONS: &str = "rw,noexec,nosuid,nodev,mode=0700,size=";
const NO_NEW_PRIVILEGES: &str = "no-new-privileges";
const APPARMOR_PREFIX: &str = "apparmor=";
const MAX_LABELS: usize = 16;
const MAX_ENV: usize = 64;
const MAX_VALUE_LEN: usize = 8 * 1024;

/// Верхние границы лимитов контейнера сессии; сам контейнер обязан задать все четыре.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// Байты памяти. Подкачка равна памяти, то есть её нет.
    pub memory: i64,
    /// Миллиардные доли ядра CPU.
    pub nano_cpus: i64,
    /// Число процессов и потоков.
    pub pids: i64,
    /// Байты tmpfs `/work`.
    pub work: i64,
}

/// Что меняется от контейнера к контейнеру; остальное задаёт шаблон.
#[derive(Debug)]
pub(crate) struct SessionSpec<'a> {
    pub image: &'a str,
    /// Метки `iapetus.*`; метки роли и агента добавляются сами.
    pub labels: BTreeMap<String, String>,
    /// Агент контейнера: от него зависит каталог монтирования тома состояния.
    pub agent: Agent,
    pub env: Vec<String>,
    /// Том состояния агента.
    pub volume: &'a str,
    /// Размер tmpfs `/work` в байтах.
    pub work_bytes: i64,
}

/// Строгий шаблон контейнера orbit session.
#[derive(Debug, Clone)]
pub(crate) struct Template {
    images: Vec<String>,
    network: String,
    limits: Limits,
    /// Профиль `AppArmor`, который запрос может указать; без него `apparmor=…` — отказ.
    apparmor_profile: Option<String>,
}

impl Template {
    /// `images` — полные ссылки `репозиторий@sha256:…`, других образов контейнер не получит;
    /// `network` — пользовательская сеть сессий, не `host`, не `none` и не `bridge`;
    /// `apparmor_profile` — имя профиля, загруженного на хосте (`docker/apparmor/iapetus-session`), если запрос
    /// вправе его указать.
    pub(crate) fn new(images: Vec<String>, network: String, limits: Limits, apparmor_profile: Option<String>) -> anyhow::Result<Self> {
        ensure!(!images.is_empty(), "список образов пуст");
        for image in &images {
            ensure!(is_digest_reference(image), "образ {image} не задан по digest sha256");
        }
        ensure!(is_name(&network), "имя сети {network} недопустимо");
        if matches!(network.as_str(), "host" | "none" | "bridge" | "default") {
            bail!("сеть {network} не годится для сессий");
        }
        ensure!(
            limits.memory > 0 && limits.nano_cpus > 0 && limits.pids > 0 && limits.work > 0,
            "лимиты должны быть положительными"
        );
        if let Some(profile) = &apparmor_profile {
            ensure!(is_name(profile) && profile != "unconfined", "профиль AppArmor {profile} недопустим");
        }
        Ok(Self {
            images,
            network,
            limits,
            apparmor_profile,
        })
    }

    /// Тело `containers/create`, которое этот шаблон принимает: для клиента внутри server, чтобы запрос не
    /// расходился с проверкой. Лимиты контейнера равны верхним границам шаблона.
    pub(crate) fn create_request(&self, spec: &SessionSpec<'_>) -> serde_json::Value {
        let mut security_opt = vec![NO_NEW_PRIVILEGES.to_owned()];
        security_opt.extend(self.apparmor_profile.iter().map(|profile| format!("{APPARMOR_PREFIX}{profile}")));
        let mut labels = spec.labels.clone();
        let target = agent_dir(spec.agent);
        labels.insert(AGENT_LABEL.to_owned(), spec.agent.id().to_owned());
        labels.insert(ROLE_LABEL.to_owned(), ROLE_VALUE.to_owned());
        serde_json::json!({
            "Image": spec.image,
            "Labels": labels,
            "Env": spec.env,
            "HostConfig": {
                "CapAdd": CAP_ADD,
                "CapDrop": CAP_DROP,
                "SecurityOpt": security_opt,
                "Tmpfs": {
                    SECRETS_DIR.0: SECRETS_DIR.1,
                    WORK_DIR: format!("{WORK_OPTIONS}{}", spec.work_bytes),
                },
                "Devices": [{ "PathOnHost": FUSE, "PathInContainer": FUSE, "CgroupPermissions": "rwm" }],
                "NetworkMode": self.network,
                "Mounts": [{ "Type": "volume", "Source": spec.volume, "Target": target }],
                "Memory": self.limits.memory,
                "MemorySwap": self.limits.memory,
                "NanoCpus": self.limits.nano_cpus,
                "PidsLimit": self.limits.pids,
            },
        })
    }

    /// Проверяет тело `containers/create` и собирает новое.
    pub(super) fn check_create(&self, body: &[u8]) -> Result<CheckedCreate, Denied> {
        let request: CreateRequest = parse(body)?;
        if !self.images.contains(&request.image) {
            return Err(Denied::Image);
        }
        let labels = check_labels(request.labels)?;
        let env = check_env(request.env)?;
        let agent: Agent = labels.get(AGENT_LABEL).and_then(|id| id.parse().ok()).ok_or(Denied::Label)?;
        let agent_dir = agent_dir(agent);
        let host = request.host_config;
        let host = self.check_host(host, agent_dir)?;
        let volume = host.mounts[0].source.clone();
        let rebuilt = CreateBody {
            image: request.image,
            labels,
            env,
            host_config: host,
        };
        Ok(CheckedCreate {
            body: to_json(&rebuilt),
            volume,
        })
    }

    /// Проверяет тело `volumes/create` и собирает новое: имя и метки (роль сессии и прочие `iapetus.*`), драйвера нет.
    pub(super) fn check_volume(body: &[u8]) -> Result<Vec<u8>, Denied> {
        let request: VolumeRequest = parse(body)?;
        if request.driver.is_some() || request.driver_opts.is_some() {
            return Err(Denied::VolumeDriver);
        }
        if !is_name(&request.name) {
            return Err(Denied::Name);
        }
        let labels = check_labels(request.labels)?;
        let rebuilt = VolumeBody {
            name: request.name,
            labels,
        };
        Ok(to_json(&rebuilt))
    }

    /// `no-new-privileges` обязателен; `apparmor=<профиль шаблона>` — по желанию запроса. Остальное, включая
    /// `seccomp=unconfined` и `apparmor=unconfined`, — отказ.
    fn check_security_opt(&self, requested: Option<&[String]>) -> Result<Vec<String>, Denied> {
        let apparmor = self.apparmor_profile.as_ref().map(|profile| format!("{APPARMOR_PREFIX}{profile}"));
        let (mut has_no_new_privileges, mut has_apparmor) = (false, false);
        for option in requested.unwrap_or_default() {
            if option == NO_NEW_PRIVILEGES && !has_no_new_privileges {
                has_no_new_privileges = true;
            } else if apparmor.as_ref() == Some(option) && !has_apparmor {
                has_apparmor = true;
            } else {
                return Err(Denied::SecurityOpt);
            }
        }
        if !has_no_new_privileges {
            return Err(Denied::SecurityOpt);
        }
        let mut checked = vec![NO_NEW_PRIVILEGES.to_owned()];
        checked.extend(apparmor.filter(|_| has_apparmor));
        Ok(checked)
    }

    fn check_host(&self, host: HostConfigRequest, agent_dir: String) -> Result<HostConfigBody, Denied> {
        if host.privileged.unwrap_or(false) {
            return Err(Denied::Privileged);
        }
        for (field, mode) in [
            ("PidMode", &host.pid_mode),
            ("IpcMode", &host.ipc_mode),
            ("UTSMode", &host.uts_mode),
            ("UsernsMode", &host.userns_mode),
            ("CgroupnsMode", &host.cgroupns_mode),
        ] {
            if mode.as_deref().is_some_and(|mode| !mode.is_empty()) {
                return Err(Denied::HostMode(field));
            }
        }
        if host.network_mode.as_deref() != Some(self.network.as_str()) {
            return Err(Denied::Network);
        }
        if host.binds.is_some_and(|binds| !binds.is_empty()) {
            return Err(Denied::Binds);
        }
        if !is_exactly_strs(host.cap_add.as_deref(), &CAP_ADD) || !is_exactly_strs(host.cap_drop.as_deref(), &CAP_DROP) {
            return Err(Denied::Capabilities);
        }
        let tmpfs = check_tmpfs(host.tmpfs, self.limits.work)?;
        let security_opt = self.check_security_opt(host.security_opt.as_deref())?;
        let fuse = Device {
            path_on_host: FUSE.to_owned(),
            path_in_container: FUSE.to_owned(),
            cgroup_permissions: "rwm".to_owned(),
        };
        if !is_exactly(host.devices.as_deref(), &[fuse]) {
            return Err(Denied::Devices);
        }
        let mount = single_volume_mount(host.mounts.as_deref(), agent_dir)?;
        let limits = &self.limits;
        let memory = within("Memory", host.memory, limits.memory)?;
        let swap = within("MemorySwap", host.memory_swap, limits.memory)?;
        if swap != memory {
            return Err(Denied::Limit("MemorySwap"));
        }
        Ok(HostConfigBody {
            cap_add: CAP_ADD,
            cap_drop: CAP_DROP,
            security_opt,
            tmpfs,
            devices: [HostDevice {
                path_on_host: FUSE,
                path_in_container: FUSE,
                cgroup_permissions: "rwm",
            }],
            network_mode: self.network.clone(),
            mounts: [mount],
            memory,
            memory_swap: swap,
            nano_cpus: within("NanoCpus", host.nano_cpus, limits.nano_cpus)?,
            pids_limit: within("PidsLimit", host.pids_limit, limits.pids)?,
        })
    }
}

/// Результат проверки `containers/create`.
#[derive(Debug)]
pub(super) struct CheckedCreate {
    /// Собранное тело, которое уходит в Docker.
    pub(super) body: Vec<u8>,
    /// Том состояния агента: прокси перед созданием проверяет, что он с меткой сессии.
    pub(super) volume: String,
}

fn parse<'a, T: Deserialize<'a>>(body: &'a [u8]) -> Result<T, Denied> {
    serde_json::from_slice(body).map_err(|e| Denied::Malformed(e.to_string()))
}

#[expect(clippy::expect_used, reason = "структуры из строк, чисел и массивов сериализуются без ошибок")]
fn to_json(body: &impl Serialize) -> Vec<u8> {
    serde_json::to_vec(body).expect("сериализация структуры шаблона")
}

fn is_exactly_strs(actual: Option<&[String]>, expected: &[&str]) -> bool {
    actual.is_some_and(|actual| actual.iter().map(String::as_str).eq(expected.iter().copied()))
}

fn is_exactly<T: PartialEq>(actual: Option<&[T]>, expected: &[T]) -> bool {
    actual == Some(expected)
}

fn is_digest_reference(image: &str) -> bool {
    let Some((repository, digest)) = image.split_once('@') else {
        return false;
    };
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return false;
    };
    !repository.is_empty()
        && repository.starts_with(|c: char| c.is_ascii_alphanumeric())
        && repository
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-' | '/' | ':'))
        && hex.len() == 64
        && hex.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// Метка роли обязательна и точна; остальные метки — только `iapetus.*`.
fn check_labels(labels: Option<UniqueMap>) -> Result<BTreeMap<String, String>, Denied> {
    let labels = labels.ok_or(Denied::Label)?.0;
    if labels.len() > MAX_LABELS {
        return Err(Denied::Label);
    }
    let has_role = labels.iter().any(|(k, v)| k == ROLE_LABEL && v == ROLE_VALUE);
    let all_ours = labels
        .iter()
        .all(|(k, v)| k.starts_with(LABEL_PREFIX) && v.len() <= MAX_VALUE_LEN && !v.contains('\0'));
    if !has_role || !all_ours {
        return Err(Denied::Label);
    }
    Ok(labels.into_iter().collect())
}

/// Переменные окружения — `IAPETUS_ИМЯ=значение`, без повторов.
fn check_env(env: Option<Vec<String>>) -> Result<Vec<String>, Denied> {
    let env = env.unwrap_or_default();
    if env.len() > MAX_ENV {
        return Err(Denied::Env("too many variables".to_owned()));
    }
    let mut names = Vec::new();
    for entry in &env {
        let (name, value) = entry.split_once('=').ok_or_else(|| Denied::Env("no `=`".to_owned()))?;
        let name_ok = name
            .strip_prefix(ENV_PREFIX)
            .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'));
        if !name_ok {
            return Err(Denied::Env(format!("name must be {ENV_PREFIX}UPPER_CASE")));
        }
        if value.len() > MAX_VALUE_LEN || value.contains('\0') {
            return Err(Denied::Env("value is too long or has NUL".to_owned()));
        }
        if names.contains(&name) {
            return Err(Denied::Env("duplicate name".to_owned()));
        }
        names.push(name);
    }
    Ok(env)
}

/// Ровно `/work` и `/run/secrets` с параметрами шаблона: без них кэш и секреты лежали бы на диске. Размер `/work` —
/// число байт без знака и нулей впереди, от 1 до `max_work`.
fn check_tmpfs(tmpfs: Option<UniqueMap>, max_work: i64) -> Result<BTreeMap<&'static str, String>, Denied> {
    let mut requested = tmpfs.ok_or(Denied::Tmpfs)?.0;
    requested.sort();
    let [(secrets_path, secrets_options), (work_path, work_options)] = requested.as_slice() else {
        return Err(Denied::Tmpfs);
    };
    if work_path != WORK_DIR || secrets_path != SECRETS_DIR.0 || secrets_options != SECRETS_DIR.1 {
        return Err(Denied::Tmpfs);
    }
    let size = work_options.strip_prefix(WORK_OPTIONS).ok_or(Denied::Tmpfs)?;
    let bytes: i64 = size.parse().map_err(|_| Denied::Tmpfs)?;
    if bytes.to_string() != size {
        return Err(Denied::Tmpfs);
    }
    let bytes = within("Tmpfs /work size", Some(bytes), max_work)?;
    Ok(BTreeMap::from([
        (SECRETS_DIR.0, SECRETS_DIR.1.to_owned()),
        (WORK_DIR, format!("{WORK_OPTIONS}{bytes}")),
    ]))
}

/// Каталог состояния агента в контейнере: том монтируется сюда.
fn agent_dir(agent: Agent) -> String {
    format!("{AGENT_ROOT}/{}", agent.state_dir())
}

/// Ровно один том `volume` в каталог состояния агента `dir`, без параметров драйвера и без других видов монтирования.
fn single_volume_mount(mounts: Option<&[MountRequest]>, dir: String) -> Result<VolumeMount, Denied> {
    let Some([mount]) = mounts else {
        return Err(Denied::Mounts);
    };
    if mount.kind != "volume" || mount.target != dir || !is_name(&mount.source) {
        return Err(Denied::Mounts);
    }
    Ok(VolumeMount {
        kind: "volume",
        source: mount.source.clone(),
        target: dir,
    })
}

/// Лимит задан, положителен и не больше верхней границы.
fn within(field: &'static str, value: Option<i64>, max: i64) -> Result<i64, Denied> {
    match value {
        Some(value) if (1..=max).contains(&value) => Ok(value),
        _ => Err(Denied::Limit(field)),
    }
}

/// Словарь строк, который отвергает повторяющиеся ключи: `HashMap` молча оставил бы последний,
/// а Docker (Go) мог бы выбрать другое значение, чем проверили мы.
#[derive(Debug)]
struct UniqueMap(Vec<(String, String)>);

impl<'de> Deserialize<'de> for UniqueMap {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueMapVisitor;

        impl<'de> Visitor<'de> for UniqueMapVisitor {
            type Value = UniqueMap;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a map of strings without repeated keys")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<UniqueMap, A::Error> {
                let mut entries: Vec<(String, String)> = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, String>()? {
                    if entries.iter().any(|(known, _)| *known == key) {
                        return Err(de::Error::custom(format_args!("duplicate key `{key}`")));
                    }
                    entries.push((key, value));
                }
                Ok(UniqueMap(entries))
            }
        }

        deserializer.deserialize_map(UniqueMapVisitor)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    #[serde(rename = "Image")]
    image: String,
    #[serde(rename = "Labels")]
    labels: Option<UniqueMap>,
    #[serde(rename = "Env")]
    env: Option<Vec<String>>,
    #[serde(rename = "HostConfig")]
    host_config: HostConfigRequest,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostConfigRequest {
    #[serde(rename = "Privileged")]
    privileged: Option<bool>,
    #[serde(rename = "CapAdd")]
    cap_add: Option<Vec<String>>,
    #[serde(rename = "CapDrop")]
    cap_drop: Option<Vec<String>>,
    #[serde(rename = "SecurityOpt")]
    security_opt: Option<Vec<String>>,
    #[serde(rename = "Tmpfs")]
    tmpfs: Option<UniqueMap>,
    #[serde(rename = "Devices")]
    devices: Option<Vec<Device>>,
    #[serde(rename = "Binds")]
    binds: Option<Vec<String>>,
    #[serde(rename = "Mounts")]
    mounts: Option<Vec<MountRequest>>,
    #[serde(rename = "NetworkMode")]
    network_mode: Option<String>,
    #[serde(rename = "PidMode")]
    pid_mode: Option<String>,
    #[serde(rename = "IpcMode")]
    ipc_mode: Option<String>,
    #[serde(rename = "UTSMode")]
    uts_mode: Option<String>,
    #[serde(rename = "UsernsMode")]
    userns_mode: Option<String>,
    #[serde(rename = "CgroupnsMode")]
    cgroupns_mode: Option<String>,
    #[serde(rename = "Memory")]
    memory: Option<i64>,
    #[serde(rename = "MemorySwap")]
    memory_swap: Option<i64>,
    #[serde(rename = "NanoCpus")]
    nano_cpus: Option<i64>,
    #[serde(rename = "PidsLimit")]
    pids_limit: Option<i64>,
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Device {
    #[serde(rename = "PathOnHost")]
    path_on_host: String,
    #[serde(rename = "PathInContainer")]
    path_in_container: String,
    #[serde(rename = "CgroupPermissions")]
    cgroup_permissions: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MountRequest {
    #[serde(rename = "Type")]
    kind: String,
    #[serde(rename = "Source")]
    source: String,
    #[serde(rename = "Target")]
    target: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VolumeRequest {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Labels")]
    labels: Option<UniqueMap>,
    #[serde(rename = "Driver")]
    driver: Option<serde_json::Value>,
    #[serde(rename = "DriverOpts")]
    driver_opts: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct CreateBody {
    #[serde(rename = "Image")]
    image: String,
    #[serde(rename = "Labels")]
    labels: BTreeMap<String, String>,
    #[serde(rename = "Env")]
    env: Vec<String>,
    #[serde(rename = "HostConfig")]
    host_config: HostConfigBody,
}

#[derive(Serialize)]
struct HostConfigBody {
    #[serde(rename = "CapAdd")]
    cap_add: [&'static str; CAP_ADD.len()],
    #[serde(rename = "CapDrop")]
    cap_drop: [&'static str; CAP_DROP.len()],
    #[serde(rename = "SecurityOpt")]
    security_opt: Vec<String>,
    #[serde(rename = "Tmpfs")]
    tmpfs: BTreeMap<&'static str, String>,
    #[serde(rename = "Devices")]
    devices: [HostDevice; 1],
    #[serde(rename = "NetworkMode")]
    network_mode: String,
    #[serde(rename = "Mounts")]
    mounts: [VolumeMount; 1],
    #[serde(rename = "Memory")]
    memory: i64,
    #[serde(rename = "MemorySwap")]
    memory_swap: i64,
    #[serde(rename = "NanoCpus")]
    nano_cpus: i64,
    #[serde(rename = "PidsLimit")]
    pids_limit: i64,
}

#[derive(Serialize)]
struct HostDevice {
    #[serde(rename = "PathOnHost")]
    path_on_host: &'static str,
    #[serde(rename = "PathInContainer")]
    path_in_container: &'static str,
    #[serde(rename = "CgroupPermissions")]
    cgroup_permissions: &'static str,
}

#[derive(Serialize)]
struct VolumeMount {
    #[serde(rename = "Type")]
    kind: &'static str,
    #[serde(rename = "Source")]
    source: String,
    #[serde(rename = "Target")]
    target: String,
}

#[derive(Serialize)]
struct VolumeBody {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Labels")]
    labels: BTreeMap<String, String>,
}

#[cfg(test)]
#[path = "../../../tests/server/docker/template.rs"]
mod tests;
