use crate::SCREEN;
use crate::keyboard::{Key, KeyReport, KeyState};
use crate::storage::ls_command;
use alloc::string::String;
use alloc::sync::Arc;
use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::lazy_lock::LazyLock;
extern crate alloc;

pub type Mutex<T> = embassy_sync::mutex::Mutex<CriticalSectionRawMutex, T>;
pub type ProcHandle = Arc<&'static dyn Process>;

pub static SHELL: LazyLock<Arc<LocalShell>> = LazyLock::new(LocalShell::new);

pub trait Process {
    async fn key_input(&self, key: KeyReport)
    where
        Self: Sized;
    async fn render(&self)
    where
        Self: Sized;
}

pub struct LocalShell {
    command: Mutex<String>,
    cursor_x: AtomicUsize,
}

impl LocalShell {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            command: Mutex::new(String::new()),
            cursor_x: AtomicUsize::new(0),
        })
    }

    async fn dispatch_command(&self, command: &str) {
        let (arg0, args) = command.split_once(' ').unwrap_or((command, ""));
        match arg0 {
            "ls" => ls_command(args).await,
            "free" => crate::heap::free_command(args).await,
            _ => {
                let mut screen = SCREEN.get().lock().await;
                write!(screen, "Unknown command: {arg0}\r\n").ok();
            }
        }
    }
}

impl Process for LocalShell {
    async fn render(&self) {
        let mut screen = SCREEN.get().lock().await;
        let command = self.command.lock().await;
        write!(screen, "\r$ {}\u{1b}[K", command.as_str()).ok();
    }

    async fn key_input(&self, key: KeyReport) {
        if key.state != KeyState::Pressed {
            return;
        }
        let mut command = self.command.lock().await;
        match key.key {
            Key::Char(c) => {
                let cursor = self.cursor_x.load(Ordering::SeqCst);
                command.insert(cursor, c);
                self.cursor_x.store(cursor + 1, Ordering::SeqCst);
            }
            Key::BackSpace => {
                let cursor = self.cursor_x.load(Ordering::SeqCst);
                if cursor == command.len() {
                    command.pop();
                    self.cursor_x
                        .store(cursor.saturating_sub(1), Ordering::SeqCst);
                } else {
                    command.remove(cursor);
                }
            }
            Key::Enter => {
                let cmd = command.clone();
                command.clear();
                self.cursor_x.store(0, Ordering::SeqCst);
                drop(command);

                write!(SCREEN.get().lock().await, "\r\n").ok();
                self.dispatch_command(&cmd).await;
            }
            _ => {}
        }
    }
}
