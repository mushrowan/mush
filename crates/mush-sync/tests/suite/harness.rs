use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mush_ai::types::{Message, Timestamp, UserContent, UserMessage};
use mush_session::{Session, SessionId, SessionStore};
use mush_sync::server::{ServerConfig, serve};
use mush_sync::{Direction, Engine, Keys, Layout, Limits, Remote, Report, Scope};

pub const MASTER: [u8; 32] = [42; 32];

pub struct Server {
    pub url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn server_at(root: &Path) -> Server {
    server_with(root, None).await
}

pub async fn server_with(root: &Path, retention: Option<Duration>) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let config = ServerConfig {
        root: root.to_owned(),
        token_hash: Keys::from_master(&MASTER).token_hash(),
        retention,
    };
    let task = tokio::spawn(async move {
        serve(listener, config).await.unwrap();
    });
    Server { url, task }
}

pub struct Machine {
    pub dir: tempfile::TempDir,
    pub engine: Arc<Engine>,
}

pub fn machine_with(name: &str, url: &str, master: [u8; 32], limits: Limits) -> Machine {
    let dir = tempfile::tempdir().unwrap();
    let keys = Arc::new(Keys::from_master(&master));
    let remote = Remote::new(url, keys.token(), Duration::from_secs(2)).unwrap();
    let engine = Arc::new(Engine::new(
        Layout::new(dir.path()),
        keys,
        remote,
        name.to_owned(),
        limits,
    ));
    Machine { dir, engine }
}

pub fn machine(name: &str, server: &Server) -> Machine {
    machine_with(name, &server.url, MASTER, Limits::default())
}

pub fn user(text: &str) -> Message {
    Message::User(
        UserMessage::builder()
            .content(UserContent::Text(text.into()))
            .timestamp_ms(Timestamp::from_ms(1000))
            .build(),
    )
}

impl Machine {
    pub fn moved_to(self, server: &Server) -> Machine {
        let keys = Arc::new(Keys::from_master(&MASTER));
        let remote = Remote::new(&server.url, keys.token(), Duration::from_secs(2)).unwrap();
        let engine = Arc::new(Engine::new(
            Layout::new(self.dir.path()),
            keys,
            remote,
            self.engine.machine().to_owned(),
            Limits::default(),
        ));
        Machine {
            dir: self.dir,
            engine,
        }
    }

    pub fn store(&self) -> SessionStore {
        SessionStore::new(self.dir.path().join("sessions"))
    }

    pub fn data(&self) -> &Path {
        self.dir.path()
    }

    pub async fn sync(&self) -> Report {
        self.engine
            .pass(Scope::All, Direction::Both, None, Duration::from_secs(5))
            .await
            .unwrap()
    }

    pub fn create(&self, title: &str, texts: &[&str]) -> SessionId {
        let mut session = Session::new("test-model", "/tmp");
        session.meta.title = Some(title.to_owned());
        for text in texts {
            session.push_message(user(text));
        }
        self.store().save(&session).unwrap();
        session.meta.id
    }

    pub fn append(&self, id: &SessionId, text: &str) {
        let store = self.store();
        let mut session = store.load(id).unwrap();
        session.push_message(user(text));
        store.save(&session).unwrap();
    }

    pub fn texts(&self, id: &SessionId) -> Vec<String> {
        self.store()
            .load(id)
            .unwrap()
            .context()
            .iter()
            .filter_map(|message| match message {
                Message::User(user) => Some(user.text()),
                _ => None,
            })
            .collect()
    }

    pub fn title(&self, id: &SessionId) -> Option<String> {
        self.store().resolve(id.as_str()).unwrap().title
    }

    pub fn history(&self, id: &SessionId) -> Vec<u8> {
        std::fs::read(self.store().history_path(id)).unwrap()
    }

    pub fn artifact(&self, id: &SessionId, name: &str) -> PathBuf {
        self.dir
            .path()
            .join("session-artifacts")
            .join(id.as_str())
            .join(name)
    }

    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .store()
            .list()
            .unwrap()
            .into_iter()
            .map(|meta| meta.id.to_string())
            .collect();
        ids.sort();
        ids
    }
}

pub fn bloated(machine: &Machine, after: &str) -> SessionId {
    let mut session = Session::new("test-model", "/tmp");
    session
        .conversation
        .append_message(Message::ToolResult(mush_ai::types::ToolResultMessage {
            tool_call_id: mush_ai::types::ToolCallId::from("call"),
            tool_name: mush_ai::types::ToolName::from("ipython"),
            content: vec![mush_ai::types::ToolResultContentPart::Text(
                mush_ai::types::TextContent {
                    text: "shown to the model".into(),
                },
            )],
            outcome: mush_ai::types::ToolOutcome::Success,
            timestamp_ms: Timestamp::zero(),
            details: Some(serde_json::json!({
                "stdout": "x\n".repeat(mush_ai::types::DETAIL_TEXT_LIMIT)
            })),
        }));
    session.push_message(user(after));
    machine.store().save(&session).unwrap();
    session.meta.id
}

pub fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            } else {
                found.push(entry.path());
            }
        }
    }
    found
}
