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
use embassy_sync::blocking_mutex::CriticalSectionMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::lazy_lock::LazyLock;
extern crate alloc;

pub type Mutex<T> = embassy_sync::mutex::Mutex<CriticalSectionRawMutex, T>;
pub type ProcHandle = Arc<dyn Process + Send + Sync>;

pub static SHELL: LazyLock<ProcHandle> = LazyLock::new(LocalShell::new);
static CURRENT: LazyLock<CriticalSectionMutex<RefCell<Arc<dyn Process + Send + Sync>>>> =
    LazyLock::new(|| CriticalSectionMutex::new(RefCell::new(Arc::clone(SHELL.get()))));

pub async fn assign_proc_if(
    proc: ProcHandle,
    func: impl FnOnce(&ProcHandle) -> bool,
) -> Option<ProcHandle> {
    let prior = CURRENT.get().lock(|current| {
        if (func)(&current.borrow()) {
            Some(core::mem::replace(&mut *current.borrow_mut(), proc.clone()))
        } else {
            None
        }
    })?;

    prior.un_prompt(&mut *SCREEN.get().lock().await);
    proc.render().await;
    Some(prior)
}

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

#[async_trait::async_trait(?Send)]
pub trait Process {
    async fn key_input(&self, key: KeyReport);
    async fn render(&self);

    fn name(&self) -> &str;

    // Erase whatever prompt may have been printed
    fn un_prompt(&self, _screen: &mut Screen) {}
}

#[derive(Default)]
pub struct LineEditor {
    command: String,
    cursor_x: usize,
}

impl LineEditor {
    pub fn apply_key(&mut self, key: KeyReport) -> Option<String> {
        if key.state != KeyState::Pressed {
            return None;
        }
        match key.key {
            Key::Char(c) => {
                self.command.insert(self.cursor_x, c);
                self.cursor_x += 1;
            }
            Key::BackSpace => {
                if self.cursor_x == self.command.len() {
                    self.command.pop();
                    self.cursor_x = self.cursor_x.saturating_sub(1);
                } else {
                    self.command.remove(self.cursor_x);
                }
            }
            Key::Enter => {
                let cmd = self.command.clone();
                self.command.clear();
                self.cursor_x = 0;

                return Some(cmd);
            }
            _ => {}
        };

        None
    }

    pub fn input(&self) -> &str {
        &self.command
    }
}

pub struct LocalShell {
    command: Mutex<LineEditor>,
}

impl LocalShell {
    pub fn new() -> ProcHandle {
        Arc::new(Self {
            command: Mutex::new(LineEditor::default()),
        })
    }

    async fn dispatch_command(&self, command: &str) {
        let argv: Vec<&str> = command.split(' ').collect();
        let arg0 = argv[0];
        match arg0 {
            "bat" => crate::keyboard::battery_command(&argv).await,
            "bl" => crate::keyboard::backlight_command(&argv).await,
            "bootsel" => crate::keyboard::reboot_bootsel(),
            "cls" => crate::screen::cls_command(&argv).await,
            "config" => crate::config::config_command(&argv).await,
            "free" => crate::heap::free_command(&argv).await,
            "ls" => ls_command(&argv).await,
            "reboot" => crate::keyboard::reboot(),
            "ssh" => crate::net::ssh_command(&argv).await,
            "time" => crate::time::time_command(&argv).await,
            _ => {
                let mut screen = SCREEN.get().lock().await;
                write!(screen, "Unknown command: {arg0}\r\n").ok();
            }
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Process for LocalShell {
    fn name(&self) -> &str {
        "shell"
    }
    async fn render(&self) {
        let mut screen = SCREEN.get().lock().await;
        let command = self.command.lock().await;
        write!(screen, "\r$ {}\u{1b}[K", command.command.as_str()).ok();
    }

    fn un_prompt(&self, screen: &mut Screen) {
        write!(screen, "\r\u{1b}[K").ok();
    }

    async fn key_input(&self, key: KeyReport) {
        if key.state != KeyState::Pressed {
            return;
        }

        // Take care with the scoping, as the write! call
        // below can call through to un_prompt and render
        // and attempt to acquire self.command.lock()
        let command = {
            let mut cmd = self.command.lock().await;
            cmd.apply_key(key)
        };

        if let Some(command) = command {
            write!(SCREEN.get().lock().await, "\r\n").ok();
            self.dispatch_command(&command).await;
        }
    }
}
