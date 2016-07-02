use std::ptr;
use std::mem;
use std::str;
use std::collections::HashMap;
use std::thread;
use std::time::Duration;
use std::sync::mpsc::{Receiver, Sender, SendError, channel};
use std::sync::Arc;

use rand::{self, Rng};
use x11::{xlib, keysym, xrandr};
use libc::{c_int, c_uint, c_ulong};

use error;
use config::Config;
use saver::{self, Saver, Password, Pointer};

mod display;
pub use self::display::Display;

mod window;
pub use self::window::Window;

/// The actual locker.
///
/// Deals with ugly X11 shit and handles savers.
///
/// TODO(meh): Add a timeout to stopping a saver, if it takes too long it may
///            be stuck or trying to ruse us.
///
/// TODO(meh): Consider timeouts for other saver commands.
#[derive(Debug)]
pub struct Locker {
	receiver: Receiver<Response>,
	sender:   Sender<Request>,
	display:  Arc<Display>,
}

#[derive(Clone)]
pub enum Request {
	Sanitize,

	Start,
	Lock,
	Power(bool),
	Stop,
}

#[derive(Clone)]
pub enum Response {
	Activity,
	Password(String),
}

impl Locker {
	pub fn spawn(config: Config) -> error::Result<Locker> {
		unsafe {
			let     display = Display::open(config.locker())?;
			let mut windows = HashMap::new(): HashMap<xlib::Window, Window>;
			let mut savers  = HashMap::new(): HashMap<xlib::Window, Saver>;

			xlib::XSetScreenSaver(display.id, 0, 0, 0, 0);

			for screen in 0 .. xlib::XScreenCount(display.id) {
				let window = Window::create(display.clone(), screen)?;

				display.observe(window.root);
				windows.insert(window.id, window);
			}

			let (sender, i_receiver) = channel();
			let (i_sender, receiver) = channel();

			// FIXME(meh): The whole `XPending` check and sleeping for 100ms to then
			//             `try_recv` on the channels is very fragile.
			//
			//             Find a better way to do it that plays well with Xlib's
			//             threading model (yeah, right, guess moving to XCB would be
			//             the right way, or reimplementing it properly for Rust).
			{
				let display = display.clone();

				thread::spawn(move || {
					let mut event = mem::zeroed(): xlib::XEvent;

					loop {
						// Check if there are any control messages.
						if let Ok(message) = receiver.try_recv() {
							match message {
								Request::Sanitize => {
									for window in windows.values_mut() {
										window.sanitize();
									}
								}

								Request::Start => {
									for window in windows.values_mut() {
										let mut has_saver = false;

										if !config.savers().is_empty() {
											let name = &config.savers()[rand::thread_rng().gen_range(0, config.savers().len())];

											if let Ok(saver) = Saver::spawn(name) {
												if saver.config(config.saver(name)).is_ok() &&
												   saver.target(display.name(), window.screen, window.id).is_ok()
												{
													savers.insert(window.id, saver);
													has_saver = true;
												}
											}
										}

										if !has_saver {
											// TODO(meh): Do not crash on grab failure.
											window.lock().unwrap();
											window.blank();
										}
									}
								}

								Request::Lock => {

								}

								Request::Power(value) => {
									display.power(value);
								}

								Request::Stop => {
									for window in windows.values_mut() {
										let mut has_saver = false;

										if let Some(saver) = savers.get_mut(&window.id) {
											if saver.stop().is_ok() {
												has_saver = true;
											}
										}

										if !has_saver {
											savers.remove(&window.id);
											window.unlock();
										}
									}
								}
							}

							continue;
						}

						// Check if there are any messages from savers.
						{
							let mut stopped = Vec::new();
							let mut crashed = Vec::new();

							for (&id, saver) in &savers {
								let response = saver.recv();

								if response.is_err() {
									crashed.push(id);
									continue;
								}

								match response.unwrap() {
									Some(saver::Response::Initialized) => {
										if saver.start().is_err() {
											crashed.push(id);
										}
									}

									Some(saver::Response::Started) => {
										// TODO(meh): Do not crash on grab failure.
										windows.get_mut(&id).unwrap().lock().unwrap();
									}

									Some(saver::Response::Stopped) => {
										stopped.push(id);
									}

									None => ()
								}
							}

							// If the saver has crashed blank the window and remove it.
							for id in &crashed {
								windows.get_mut(id).unwrap().blank();
								savers.remove(id);
							}

							// Unlock the stopped savers.
							//
							// TODO(meh): Need to check if the saver was actually requested
							//            to stop.
							for id in &stopped {
								windows.get_mut(id).unwrap().unlock();
								savers.remove(id);
							}
						}

						// Check if there are any pending events, or sleep 100ms.
						if xlib::XPending(display.id) == 0 {
							thread::sleep(Duration::from_millis(100));
							continue;
						}
						else {
							xlib::XNextEvent(display.id, &mut event);
						}

						let     any    = xlib::XAnyEvent::from(event);
						let mut crashed = Vec::new();

						match event.get_type() {
							// Handle screen changes.
							e if display.randr.as_ref().map_or(false, |rr| e == rr.event(xrandr::RRScreenChangeNotify)) => {
								let event = xrandr::XRRScreenChangeNotifyEvent::from(event);

								for window in windows.values_mut() {
									if window.root == event.root {
										window.resize(event.width as u32, event.height as u32);

										if let Some(saver) = savers.get_mut(&window.id) {
											if saver.resize(event.width as u32, event.height as u32).is_err() {
												crashed.push(window.id);
											}
										}
									}
								}
							}

							// Handle keyboard input.
							xlib::KeyPress | xlib::KeyRelease => {
								if let Some(window) = windows.values().find(|w| w.id == any.window) {
									let     key    = xlib::XKeyEvent::from(event);
									let     code   = key.keycode;
									let mut ic_sym = 0;

									let mut buffer = [0u8; 16];
									let     count  = xlib::Xutf8LookupString(window.ic, mem::transmute(&event),
										mem::transmute(buffer.as_mut_ptr()), buffer.len() as c_int,
										&mut ic_sym, ptr::null_mut());

									for ch in str::from_utf8(&buffer[..count as usize]).unwrap_or("").chars() {
										// TODO(meh): Implement password handling.

										for (&id, saver) in &savers {
											if saver.password(Password::Insert).is_err() {
												crashed.push(id);
											}
										}
									}

									let mut sym = xlib::XKeycodeToKeysym(window.display.id, code as xlib::KeyCode, 0);

									if keysym::XK_KP_Space as c_ulong <= sym && sym <= keysym::XK_KP_9 as c_ulong {
										sym = ic_sym;
									}

									match sym as c_uint {
										// XXX(meh): Temporary.
										keysym::XK_Escape => {
											sender.send(Response::Password("".into())).unwrap();
										}

										_ => ()
									}
								}
								else {
									sender.send(Response::Activity).unwrap();
								}
							}

							// Handle mouse button presses.
							xlib::ButtonPress | xlib::ButtonRelease => {
								if let Some(window) = windows.values().find(|w| w.id == any.window) {
									if let Some(saver) = savers.get(&window.id) {
										let event = xlib::XButtonEvent::from(event);

										if saver.pointer(Pointer::Button {
											x: event.x,
											y: event.y,

											button: event.button as u8,
											press:  event.type_ == xlib::ButtonPress,
										}).is_err() {
											crashed.push(window.id);
										}
									}
								}
								else {
									sender.send(Response::Activity).unwrap();
								}
							}

							// Handle mouse motion.
							xlib::MotionNotify => {
								if let Some(window) = windows.values().find(|w| w.id == any.window) {
									if let Some(saver) = savers.get(&window.id) {
										let event = xlib::XMotionEvent::from(event);

										if saver.pointer(Pointer::Move {
											x: event.x,
											y: event.y,
										}).is_err() {
											crashed.push(window.id);
										}
									}
								}
								else {
									sender.send(Response::Activity).unwrap();
								}
							}

							// On window changes, try to observe the window.
							xlib::MapNotify | xlib::UnmapNotify | xlib::ConfigureNotify => {
								display.observe(any.window);
							}

							event => {
								debug!("event for {}: {}", any.window, event);
							}
						}

						for id in &crashed {
							windows.get_mut(id).unwrap().blank();
							savers.remove(id);
						}
					}
				});
			}

			Ok(Locker {
				receiver: i_receiver,
				sender:   i_sender,
				display:  display,
			})
		}
	}

	pub fn sanitize(&self) -> Result<(), SendError<Request>> {
		self.sender.send(Request::Sanitize)
	}

	pub fn start(&self) -> Result<(), SendError<Request>> {
		self.sender.send(Request::Start)
	}

	pub fn lock(&self) -> Result<(), SendError<Request>> {
		self.sender.send(Request::Lock)
	}

	pub fn stop(&self) -> Result<(), SendError<Request>> {
		self.sender.send(Request::Stop)
	}

	pub fn power(&self, value: bool) -> Result<(), SendError<Request>> {
		self.sender.send(Request::Power(value))
	}
}

impl AsRef<Receiver<Response>> for Locker {
	fn as_ref(&self) -> &Receiver<Response> {
		&self.receiver
	}
}

impl AsRef<Sender<Request>> for Locker {
	fn as_ref(&self) -> &Sender<Request> {
		&self.sender
	}
}
