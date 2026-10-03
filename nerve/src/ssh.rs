//! Общие для orbit и waldo соглашения SSH: имена входа, окна, keepalive и правила сертификатов.

use std::time::Duration;

use crate::fingerprint::Fingerprint;

mod trust;

pub use trust::Trust;

/// Имя входа узлов на SSH orbit server.
pub const NODE_LOGIN: &str = "iapetus";

/// Имя входа runner на сервер данных сессии: отдельный пользователь, у которого есть только канал данных.
pub const DATA_LOGIN: &str = "iapetus-data";

/// Имя входа узла без действующего сертификата на внешнем слушателе server: `password` — `orbit_key`, разрешены
/// только запросы входа ([`EnrollRequest`](crate::protocol::EnrollRequest)).
pub const ENROLL_LOGIN: &str = "iapetus-enroll";

/// Имя входа runner и worker с сертификатом на канал управления server.
pub const CONTROL_LOGIN: &str = "iapetus-control";

/// Имя входа runner и worker с сертификатом на jump server: `direct-tcpip` в session.
pub const JUMP_LOGIN: &str = "iapetus-jump";

/// `key_id` сертификата waldo worker. В fingerprint нет двоеточия, поэтому сертификат worker не спутать с
/// сертификатом runner, у которого `key_id` — fingerprint.
pub const WORKER_KEY_ID: &str = "worker:";

/// Начало имени входа session на внутренний слушатель server; дальше — fingerprint сессии.
pub const SESSION_LOGIN_PREFIX: &str = "session-";

/// Подсистема SSH, в которой идёт канал управления.
pub const CONTROL_SUBSYSTEM: &str = "iapetus-control";

/// Имя входа session на внутренний слушатель server: `session-<fp>`.
pub fn session_login(session: &Fingerprint) -> String {
    format!("{SESSION_LOGIN_PREFIX}{session}")
}

/// Хост в запросе `direct-tcpip` через jump и принципал хост-сертификата сессии. Куда на самом деле вести runner,
/// решает server по реестру, но имя хоста runner проверяет по сертификату сессии.
pub const DATA_HOST: &str = "iapetus-session";

/// Подсистема SSH, в которой идёт канал данных.
pub const DATA_SUBSYSTEM: &str = "iapetus-data";

/// Окно канала SSH: меньше режет пропускную способность в WAN, больше — задержку мелких запросов при передаче
/// большого файла.
pub const WINDOW: u32 = 8 << 20;

/// Наибольший пакет SSH.
pub const PACKET: u32 = 32 * 1024;

/// Как часто SSH спрашивает собеседника, жив ли он, и сколько ответов подряд можно пропустить. Короче — ложные
/// обрывы на коротких провалах сети.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
pub const KEEPALIVE_MAX: usize = 3;

/// Срок жизни сертификата узла.
pub const CERTIFICATE_LIFETIME: Duration = Duration::from_mins(15);

/// Допуск на расхождение часов: `valid_after` ставится на минуту раньше момента выдачи.
pub const CLOCK_SKEW: Duration = Duration::from_secs(60);

/// Наибольшее окно `valid_after..valid_before` сертификата узла: срок жизни и допуск на расхождение
/// часов. Столько разрешают jump и сессия.
pub const MAX_CERTIFICATE_WINDOW: Duration = Duration::from_secs(CERTIFICATE_LIFETIME.as_secs() + CLOCK_SKEW.as_secs());
