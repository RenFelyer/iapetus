//! TUI worker: список сессий с лимитами, новое подключение (агент, runner), завершение сессии, вход в агента
//! (в нём пользователь входит в свой аккаунт сам), смена runner у сессии и разделы управления: узлы (отзыв runner,
//! новый `orbit_key`) и окружение. Все действия — запросы к server по каналу управления; ошибка запроса остаётся в
//! строке состояния.

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use nerve::fingerprint::Fingerprint;
use nerve::protocol::{Agent, Cause, EnvEntry, NodeInfo, RunnerInfo, RunnerPresence, SessionEvent, SessionInfo, UsageWindow};
use nerve::session_state::SessionState;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, List, ListState, Paragraph, Row, Table, TableState};
use ratatui::{DefaultTerminal, Frame};

use super::control::Control;
use super::terminal::{self, Leave};

/// Как часто обновляется список сессий.
const REFRESH: Duration = Duration::from_secs(5);

/// Как долго ждём клавишу, прежде чем перерисовать и проверить обновление.
const TICK: Duration = Duration::from_millis(200);

#[derive(Debug, Clone)]
enum Choice {
    Agent(Agent),
    Runner(Fingerprint),
    /// Ответ на вопрос «продолжить разговор?».
    Resume(bool),
    Confirm,
    /// Пункт виден, но не выбирается; внутри — причина.
    Disabled(&'static str),
    Env(String),
    Node(Fingerprint),
}

/// Что вводится в строке и что делать с введённым.
#[derive(Debug)]
enum Typing {
    CloneDir { runner: Fingerprint },
    RunnerName { runner: Fingerprint },
    EnvEntry,
}

#[derive(Debug)]
struct Input {
    title: String,
    text: String,
    typing: Typing,
}

/// Раздел управления.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Nodes,
    Env,
}

/// Что выбирается в списке выбора и что делать с выбором.
#[derive(Debug)]
enum Purpose {
    NewAgent,
    NewRunner {
        agent: Agent,
    },
    AssignRunner {
        session: Fingerprint,
    },
    /// Новый runner выбран; остался вопрос, продолжать ли разговор агента.
    AssignResume {
        session: Fingerprint,
        runner: Fingerprint,
    },
    CloneRunner,
    RenameRunner,
    End {
        session: Fingerprint,
    },
    RevokeNode,
    NewOrbitKey,
    DeleteEnv,
    Restart {
        session: Fingerprint,
    },
}

#[derive(Debug)]
struct Pick {
    title: String,
    labels: Vec<String>,
    choices: Vec<Choice>,
    state: ListState,
    purpose: Purpose,
}

impl Pick {
    fn new(title: &str, purpose: Purpose, items: Vec<(String, Choice)>) -> Self {
        let (labels, choices) = items.into_iter().unzip();
        Self {
            title: title.to_owned(),
            labels,
            choices,
            state: ListState::default().with_selected(Some(0)),
            purpose,
        }
    }

    fn selected(&self) -> Option<&Choice> {
        self.choices.get(self.state.selected()?)
    }
}

#[derive(Debug)]
enum Screen {
    List,
    Manage,
    Pick(Pick),
    Input(Input),
    /// Секрет, который показывается один раз; любая клавиша закрывает.
    Secret(String),
}

/// Что сделать после обработки клавиши.
#[derive(Debug)]
enum Outcome {
    Stay,
    Quit,
    Enter(Fingerprint),
    Start { agent: Agent, runner: Fingerprint },
}

struct App {
    sessions: Vec<SessionInfo>,
    runners: Vec<RunnerInfo>,
    nodes: Vec<NodeInfo>,
    env: Vec<EnvEntry>,
    section: Option<Section>,
    manage: ListState,
    table: TableState,
    screen: Screen,
    status: String,
}

/// Работает с терминалом, пока пользователь не выйдет. `name` — имя входа в сессии.
pub(super) async fn run(control: &Control, name: &str) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, control, name).await;
    ratatui::restore();
    result
}

async fn event_loop(terminal: &mut DefaultTerminal, control: &Control, name: &str) -> Result<()> {
    let mut app = App {
        sessions: Vec::new(),
        runners: Vec::new(),
        nodes: Vec::new(),
        env: Vec::new(),
        section: None,
        manage: ListState::default(),
        table: TableState::default(),
        screen: Screen::List,
        status: String::new(),
    };
    app.refresh(control).await;
    let mut refreshed = Instant::now();
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        if refreshed.elapsed() >= REFRESH {
            app.refresh_sessions(control).await;
            refreshed = Instant::now();
        }
        let Some(Event::Key(key)) = next_event().await? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match app.handle_key(control, key).await {
            Outcome::Stay => {}
            Outcome::Quit => return Ok(()),
            Outcome::Enter(session) => {
                enter(terminal, control, name, &session, &mut app).await?;
                refreshed = Instant::now();
            }
            Outcome::Start { agent, runner } => {
                if let Some(session) = start(terminal, control, &mut app, agent, &runner).await? {
                    enter(terminal, control, name, &session, &mut app).await?;
                }
                refreshed = Instant::now();
            }
        }
    }
}

/// Следующее событие терминала; `None` — за [`TICK`] ничего не пришло. Чтение идёт в отдельном потоке и
/// не переживает вызов, поэтому после выхода в агента оно не отнимает клавиши.
async fn next_event() -> Result<Option<Event>> {
    tokio::task::spawn_blocking(|| -> std::io::Result<Option<Event>> {
        if event::poll(TICK)? { event::read().map(Some) } else { Ok(None) }
    })
    .await?
    .map_err(Into::into)
}

/// Уступает терминал агенту и возвращает TUI после выхода.
async fn enter(terminal: &mut DefaultTerminal, control: &Control, name: &str, session: &Fingerprint, app: &mut App) -> Result<()> {
    let Some((server, key, certificate)) = control.login() else {
        app.say("сертификата worker ещё нет: нет связи с server");
        return Ok(());
    };
    ratatui::restore();
    let result = terminal::attach(server, key, &certificate, name, session).await;
    *terminal = ratatui::init();
    app.refresh(control).await;
    app.status = match result {
        Ok(Leave::Detached) => String::new(),
        Ok(Leave::AgentClosed) => app.agent_closed_message(session),
        Err(error) => format!("вход не удался: {error:#}"),
    };
    Ok(())
}

/// Поднимает сессию и ждёт `Ready`; Esc отменяет ожидание, сессия при этом остаётся у server.
async fn start(
    terminal: &mut DefaultTerminal,
    control: &Control,
    app: &mut App,
    agent: Agent,
    runner: &Fingerprint,
) -> Result<Option<Fingerprint>> {
    let mut replies = match control.start_session(agent, runner).await {
        Ok(replies) => replies,
        Err(error) => {
            app.status = format!("{error:#}");
            return Ok(None);
        }
    };
    app.say("Агент просыпается, Esc — не ждать");
    terminal.draw(|frame| app.draw(frame))?;
    loop {
        tokio::select! {
            reply = replies.next() => {
                if let Some(session) = app.report_start(reply) {
                    return Ok(Some(session));
                }
                app.refresh(control).await;
                return Ok(None);
            }
            event = next_event() => {
                if let Some(Event::Key(key)) = event?
                    && key.kind == KeyEventKind::Press
                    && key.code == KeyCode::Esc
                {
                    app.say("ожидание отменено");
                    app.refresh(control).await;
                    return Ok(None);
                }
            }
        }
    }
}

impl App {
    /// Ответ на `Sessions.Start`: готовая сессия или текст отказа в строке состояния.
    fn report_start(&mut self, reply: Result<Option<SessionEvent>>) -> Option<Fingerprint> {
        match reply {
            Ok(Some(SessionEvent::Ready(session))) => {
                self.status = String::new();
                return Some(session);
            }
            Ok(Some(SessionEvent::Refused(reason))) => self.status = format!("сессия не поднята: {reason}"),
            Ok(Some(SessionEvent::Ended) | None) => self.say("сессия завершилась, вход не выполнен"),
            Err(error) => self.status = format!("сессия не поднята: {error:#}"),
        }
        None
    }

    async fn refresh(&mut self, control: &Control) {
        self.refresh_sessions(control).await;
        match control.runners().await {
            Ok(runners) => self.runners = runners,
            Err(error) => self.status = format!("{error:#}"),
        }
    }

    async fn refresh_sessions(&mut self, control: &Control) {
        match control.sessions().await {
            Ok(sessions) => {
                self.sessions = sessions;
                let last = self.sessions.len().checked_sub(1);
                let selected = self.table.selected().map_or(0, |at| at.min(last.unwrap_or(0)));
                self.table.select(last.map(|_| selected));
            }
            Err(error) => self.status = format!("{error:#}"),
        }
    }

    fn say(&mut self, text: &str) {
        text.clone_into(&mut self.status);
    }

    fn selected_session(&self) -> Option<&SessionInfo> {
        self.sessions.get(self.table.selected()?)
    }

    /// Экран, на который возвращаются из выбора и ввода: раздел управления, если он открыт.
    fn home(&self) -> Screen {
        if self.section.is_some() { Screen::Manage } else { Screen::List }
    }

    async fn handle_key(&mut self, control: &Control, key: KeyEvent) -> Outcome {
        let home = self.home();
        match &mut self.screen {
            Screen::List => self.handle_list_key(control, key).await,
            Screen::Manage => {
                self.handle_manage_key(control, key).await;
                Outcome::Stay
            }
            Screen::Secret(_) => {
                self.screen = home;
                Outcome::Stay
            }
            Screen::Pick(pick) => {
                match key.code {
                    KeyCode::Esc => self.screen = home,
                    KeyCode::Up | KeyCode::Char('k') => pick.state.select_previous(),
                    KeyCode::Down | KeyCode::Char('j') => pick.state.select_next(),
                    KeyCode::Enter => {
                        if let Some(Choice::Disabled(reason)) = pick.selected() {
                            self.status = (*reason).to_owned();
                        } else {
                            return self.choose(control).await;
                        }
                    }
                    _ => {}
                }
                Outcome::Stay
            }
            Screen::Input(input) => {
                match key.code {
                    KeyCode::Esc => self.screen = home,
                    KeyCode::Backspace => {
                        input.text.pop();
                    }
                    KeyCode::Char(c) => input.text.push(c),
                    KeyCode::Enter => self.submit(control).await,
                    _ => {}
                }
                Outcome::Stay
            }
        }
    }

    async fn handle_list_key(&mut self, control: &Control, key: KeyEvent) -> Outcome {
        self.status = String::new();
        match key.code {
            KeyCode::Char('q') => return Outcome::Quit,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Outcome::Quit,
            KeyCode::Up | KeyCode::Char('k') => self.table.select_previous(),
            KeyCode::Down | KeyCode::Char('j') => self.table.select_next(),
            KeyCode::Char('r') => self.refresh(control).await,
            KeyCode::Enter => return self.enter_selected(control).await,
            KeyCode::Char('n') => self.pick_agent(),
            KeyCode::Char('R') => {
                if let Some(session) = self.selected_session() {
                    // Остановленный агент ждёт назначения: его прежний runner занят этой же сессией, но выбирается.
                    let own = session.runner.clone().filter(|_| session.cause == Some(Cause::RunnerRequired));
                    let session = session.fingerprint.clone();
                    self.pick_runner_with("Сменить runner", Purpose::AssignRunner { session }, move |runner| {
                        let own = own.as_ref() == Some(&runner.fingerprint) && runner.presence == RunnerPresence::Busy;
                        runner.presence == RunnerPresence::Free || own
                    });
                }
            }
            KeyCode::Char('N') => self.open_section(control, Section::Nodes).await,
            KeyCode::Char('E') => self.open_section(control, Section::Env).await,
            KeyCode::Char('c') => self.pick_runner("Создать клон runner", Purpose::CloneRunner, online),
            KeyCode::Char('m') => self.pick_runner("Переименовать runner", Purpose::RenameRunner, any_runner),
            KeyCode::Char('f') => {
                if let Some(session) = self.selected_session().map(|session| session.fingerprint.clone()) {
                    let items = vec![("Да, перезапустить сессию".to_owned(), Choice::Confirm)];
                    self.screen = Screen::Pick(Pick::new("Перезапустить сессию из failure", Purpose::Restart { session }, items));
                }
            }
            KeyCode::Char('a') => self.restart_agent(control).await,
            KeyCode::Char('e') => {
                if let Some(session) = self.selected_session().map(|session| session.fingerprint.clone()) {
                    let items = vec![("Да, завершить сессию".to_owned(), Choice::Confirm)];
                    self.screen = Screen::Pick(Pick::new("Завершить сессию", Purpose::End { session }, items));
                }
            }
            _ => {}
        }
        Outcome::Stay
    }

    /// Требует перезапустить агента выбранной сессии. Если перезапуск уже ждёт конца хода, повторное нажатие
    /// перезапускает агента сразу: решает session, список нужен только для текста в строке состояния.
    async fn restart_agent(&mut self, control: &Control) {
        let Some(session) = self.selected_session() else {
            return;
        };
        let (fingerprint, immediately) = (session.fingerprint.clone(), session.restart_pending);
        self.status = match control.restart_agent(&fingerprint).await {
            Ok(()) if immediately => "агент перезапускается сразу".to_owned(),
            Ok(()) => "агент перезапустится в конце хода, a ещё раз — сразу".to_owned(),
            Err(error) => format!("{error:#}"),
        };
        self.refresh_sessions(control).await;
    }

    /// Вход в выбранную сессию. Если агент в ней не запущен, вход не открывается, а в строке состояния — состояние
    /// сессии и что делать. Список мог устареть, поэтому перед отказом он обновляется.
    async fn enter_selected(&mut self, control: &Control) -> Outcome {
        let Some(selected) = self.selected_session() else {
            return Outcome::Stay;
        };
        let fingerprint = selected.fingerprint.clone();
        if !selected.agent_running {
            self.refresh_sessions(control).await;
        }
        match self.sessions.iter().find(|session| session.fingerprint == fingerprint) {
            Some(session) if !session.agent_running => self.status = no_agent_message(session),
            Some(_) => return Outcome::Enter(fingerprint),
            None => self.say("сессии больше нет"),
        }
        Outcome::Stay
    }

    /// Что сказать, когда session закрыла канал агента: он перезапущен, остановлен или сессии больше нет.
    fn agent_closed_message(&self, fingerprint: &Fingerprint) -> String {
        match self.sessions.iter().find(|session| &session.fingerprint == fingerprint) {
            None => "сессия завершена".to_owned(),
            Some(session) if session.agent_running => "агент перезапущен: Enter — войти снова".to_owned(),
            Some(session) => no_agent_message(session),
        }
    }

    /// Список runner со значком состояния; `allowed` решает по состоянию, какие из них выбираются.
    fn pick_runner(&mut self, title: &str, purpose: Purpose, allowed: fn(RunnerPresence) -> bool) {
        self.pick_runner_with(title, purpose, |runner| allowed(runner.presence));
    }

    /// То же, когда выбор зависит от самого runner.
    fn pick_runner_with(&mut self, title: &str, purpose: Purpose, allowed: impl Fn(&RunnerInfo) -> bool) {
        let items: Vec<_> = self
            .runners
            .iter()
            .map(|runner| {
                let label = format!("{} {} {}", presence_icon(runner.presence), runner.name, runner.fingerprint);
                let choice = if allowed(runner) {
                    Choice::Runner(runner.fingerprint.clone())
                } else {
                    Choice::Disabled(match runner.presence {
                        RunnerPresence::Busy => "runner занят",
                        RunnerPresence::Offline => "runner не в сети",
                        RunnerPresence::Free => "runner не подходит",
                    })
                };
                (label, choice)
            })
            .collect();
        if items.is_empty() {
            self.say("нет runner");
            return;
        }
        self.screen = Screen::Pick(Pick::new(title, purpose, items));
    }

    fn pick_agent(&mut self) {
        let items = Agent::ALL
            .iter()
            .map(|agent| (agent.name().to_owned(), Choice::Agent(*agent)))
            .collect();
        self.screen = Screen::Pick(Pick::new("Новое подключение: агент", Purpose::NewAgent, items));
    }

    async fn open_section(&mut self, control: &Control, section: Section) {
        self.section = Some(section);
        self.manage = ListState::default().with_selected(Some(0));
        self.screen = Screen::Manage;
        self.refresh_manage(control).await;
    }

    /// Обновляет данные открытого раздела.
    async fn refresh_manage(&mut self, control: &Control) {
        match self.section {
            Some(Section::Nodes) => match control.nodes().await {
                Ok(nodes) => self.nodes = nodes,
                Err(error) => self.status = format!("{error:#}"),
            },
            Some(Section::Env) => match control.env().await {
                Ok(env) => self.env = env,
                Err(error) => self.status = format!("{error:#}"),
            },
            None => {}
        }
    }

    /// Число строк открытого раздела.
    fn manage_len(&self) -> usize {
        match self.section {
            Some(Section::Nodes) => self.nodes.len(),
            Some(Section::Env) => self.env.len(),
            None => 0,
        }
    }

    /// Номер выбранной строки раздела; `None` — строк нет.
    fn manage_selected(&self) -> Option<usize> {
        let last = self.manage_len().checked_sub(1)?;
        Some(self.manage.selected().map_or(0, |at| at.min(last)))
    }

    async fn handle_manage_key(&mut self, control: &Control, key: KeyEvent) {
        self.status = String::new();
        let Some(section) = self.section else {
            self.screen = Screen::List;
            return;
        };
        let selected = self.manage_selected();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.section = None;
                self.screen = Screen::List;
            }
            KeyCode::Up | KeyCode::Char('k') => self.manage.select(selected.map(|at| at.saturating_sub(1))),
            KeyCode::Down | KeyCode::Char('j') => self.manage.select(selected.map(|at| at + 1)),
            KeyCode::Char('r') => self.refresh_manage(control).await,
            KeyCode::Char('a') if section == Section::Env => {
                self.screen = Screen::Input(Input {
                    title: "Переменная: all.ИМЯ=значение или <fingerprint runner>.ИМЯ=значение".to_owned(),
                    text: String::new(),
                    typing: Typing::EnvEntry,
                });
            }
            KeyCode::Char('d') => self.pick_delete(section, selected),
            KeyCode::Char('v') if section == Section::Nodes => {
                let Some(node) = selected.and_then(|at| self.nodes.get(at)) else {
                    return;
                };
                let items = vec![(
                    format!("Да, отозвать {} — сертификаты runner перестанут действовать", node.runner.name),
                    Choice::Node(node.runner.fingerprint.clone()),
                )];
                self.screen = Screen::Pick(Pick::new("Отозвать runner", Purpose::RevokeNode, items));
            }
            KeyCode::Char('o') if section == Section::Nodes => {
                let items = vec![(
                    "Да, выдать новый orbit_key: все worker отключатся, прежний ключ перестанет действовать".to_owned(),
                    Choice::Confirm,
                )];
                self.screen = Screen::Pick(Pick::new("Новый orbit_key", Purpose::NewOrbitKey, items));
            }
            _ => {}
        }
    }

    /// Подтверждение удаления выбранной переменной окружения.
    fn pick_delete(&mut self, section: Section, selected: Option<usize>) {
        let Some(at) = selected else {
            return;
        };
        let (title, purpose, label, choice) = match section {
            Section::Env => {
                let Some(entry) = self.env.get(at) else { return };
                (
                    "Удалить переменную",
                    Purpose::DeleteEnv,
                    format!("Да, удалить {}", entry.key),
                    Choice::Env(entry.key.clone()),
                )
            }
            Section::Nodes => return,
        };
        self.screen = Screen::Pick(Pick::new(title, purpose, vec![(label, choice)]));
    }

    fn rename_input(&mut self, runner: Fingerprint) {
        let current = self
            .runners
            .iter()
            .find(|info| info.fingerprint == runner)
            .map(|info| info.name.clone());
        self.screen = Screen::Input(Input {
            title: "Новое имя runner".to_owned(),
            text: current.unwrap_or_default(),
            typing: Typing::RunnerName { runner },
        });
    }

    /// Выбор в списке: следующий шаг мастера или само действие.
    async fn choose(&mut self, control: &Control) -> Outcome {
        let home = self.home();
        let Screen::Pick(pick) = std::mem::replace(&mut self.screen, home) else {
            return Outcome::Stay;
        };
        let Some(choice) = pick.state.selected().and_then(|at| pick.choices.get(at)).cloned() else {
            return Outcome::Stay;
        };
        match (pick.purpose, choice) {
            (Purpose::NewAgent, Choice::Agent(agent)) => {
                self.pick_runner("Новое подключение: runner", Purpose::NewRunner { agent }, free_only);
            }
            (Purpose::NewRunner { agent }, Choice::Runner(runner)) => return Outcome::Start { agent, runner },
            (Purpose::AssignRunner { session }, Choice::Runner(runner)) => {
                let items = vec![
                    ("Да, продолжить разговор".to_owned(), Choice::Resume(true)),
                    ("Нет, начать заново".to_owned(), Choice::Resume(false)),
                ];
                self.screen = Screen::Pick(Pick::new(
                    "Продолжить разговор агента?",
                    Purpose::AssignResume { session, runner },
                    items,
                ));
            }
            (Purpose::AssignResume { session, runner }, Choice::Resume(resume)) => {
                self.status = match control.assign_runner(&session, &runner, resume).await {
                    Ok(()) => "runner сменён".to_owned(),
                    Err(error) => format!("{error:#}"),
                };
                self.refresh(control).await;
            }
            (Purpose::CloneRunner, Choice::Runner(runner)) => {
                self.screen = Screen::Input(Input {
                    title: "Каталог клона на машине runner (абсолютный путь)".to_owned(),
                    text: String::new(),
                    typing: Typing::CloneDir { runner },
                });
            }
            (Purpose::RenameRunner, Choice::Runner(runner)) => self.rename_input(runner),
            (Purpose::End { session }, Choice::Confirm) => {
                self.status = match control.end_session(&session).await {
                    Ok(()) => "сессия завершена".to_owned(),
                    Err(error) => format!("{error:#}"),
                };
                self.refresh_sessions(control).await;
            }
            (Purpose::RevokeNode, Choice::Node(runner)) => {
                self.status = match control.revoke_node(&runner).await {
                    Ok(()) => "runner отозван".to_owned(),
                    Err(error) => format!("{error:#}"),
                };
                self.refresh_manage(control).await;
            }
            (Purpose::NewOrbitKey, Choice::Confirm) => match control.new_orbit_key().await {
                Ok(secret) => {
                    self.status = String::new();
                    self.screen = Screen::Secret(secret);
                }
                Err(error) => self.status = format!("{error:#}"),
            },
            (Purpose::DeleteEnv, Choice::Env(key)) => {
                self.status = match control.delete_env(&key).await {
                    Ok(()) => "переменная удалена".to_owned(),
                    Err(error) => format!("{error:#}"),
                };
                self.refresh_manage(control).await;
            }
            (Purpose::Restart { session }, Choice::Confirm) => {
                self.status = match control.restart_session(&session).await {
                    Ok(()) => "сессия перезапущена".to_owned(),
                    Err(error) => format!("{error:#}"),
                };
                self.refresh_sessions(control).await;
            }
            (purpose, choice) => self.status = anyhow!("выбор {choice:?} не подходит шагу {purpose:?}").to_string(),
        }
        Outcome::Stay
    }

    /// Введённая строка уходит запросом; ошибка остаётся в строке состояния, повтора нет.
    async fn submit(&mut self, control: &Control) {
        let home = self.home();
        let Screen::Input(input) = std::mem::replace(&mut self.screen, home) else {
            return;
        };
        let text = input.text.trim();
        if text.is_empty() {
            self.say("строка пуста");
            return;
        }
        self.status = match input.typing {
            Typing::EnvEntry => {
                let Some((key, value)) = text.split_once('=') else {
                    self.say("нужна строка вида all.ПЕРЕМЕННАЯ=значение");
                    return;
                };
                let set = control.set_env(key.trim(), value).await;
                self.refresh_manage(control).await;
                match set {
                    Ok(()) => "переменная записана".to_owned(),
                    Err(error) => format!("{error:#}"),
                }
            }
            Typing::CloneDir { runner } => match control.clone_runner(&runner, text).await {
                Ok(clone) => format!("клон {} создан", clone.name),
                Err(error) => format!("{error:#}"),
            },
            Typing::RunnerName { runner } => match control.rename_runner(&runner, text).await {
                Ok(()) => "runner переименован".to_owned(),
                Err(error) => format!("{error:#}"),
            },
        };
        match control.runners().await {
            Ok(runners) => self.runners = runners,
            Err(error) => self.status = format!("{error:#}"),
        }
    }

    fn draw(&mut self, frame: &mut Frame<'_>) {
        let [body, footer] = Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).areas(frame.area());
        let rows: Vec<Row<'static>> = self.sessions.iter().map(|session| self.row(session)).collect();
        let widths = [
            Constraint::Length(13),
            Constraint::Length(16),
            Constraint::Min(12),
            Constraint::Length(22),
            Constraint::Min(20),
        ];
        let table = Table::new(rows, widths)
            .header(Row::new(["Агент", "Состояние", "Runner", "Лимиты", "Причина"]).style(Style::new().add_modifier(Modifier::BOLD)))
            .block(Block::bordered().title("Сессии"))
            .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        frame.render_stateful_widget(table, body, &mut self.table);

        if let Some(section) = self.section {
            self.draw_section(frame, body, section);
        }
        let help = match (&self.screen, self.section) {
            (Screen::Pick(_), _) => "Enter выбрать · Esc назад",
            (Screen::Input(_), _) => "Enter подтвердить · Esc назад",
            (Screen::Secret(_), _) => "любая клавиша закрывает",
            (Screen::Manage, Some(Section::Nodes)) => "v отозвать runner · o новый orbit_key · r обновить · Esc назад",
            (Screen::Manage, Some(Section::Env)) => "a задать · d удалить · r обновить · Esc назад",
            (Screen::Manage | Screen::List, _) => {
                "Enter вход (Ctrl-] выход из агента) · n новое · R сменить runner · a перезапустить агента · f перезапустить сессию · e завершить · c клон · m имя runner · N узлы · E окружение · r обновить · q выйти"
            }
        };
        frame.render_widget(Paragraph::new(vec![Line::raw(self.status.clone()), Line::raw(help)]), footer);

        if let Screen::Input(input) = &self.screen {
            let area = body.centered(Constraint::Percentage(60), Constraint::Length(3));
            frame.render_widget(ratatui::widgets::Clear, area);
            frame.render_widget(
                Paragraph::new(format!("{}_", input.text)).block(Block::bordered().title(input.title.as_str())),
                area,
            );
        }
        if let Screen::Secret(secret) = &self.screen {
            let area = body.centered(Constraint::Percentage(80), Constraint::Length(6));
            let lines = vec![
                Line::raw(secret.clone()),
                Line::raw(""),
                Line::raw("Показан один раз: сохраните его. Все worker, включая этот, отключатся;"),
                Line::raw("впишите новый ключ в настройки каждого и перезапустите их."),
            ];
            frame.render_widget(ratatui::widgets::Clear, area);
            frame.render_widget(Paragraph::new(lines).block(Block::bordered().title("Новый orbit_key")), area);
        }
        if let Screen::Pick(pick) = &mut self.screen {
            let area = body.centered(
                Constraint::Percentage(60),
                Constraint::Length(u16::try_from(pick.labels.len() + 2).unwrap_or(u16::MAX)),
            );
            let list = List::new(pick.labels.iter().map(String::as_str))
                .block(Block::bordered().title(pick.title.as_str()))
                .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
            frame.render_widget(ratatui::widgets::Clear, area);
            frame.render_stateful_widget(list, area, &mut pick.state);
        }
    }

    /// Раздел управления поверх списка сессий.
    fn draw_section(&mut self, frame: &mut Frame<'_>, area: ratatui::layout::Rect, section: Section) {
        let (title, lines): (&str, Vec<String>) = match section {
            Section::Nodes => (
                "Узлы",
                self.nodes
                    .iter()
                    .map(|node| {
                        format!(
                            "{} · {} · {}{}",
                            node.runner.name,
                            node.os,
                            presence_name(node.runner.presence),
                            if node.revoked { " · отозван" } else { "" }
                        )
                    })
                    .collect(),
            ),
            Section::Env => (
                "Окружение (значения скрыты)",
                self.env.iter().map(|entry| entry.key.clone()).collect(),
            ),
        };
        let selected = self.manage_selected();
        self.manage.select(selected);
        let list = List::new(lines)
            .block(Block::bordered().title(title))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        frame.render_widget(ratatui::widgets::Clear, area);
        frame.render_stateful_widget(list, area, &mut self.manage);
    }

    fn row(&self, session: &SessionInfo) -> Row<'static> {
        let runner = session
            .runner
            .as_ref()
            .and_then(|fingerprint| self.runners.iter().find(|runner| &runner.fingerprint == fingerprint))
            .map_or_else(|| "—".to_owned(), |runner| runner.name.clone());
        let limits = limits_text(&session.limits);
        Row::new([
            session.agent.name().to_owned(),
            state_name(session.state).to_owned(),
            runner,
            limits,
            reason_text(session),
        ])
    }
}

/// Причина состояния; ожидающий конца хода перезапуск агента показан перед ней.
fn reason_text(session: &SessionInfo) -> String {
    let reason = session.reason.as_deref().unwrap_or("—");
    if session.restart_pending {
        format!("перезапуск ждёт конца хода · {reason}")
    } else {
        reason.to_owned()
    }
}

/// Два самых заполненных окна лимита через « · »; без окон — «—».
fn limits_text(windows: &[UsageWindow]) -> String {
    let mut fullest: Vec<&UsageWindow> = windows.iter().collect();
    fullest.sort_by(|a, b| b.used.total_cmp(&a.used));
    fullest.truncate(2);
    if fullest.is_empty() {
        return "—".to_owned();
    }
    let texts: Vec<String> = fullest.into_iter().map(window_text).collect();
    texts.join(" · ")
}

/// Окно лимита: метка из длины окна (`5ч`, `7д`) или модели и процент.
fn window_text(window: &UsageWindow) -> String {
    let duration = window.duration.map(|seconds| match seconds {
        seconds if seconds >= 2 * 86_400 => format!("{}д", seconds / 86_400),
        seconds => format!("{}ч", seconds / 3_600),
    });
    let label = match (duration, &window.scope) {
        (Some(duration), Some(scope)) => format!("{scope} {duration}"),
        (Some(label), None) => label,
        (None, Some(scope)) => scope.clone(),
        (None, None) => "лимит".to_owned(),
    };
    format!("{label} {:.0}%", window.used * 100.0)
}

/// Что с сессией без запущенного агента и что делать: подсказка идёт первой, чтобы узкий терминал не отрезал её.
fn no_agent_message(session: &SessionInfo) -> String {
    let state = state_name(session.state);
    let reason = session.reason.as_deref().unwrap_or("причина не названа");
    let hint = match session.cause {
        Some(Cause::RunnerRequired) => "R — назначить runner, e — завершить",
        Some(Cause::Restarting | Cause::AgentRestarted) => "он запускается, войдите через несколько секунд",
        _ if session.state == SessionState::Failure => "f — перезапустить с продолжением, e — завершить",
        _ => "дождитесь запуска, e — завершить",
    };
    format!("агент не запущен: {hint} · сессия: {state} ({reason})")
}

fn state_name(state: SessionState) -> &'static str {
    match state {
        SessionState::Success => "работает",
        SessionState::Waiting => "ждёт входа",
        SessionState::Warning => "предупреждение",
        SessionState::Failure => "сбой",
    }
}

/// Новое подключение и смена runner: только свободный runner.
fn free_only(presence: RunnerPresence) -> bool {
    presence == RunnerPresence::Free
}

/// Клон создаётся на runner в сети, занятом или свободном.
fn online(presence: RunnerPresence) -> bool {
    presence != RunnerPresence::Offline
}

fn any_runner(_: RunnerPresence) -> bool {
    true
}

fn presence_icon(presence: RunnerPresence) -> &'static str {
    match presence {
        RunnerPresence::Free => "🔓",
        RunnerPresence::Busy => "🔒",
        RunnerPresence::Offline => "📴",
    }
}

fn presence_name(presence: RunnerPresence) -> &'static str {
    match presence {
        RunnerPresence::Free => "свободен",
        RunnerPresence::Busy => "занят",
        RunnerPresence::Offline => "не в сети",
    }
}

#[cfg(test)]
#[path = "../../tests/worker/tui.rs"]
mod tests;
