use super::*;

impl Link {
    /// Канал, которым управляет тест: без SSH-сервера, с готовым мультиплексором или без него. Сессия берёт
    /// канал у [`Data`].
    pub(crate) fn fixed(mux: Option<Mux>) -> (Self, watch::Sender<Option<Mux>>) {
        let (sender, receiver) = watch::channel(mux);
        (Self(receiver), sender)
    }
}
