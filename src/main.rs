use std::time::Duration;
use std::{env, process};

use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, LoopHandle, RegistrationToken};
use calloop_wayland_source::WaylandSource;
use configory::{Manager as ConfigManager, Options as ConfigOptions};
use smithay_client_toolkit::data_device_manager::data_source::CopyPasteSource;
use smithay_client_toolkit::reexports::client::globals::{
    self, BindError, GlobalError, GlobalList,
};
use smithay_client_toolkit::reexports::client::protocol::wl_keyboard::WlKeyboard;
use smithay_client_toolkit::reexports::client::protocol::wl_pointer::WlPointer;
use smithay_client_toolkit::reexports::client::protocol::wl_touch::WlTouch;
use smithay_client_toolkit::reexports::client::{
    ConnectError, Connection, DispatchError, QueueHandle,
};
use smithay_client_toolkit::seat::keyboard::{Keysym, Modifiers, RepeatInfo};
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, FmtSubscriber};

use crate::config::{Config, ConfigEventHandler};
use crate::wayland::{ProtocolStates, TextInput};
use crate::window::Window;

mod config;
mod geometry;
mod renderer;
mod skia;
mod wayland;
mod window;

mod gl {
    #![allow(clippy::all, unsafe_op_in_unsafe_fn)]
    include!(concat!(env!("OUT_DIR"), "/gl_bindings.rs"));
}

fn main() {
    // Setup logging.
    let directives = env::var("RUST_LOG").unwrap_or("warn,pinax=info,configory=info".into());
    let env_filter = EnvFilter::builder().parse_lossy(directives);
    FmtSubscriber::builder().with_env_filter(env_filter).with_line_number(true).init();

    info!("Started Pinax");

    if let Err(err) = run() {
        error!("[CRITICAL] {err}");
        process::exit(1);
    }
}

fn run() -> Result<(), Error> {
    // Initialize Wayland connection.
    let connection = Connection::connect_to_env()?;
    let (globals, queue) = globals::registry_queue_init(&connection)?;

    let mut event_loop = EventLoop::try_new()?;
    let mut state = State::new(event_loop.handle(), connection.clone(), &globals, queue.handle())?;

    // Insert wayland source into calloop loop.
    let wayland_source = WaylandSource::new(connection, queue);
    wayland_source.insert(event_loop.handle())?;

    // Start event loop.
    while !state.terminated {
        event_loop.dispatch(None, &mut state)?;
    }

    Ok(())
}

/// Application state.
struct State {
    event_loop: LoopHandle<'static, Self>,
    protocol_states: ProtocolStates,

    keyboard: Option<KeyboardState>,
    pointer: Option<WlPointer>,
    text_input: Vec<TextInput>,
    clipboard: ClipboardState,
    touch: Option<WlTouch>,

    window: Window,

    config: Config,

    terminated: bool,

    _config_manager: ConfigManager,
}

impl State {
    fn new(
        event_loop: LoopHandle<'static, Self>,
        connection: Connection,
        globals: &GlobalList,
        queue: QueueHandle<Self>,
    ) -> Result<Self, Error> {
        let protocol_states = ProtocolStates::new(globals, &queue)?;

        // Initialize configuration state.
        let config_options = ConfigOptions::new("pinax").notify(true);
        let config_handler = ConfigEventHandler::new(&event_loop);
        let config_manager = ConfigManager::with_options(&config_options, config_handler)?;
        let config = config_manager
            .get::<&str, Config>(&[])
            .inspect_err(|err| error!("Config error: {err}"))
            .ok()
            .flatten()
            .unwrap_or_default();

        // Create the Wayland window.
        let window = Window::new(event_loop.clone(), &protocol_states, connection, queue, &config)?;

        Ok(Self {
            protocol_states,
            event_loop,
            config,
            window,
            _config_manager: config_manager,
            terminated: Default::default(),
            text_input: Default::default(),
            clipboard: Default::default(),
            keyboard: Default::default(),
            pointer: Default::default(),
            touch: Default::default(),
        })
    }
}

/// Key status tracking for WlKeyboard.
pub struct KeyboardState {
    wl_keyboard: WlKeyboard,
    repeat_info: RepeatInfo,
    modifiers: Modifiers,

    current_repeat: Option<CurrentRepeat>,
}

impl Drop for KeyboardState {
    fn drop(&mut self) {
        self.wl_keyboard.release();
    }
}

impl KeyboardState {
    pub fn new(wl_keyboard: WlKeyboard) -> Self {
        Self {
            wl_keyboard,
            repeat_info: RepeatInfo::Disable,
            current_repeat: Default::default(),
            modifiers: Default::default(),
        }
    }

    /// Handle new key press.
    fn press_key(
        &mut self,
        event_loop: &LoopHandle<'static, State>,
        time: u32,
        raw: u32,
        keysym: Keysym,
    ) {
        // Update key repeat timers.
        if !keysym.is_modifier_key() {
            self.request_repeat(event_loop, time, raw, keysym);
        }
    }

    /// Handle new key release.
    fn release_key(&mut self, event_loop: &LoopHandle<'static, State>, raw: u32) {
        // Cancel repetition if released key is being repeated.
        if self.current_repeat.as_ref().is_some_and(|repeat| repeat.raw == raw) {
            self.cancel_repeat(event_loop);
        }
    }

    /// Stage new key repetition.
    fn request_repeat(
        &mut self,
        event_loop: &LoopHandle<'static, State>,
        time: u32,
        raw: u32,
        keysym: Keysym,
    ) {
        // Ensure all previous events are cleared.
        self.cancel_repeat(event_loop);

        let (delay_ms, rate) = match self.repeat_info {
            RepeatInfo::Repeat { delay, rate } => (delay, rate),
            _ => return,
        };

        // Stage timer for initial delay.
        let delay = Duration::from_millis(delay_ms as u64);
        let interval = Duration::from_millis(1000 / rate.get() as u64);
        let timer = Timer::from_duration(delay);
        let repeat_source = event_loop.insert_source(timer, move |_, _, state| {
            let keyboard = match state.keyboard.as_mut() {
                Some(keyboard) => keyboard,
                None => return TimeoutAction::Drop,
            };

            state.window.press_key(raw, keysym, keyboard.modifiers);

            TimeoutAction::ToDuration(interval)
        });

        match repeat_source {
            Ok(repeat_source) => {
                self.current_repeat = Some(CurrentRepeat::new(repeat_source, raw, time, delay_ms));
            },
            Err(err) => error!("Failed to stage key repeat timer: {err}"),
        }
    }

    /// Cancel currently staged key repetition.
    fn cancel_repeat(&mut self, event_loop: &LoopHandle<'static, State>) {
        if let Some(CurrentRepeat { repeat_source, .. }) = self.current_repeat.take() {
            event_loop.remove(repeat_source);
        }
    }
}

/// Active keyboard repeat state.
pub struct CurrentRepeat {
    repeat_source: RegistrationToken,
    interval: u32,
    time: u32,
    raw: u32,
}

impl CurrentRepeat {
    pub fn new(repeat_source: RegistrationToken, raw: u32, time: u32, interval: u32) -> Self {
        Self { repeat_source, time, interval, raw }
    }

    /// Get the next key event timestamp.
    pub fn next_time(&mut self) -> u32 {
        self.time += self.interval;
        self.time
    }
}

/// Clipboard content cache.
#[derive(Default)]
struct ClipboardState {
    serial: u32,
    text: String,
    source: Option<CopyPasteSource>,
}

impl ClipboardState {
    fn next_serial(&mut self) -> u32 {
        self.serial += 1;
        self.serial
    }
}

#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("Wayland protocol error for {0}: {1}")]
    WaylandProtocol(&'static str, #[source] BindError),
    #[error("{0}")]
    WaylandDispatch(#[from] DispatchError),
    #[error("{0}")]
    WaylandConnect(#[from] ConnectError),
    #[error("{0}")]
    WaylandGlobal(#[from] GlobalError),
    #[error("{0}")]
    EventLoop(#[from] calloop::Error),
    #[error("{0}")]
    Configory(#[from] configory::Error),
    #[error("{0}")]
    Glutin(#[from] glutin::error::Error),
    #[error("{0}")]
    Notify(#[from] calloop_notify::notify::Error),
}

impl<T> From<calloop::InsertError<T>> for Error {
    fn from(err: calloop::InsertError<T>) -> Self {
        Self::EventLoop(err.error)
    }
}
