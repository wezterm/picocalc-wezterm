use crate::SCREEN;
use crate::keyboard::{Key, KeyReport, KeyState};
use crate::screen::Screen;
use crate::storage::ls_command;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};
use embassy_sync::blocking_mutex::CriticalSectionMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::lazy_lock::LazyLock;
extern crate alloc;

pub type Mutex<T> = embassy_sync::mutex::Mutex<CriticalSectionRawMutex, T>;
pub type ProcHandle = Arc<dyn Process + Send + Sync>;

pub static SHELL: LazyLock<ProcHandle> = LazyLock::new(LocalShell::new);
static CURRENT: LazyLock<CriticalSectionMutex<RefCell<Arc<dyn Process + Send + Sync>>>> =
    LazyLock::new(|| CriticalSectionMutex::new(RefCell::new(Arc::clone(SHELL.get()))));

pub async fn assign_proc(proc: ProcHandle) -> ProcHandle {
    let prior = CURRENT
        .get()
        .lock(|current| core::mem::replace(&mut *current.borrow_mut(), proc.clone()));

    prior.un_prompt(&mut *SCREEN.get().lock().await);
    proc.render().await;
    prior
}

pub fn current_proc() -> ProcHandle {
    CURRENT.get().lock(|cell| Arc::clone(&*cell.borrow()))
}

#[async_trait::async_trait]
pub trait Process {
    async fn key_input(&self, key: KeyReport);
    async fn render(&self);

    // Erase whatever prompt may have been printed
    fn un_prompt(&self, _screen: &mut Screen) {}
}

pub struct LocalShell {
    command: Mutex<String>,
    cursor_x: AtomicUsize,
}

impl LocalShell {
    pub fn new() -> ProcHandle {
        Arc::new(Self {
            command: Mutex::new(String::new()),
            cursor_x: AtomicUsize::new(0),
        })
    }

    async fn dispatch_command(&self, command: &str) {
        let argv: Vec<&str> = command.split(' ').collect();
        let arg0 = argv[0];
        match arg0 {
            "ls" => ls_command(&argv).await,
            "free" => crate::heap::free_command(&argv).await,
            "time" => crate::time::time_command(&argv).await,
            "reboot" => crate::keyboard::reboot(),
            "bootsel" => crate::keyboard::reboot_bootsel(),
            "bat" => crate::keyboard::battery_command(&argv).await,
            "bl" => crate::keyboard::backlight_command(&argv).await,
            "ssh" => crate::net::ssh_command(&argv).await,
            _ => {
                let mut screen = SCREEN.get().lock().await;
                write!(screen, "Unknown command: {arg0}\r\n").ok();
            }
        }
    }
}

#[async_trait::async_trait]
impl Process for LocalShell {
    async fn render(&self) {
        let mut screen = SCREEN.get().lock().await;
        let command = self.command.lock().await;
        write!(screen, "\r$ {}\u{1b}[K", command.as_str()).ok();
    }

    fn un_prompt(&self, screen: &mut Screen) {
        write!(screen, "\r\u{1b}[K").ok();
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
