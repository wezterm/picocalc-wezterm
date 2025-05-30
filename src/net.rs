use crate::Irqs;
use crate::config::CONFIG;
use crate::keyboard::{Key, KeyReport, KeyState, Modifiers};
use crate::net::alloc::string::ToString;
use crate::process::{LineEditor, Process, assign_proc, assign_proc_if};
use crate::rng::WezTermRng;
use crate::screen::{SCREEN, SCREEN_HEIGHT, SCREEN_WIDTH, Screen};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use cyw43::Control;
use cyw43_pio::{PioSpi, RM2_CLOCK_DIVIDER};
use embassy_executor::Spawner;
use embassy_futures::select::*;
use embassy_net::dns::{DnsQueryType, DnsSocket};
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpEndpoint, Stack};
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0};
use embassy_rp::pio::Pio;
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embassy_sync::channel::Channel;
use embassy_sync::lazy_lock::LazyLock;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, with_timeout};
use embedded_io_async::{Read, Write as _};
use rand_core::RngCore;
use static_cell::StaticCell;
use sunset::{CliEvent, SessionCommand};
use sunset_embassy::{ChanInOut, ProgressHolder, SSHClient};

extern crate alloc;

type CS = embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

static WIFI_CONTROL: LazyLock<Mutex<CriticalSectionRawMutex, Option<Control<'static>>>> =
    LazyLock::new(|| Mutex::new(None));
static STACK: LazyLock<Mutex<CriticalSectionRawMutex, Option<Stack<'static>>>> =
    LazyLock::new(|| Mutex::new(None));

#[embassy_executor::task]
pub async fn run_cyw43(
    runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_runner(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

pub async fn setup_wifi(
    spawner: &Spawner,
    pin_23: embassy_rp::peripherals::PIN_23, // WL_ON
    pin_24: embassy_rp::peripherals::PIN_24, // WL_D
    pin_25: embassy_rp::peripherals::PIN_25, // WL_CS
    pin_29: embassy_rp::peripherals::PIN_29, // WL_CLK
    pio_0: embassy_rp::peripherals::PIO0,
    dma_ch0: embassy_rp::peripherals::DMA_CH0,
) {
    let fw = include_bytes!("../embassy/cyw43-firmware/43439A0.bin");
    let clm = include_bytes!("../embassy/cyw43-firmware/43439A0_clm.bin");

    // Wireless background task:
    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let (net_device, mut control, runner) = {
        let state = STATE.init(cyw43::State::new());
        let wireless_enable = Output::new(pin_23, Level::Low);
        let wireless_spi = {
            let cs = Output::new(pin_25, Level::High);
            let mut pio = Pio::new(pio_0, Irqs);
            PioSpi::new(
                &mut pio.common,
                pio.sm0,
                RM2_CLOCK_DIVIDER,
                pio.irq0,
                cs,
                pin_24,
                pin_29,
                dma_ch0,
            )
        };
        cyw43::new(state, wireless_enable, wireless_spi, fw).await
    };

    spawner.must_spawn(run_cyw43(runner));
    control.init(clm).await;
    use embassy_net::StackResources;
    static RESOURCES: StaticCell<StackResources<5>> = StaticCell::new();

    let config = embassy_net::Config::dhcpv4(Default::default());
    let (stack, runner) = embassy_net::new(
        net_device,
        config,
        RESOURCES.init(StackResources::new()),
        WezTermRng.next_u64(),
    );
    spawner.must_spawn(net_runner(runner));

    control
        .set_power_management(cyw43::PowerManagementMode::None)
        .await;

    let (ssid, wifi_pw) = {
        let mut config = CONFIG.get().lock().await;
        let ssid = config.fetch("wifi_ssid").await;
        let wifi_pw = config.fetch("wifi_pw").await;
        (ssid, wifi_pw)
    };
    match (ssid, wifi_pw) {
        (Ok(Some(ssid)), Ok(Some(wifi_pw))) => {
            if !ssid.is_empty() {
                print!("Connecting to \u{1b}[1m{ssid}\u{1b}[0m...\r\n");
                match control
                    .join(&ssid, cyw43::JoinOptions::new(wifi_pw.as_bytes()))
                    .await
                {
                    Ok(_) => {}
                    Err(err) => {
                        log::error!("join failed with status={}", err.status);
                        print!("Failed with status {}\r\n", err.status);
                    }
                }
            }
        }
        _ => {
            print!("wifi_ssid and/or wifi_pw are not set\r\n");
        }
    }
    WIFI_CONTROL.get().lock().await.replace(control);

    log::info!("waiting for TCP to be up...");
    stack.wait_config_up().await;
    log::info!("Stack is up!");
    if let Some(v4) = stack.config_v4() {
        log::info!("{v4:?}");
        print!("IP Address {}\r\n", v4.address);
    }

    spawner.must_spawn(crate::time::time_sync(stack));
    STACK.get().lock().await.replace(stack);
}

const TIMEOUT_DURATION: Duration = Duration::from_secs(10);

async fn ssh_channel_task(mut channel: ChanInOut<'_, '_>, key_rx: Arc<Channel<CS, KeyReport, 4>>) {
    log::info!("ssh_channel_task waiting for output");

    loop {
        let mut buf = [0u8; 1024];

        let output = channel.read(&mut buf);
        let input = key_rx.receive();

        match select(output, input).await {
            Either::First(read_result) => match read_result {
                Ok(n) => {
                    if n == 0 {
                        log::warn!("ssh_channel_task: EOF on ssh channel");
                        return;
                    }
                    SCREEN.get().lock().await.parse_bytes(&buf[0..n]);
                }
                Err(err) => {
                    print!("\u{1b}[1mssh_channel_task: {err:?}\r\n");
                    return;
                }
            },
            Either::Second(key_report) => {
                // Encode a key with xterm style keyboard encoding.
                // FIXME: woefully incomplete!

                if key_report.modifiers == Modifiers::CTRL {
                    if let Key::Char(c) = key_report.key {
                        if let Some(mapped) = ctrl_mapping(c) {
                            log::info!(
                                "doing mapped ctrl {} -> {}",
                                c.escape_debug(),
                                mapped.escape_debug()
                            );
                            let mut buf = [0u8; 4];
                            log::info!(
                                "{:?}",
                                with_timeout(
                                    TIMEOUT_DURATION,
                                    channel.write_all(mapped.encode_utf8(&mut buf).as_bytes()),
                                )
                                .await
                            );
                            continue;
                        }
                    }
                }

                if key_report.modifiers == Modifiers::ALT {
                    // Alt sends escape first
                    log::info!("ALT -> send escape first");
                    log::info!(
                        "{:?}",
                        with_timeout(TIMEOUT_DURATION, channel.write_all(b"\x1b")).await
                    );
                }

                if let Key::Char(c) = key_report.key {
                    let mut buf = [0u8; 4];
                    log::info!("just sending {} as-is", c.escape_debug());
                    log::info!(
                        "{:?}",
                        with_timeout(
                            TIMEOUT_DURATION,
                            channel.write_all(c.encode_utf8(&mut buf).as_bytes()),
                        )
                        .await
                    );
                } else {
                    let text = match key_report.key {
                        Key::Enter => "\n",
                        Key::BackSpace => "\u{7f}",
                        Key::Tab => "\t",
                        Key::Escape => "\u{1b}",
                        Key::Up => "\u{1b}[A",
                        Key::Down => "\u{1b}[B",
                        Key::Right => "\u{1b}[C",
                        Key::Left => "\u{1b}[D",
                        Key::Home => "\u{1b}[H",
                        Key::End => "\u{1b}[F",
                        Key::PageUp => "\u{1b}[5~",
                        Key::PageDown => "\u{1b}[6~",
                        Key::None | Key::Char(_) => continue,
                        _ => {
                            continue;
                        }
                    };
                    log::info!("{key_report:?} -> {}", text.escape_debug());
                    log::info!(
                        "{:?}",
                        with_timeout(TIMEOUT_DURATION, channel.write_all(text.as_bytes())).await
                    );
                }
            }
        }
    }
}

#[embassy_executor::task]
async fn ssh_session_task(host: String, command: Option<String>) {
    let Some(stack) = STACK.get().lock().await.as_ref().copied() else {
        print!("network is offline\r\n");
        return;
    };

    let command = command.as_deref();

    let dns_client = DnsSocket::new(stack);

    match dns_client.query(&host, DnsQueryType::A).await {
        Ok(addrs) => {
            log::info!("{host} -> {addrs:?}");
            let mut socket_tx_buf = [0u8; 8192];
            let mut socket_rx_buf = [0u8; 8192];
            let mut tcp_socket = TcpSocket::new(stack, &mut socket_tx_buf, &mut socket_rx_buf);

            match tcp_socket
                .connect(IpEndpoint {
                    addr: addrs[0],
                    port: 22,
                })
                .await
            {
                Ok(()) => {
                    use embassy_futures::select::*;

                    let key_channel = Arc::new(Channel::new());
                    let ssh_proc = Arc::new(SshProcess {
                        key_sender: key_channel.clone(),
                    });
                    let prior_proc = assign_proc(ssh_proc).await;

                    print!("Connected to {host} {}:22\r\n", addrs[0]);
                    let (mut read, mut write) = tcp_socket.split();
                    let mut ssh_tx_buf = [0u8; 8192];
                    let mut ssh_rx_buf = [0u8; 8192];
                    let ssh_client = match SSHClient::new(&mut ssh_tx_buf, &mut ssh_rx_buf) {
                        Ok(client) => client,
                        Err(err) => {
                            print!("SSHClient::new: {err:?}\r\n");
                            return;
                        }
                    };

                    let session_authd_chan =
                        embassy_sync::channel::Channel::<NoopRawMutex, bool, 1>::new();
                    let wait_for_auth = session_authd_chan.receiver();

                    let spawn_session_future = async {
                        if wait_for_auth.receive().await {
                            let channel = ssh_client.open_session_pty().await?;
                            ssh_channel_task(channel, key_channel).await;
                        }
                        Ok::<(), sunset::Error>(())
                    };

                    let runner = ssh_client.run(&mut read, &mut write);
                    let mut progress = ProgressHolder::new();
                    let ssh_ticker = async {
                        loop {
                            match ssh_client.progress(&mut progress).await {
                                Ok(event) => match event {
                                    CliEvent::Hostkey(k) => {
                                        log::info!("host key {:?}", k.hostkey());
                                        k.accept().expect("accept hostkey");
                                    }
                                    CliEvent::Banner(b) => {
                                        if let Ok(b) = b.banner() {
                                            log::info!("banner: {b}");
                                        }
                                    }
                                    CliEvent::Username(req) => {
                                        match CONFIG.get().lock().await.fetch("ssh_user").await {
                                            Ok(Some(pw)) => req.username(&pw),
                                            _ => {
                                                let user =
                                                    prompt_for_input("login: ", PromptKind::Text)
                                                        .await;
                                                match user {
                                                    Some(user) => req.username(&user),
                                                    None => {
                                                        print!("Cancelled\r\n");
                                                        return Ok(());
                                                    }
                                                }
                                            }
                                        }
                                        .expect("set user");
                                    }
                                    CliEvent::Password(req) => {
                                        match CONFIG.get().lock().await.fetch("ssh_pw").await {
                                            Ok(Some(pw)) => req.password(&pw),
                                            _ => {
                                                let user = prompt_for_input(
                                                    "password: ",
                                                    PromptKind::Password,
                                                )
                                                .await;
                                                match user {
                                                    Some(user) => req.password(&user),
                                                    None => req.skip(),
                                                }
                                            }
                                        }
                                        .expect("set pw");
                                    }
                                    CliEvent::Pubkey(req) => {
                                        req.skip().expect("skip pubkey");
                                    }
                                    CliEvent::AgentSign(req) => {
                                        req.skip().expect("skip agentsign");
                                    }
                                    CliEvent::Authenticated => {
                                        log::info!("Authenticated!");
                                        session_authd_chan.sender().send(true).await;
                                    }
                                    CliEvent::SessionOpened(mut s) => {
                                        log::info!("session opened channel {}", s.channel());

                                        use heapless::{String, Vec};

                                        let mut term = String::<32>::new();
                                        let _ = term.push_str("xterm").unwrap();

                                        let pty = {
                                            let screen = SCREEN.get().lock().await;
                                            let rows = screen.height;
                                            let cols = screen.width;

                                            sunset::Pty {
                                                term,
                                                rows: rows.into(),
                                                cols: cols.into(),
                                                width: SCREEN_WIDTH as u32,
                                                height: SCREEN_HEIGHT as u32,
                                                modes: Vec::new(),
                                            }
                                        };

                                        log::info!("requesting pty {pty:?}");
                                        if let Err(err) = s.pty(pty) {
                                            print!("requesting pty failed {err:?}\r\n");
                                            return Err(err);
                                        }
                                        log::info!("setting command");
                                        match &command {
                                            Some(cmd) => {
                                                if let Err(err) = s.cmd(&SessionCommand::Exec(cmd))
                                                {
                                                    print!("command failed: {err:?}\r\n");
                                                    return Err(err);
                                                }
                                            }
                                            None => {
                                                if let Err(err) = s.shell() {
                                                    print!("shell failed: {err:?}\r\n");
                                                    return Err(err);
                                                }
                                            }
                                        }
                                        log::info!("SessionOpened completed");
                                    }
                                    CliEvent::SessionExit(status) => {
                                        print!("[ssh session exit with {status:?}]\r\n");
                                        break;
                                    }
                                    CliEvent::Defunct => {
                                        log::error!("ssh session terminated");
                                        break;
                                    }
                                },
                                Err(err) => {
                                    print!("ssh progress error: {err:?}\r\n");
                                    return Err(err);
                                }
                            }
                        }

                        Ok::<(), sunset::Error>(())
                    };

                    let res = select(runner, select(ssh_ticker, spawn_session_future)).await;
                    log::info!("ssh result is {res:?}");
                    assign_proc(prior_proc).await;
                }
                Err(err) => {
                    print!("failed to connect to port 22: {err:?}\r\n");
                }
            }
        }
        Err(err) => {
            print!("failed to resolve {host}: {err:?}\r\n");
        }
    }
}

#[derive(Copy, Clone)]
enum PromptKind {
    Text,
    Password,
}

async fn prompt_for_input(prompt: &str, kind: PromptKind) -> Option<String> {
    use crate::process::{Mutex, ProcHandle};
    use core::fmt::Write;

    let channel = Arc::new(Channel::<CS, Option<String>, 1>::new());

    struct PromptProc {
        prompt: String,
        input: Mutex<LineEditor>,
        channel: Arc<Channel<CS, Option<String>, 1>>,
        kind: PromptKind,
    }

    impl Drop for PromptProc {
        fn drop(&mut self) {
            self.channel.try_send(None).ok();
        }
    }

    #[async_trait::async_trait(?Send)]
    impl Process for PromptProc {
        fn name(&self) -> &str {
            "prompt"
        }
        async fn render(&self) {
            let mut screen = SCREEN.get().lock().await;
            match self.kind {
                PromptKind::Text => {
                    let input = self.input.lock().await;
                    write!(screen, "\r{} {}\u{1b}[K", self.prompt, input.input()).ok();
                }
                PromptKind::Password => {
                    write!(screen, "\r{}\u{1b}[K", self.prompt).ok();
                }
            }
        }

        fn un_prompt(&self, screen: &mut Screen) {
            write!(screen, "\r\u{1b}[K").ok();
        }

        async fn key_input(&self, key: KeyReport) {
            if key.state != KeyState::Pressed {
                return;
            }
            use crate::keyboard::Modifiers;
            match (key.modifiers, key.key) {
                (Modifiers::CTRL, Key::Char('c' | 'C' | 'd' | 'D')) | (_, Key::Escape) => {
                    self.channel.send(None).await;
                }
                _ => {
                    if let Some(command) = self.input.lock().await.apply_key(key) {
                        write!(SCREEN.get().lock().await, "\r\n").ok();
                        self.channel.send(Some(command)).await;
                    }
                }
            }
        }
    }

    let prompt_proc: ProcHandle = Arc::new(PromptProc {
        prompt: prompt.to_string(),
        input: Mutex::new(LineEditor::default()),
        channel: channel.clone(),
        kind,
    });

    let prior = assign_proc(prompt_proc.clone()).await;
    let response = channel.receive().await;
    let _ = assign_proc_if(prior, |current| Arc::ptr_eq(current, &prompt_proc)).await;
    response
}

pub async fn ssh_command(args: &[&str]) {
    if args.len() > 1 {
        let hostname = args[1].to_string();

        let command: Option<String> = if args.len() > 2 {
            Some(args[2..].join(" "))
        } else {
            None
        };
        let spawn_result = {
            let spawner = Spawner::for_current_executor().await;
            spawner.spawn(ssh_session_task(hostname, command))
        };
        match spawn_result {
            Ok(_) => {}
            Err(err) => {
                print!("failed to start ssh task {err:?}\r\n");
            }
        }
        return;
    }

    print!("Usage: ssh [hostname] [command]\r\n");
}

struct SshProcess {
    key_sender: Arc<Channel<CS, KeyReport, 4>>,
}

#[async_trait::async_trait(?Send)]
impl Process for SshProcess {
    fn name(&self) -> &str {
        "ssh"
    }
    async fn render(&self) {}
    fn un_prompt(&self, _screen: &mut Screen) {}
    async fn key_input(&self, key: KeyReport) {
        if key.state != KeyState::Pressed {
            return;
        }
        self.key_sender.send(key).await;
    }
}

/*
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use heapless::{FnvIndexSet, String};
type WifiSet = FnvIndexSet<String<32>, 16>;
use embassy_sync::lazy_lock::LazyLock;
use embassy_sync::mutex::Mutex as AsyncMutex;
static NETWORKS: LazyLock<AsyncMutex<CriticalSectionRawMutex, WifiSet>> =
    LazyLock::new(|| AsyncMutex::new(WifiSet::new()));

#[embassy_executor::task]
async fn wifi_scanner(mut control: Control<'static>) {
    let mut scanner = control.scan(Default::default()).await;

    while let Some(bss) = scanner.next().await {
        if bss.ssid_len == 0 {
            continue;
        }
        if let Ok(ssid_str) = core::str::from_utf8(&bss.ssid[0..bss.ssid_len as usize]) {
            if let Ok(ssid) = String::try_from(ssid_str) {
                if let Ok(true) = NETWORKS.get().lock().await.insert(ssid) {
                    log::info!("wifi: {ssid_str} = {:x?}", bss.bssid);
                    write!(SCREEN.get().lock().await, "wifi: {ssid_str}\r\n",).ok();
                }
            }
        }
    }
}
*/

/// Taken from wezterm-input-types
/// Map c to its Ctrl equivalent.
/// In theory, this mapping is simply translating alpha characters
/// to upper case and then masking them by 0x1f, but xterm inherits
/// some built-in translation from legacy X11 so that are some
/// aliased mappings and a couple that might be technically tied
/// to US keyboard layout (particularly the punctuation characters
/// produced in combination with SHIFT) that may not be 100%
/// the right thing to do here for users with non-US layouts.
fn ctrl_mapping(c: char) -> Option<char> {
    Some(match c {
        '@' | '`' | ' ' | '2' => '\x00',
        'A' | 'a' => '\x01',
        'B' | 'b' => '\x02',
        'C' | 'c' => '\x03',
        'D' | 'd' => '\x04',
        'E' | 'e' => '\x05',
        'F' | 'f' => '\x06',
        'G' | 'g' => '\x07',
        'H' | 'h' => '\x08',
        'I' | 'i' => '\x09',
        'J' | 'j' => '\x0a',
        'K' | 'k' => '\x0b',
        'L' | 'l' => '\x0c',
        'M' | 'm' => '\x0d',
        'N' | 'n' => '\x0e',
        'O' | 'o' => '\x0f',
        'P' | 'p' => '\x10',
        'Q' | 'q' => '\x11',
        'R' | 'r' => '\x12',
        'S' | 's' => '\x13',
        'T' | 't' => '\x14',
        'U' | 'u' => '\x15',
        'V' | 'v' => '\x16',
        'W' | 'w' => '\x17',
        'X' | 'x' => '\x18',
        'Y' | 'y' => '\x19',
        'Z' | 'z' => '\x1a',
        '[' | '3' | '{' => '\x1b',
        '\\' | '4' | '|' => '\x1c',
        ']' | '5' | '}' => '\x1d',
        '^' | '6' | '~' => '\x1e',
        '_' | '7' | '/' => '\x1f',
        '8' | '?' => '\x7f', // `Delete`
        _ => return None,
    })
}
