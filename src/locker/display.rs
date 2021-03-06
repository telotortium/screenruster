// Copyleft (ↄ) meh. <meh@schizofreni.co> | http://meh.schizofreni.co
//
// This file is part of screenruster.
//
// screenruster is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// screenruster is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with screenruster.  If not, see <http://www.gnu.org/licenses/>.

use std::mem;
use std::ptr;
use std::ffi::CStr;
use std::sync::Arc;

use libc::c_int;
use x11::{xlib, xrandr, dpms, xmd};

use error;
use config;
use util;

#[derive(Debug)]
pub struct Display {
	pub id:    *mut xlib::Display,
	pub randr: Option<Extension>,
	pub dpms:  Option<Extension>,

	pub atoms: Atoms,
}

#[derive(Debug)]
pub struct Atoms {
	pub saver: xlib::Atom,
}

#[derive(Debug)]
pub struct Extension {
	event: c_int,
	error: c_int,
}

impl Extension {
	#[inline(always)]
	pub fn event(&self, event: c_int) -> c_int {
		self.event + event
	}

	#[inline(always)]
	pub fn error(&self, error: c_int) -> c_int {
		self.error + error
	}
}

unsafe extern "C" fn ignore(_display: *mut xlib::Display, _error: *mut xlib::XErrorEvent) -> c_int {
	0
}

unsafe extern "C" fn report(display: *mut xlib::Display, error: *mut xlib::XErrorEvent) -> c_int {
	let mut buffer = [0i8; 1024];

	xlib::XGetErrorText(display, (*error).error_code as c_int, buffer.as_mut_ptr(), 1024);

	error!("X11: display={:?} id={:?} serial={:?} code={:?} request={:?} minor={:?} error={:?}",
		CStr::from_ptr(xlib::XDisplayString(display)).to_str().unwrap(),
		(*error).resourceid,
		(*error).serial,
		(*error).error_code,
		(*error).request_code,
		(*error).minor_code,
		CStr::from_ptr(buffer.as_ptr()).to_str().unwrap());

	0
}

impl Display {
	/// Open the default display.
	pub fn open(config: &config::Locker) -> error::Result<Arc<Display>> {
		unsafe {
			let id = if let Some(name) = config.display.as_ref() {
				util::with(name, |name| xlib::XOpenDisplay(name))
			}
			else {
				xlib::XOpenDisplay(ptr::null())
			}.as_mut().ok_or(error::Locker::Display)?;

			xlib::XSetErrorHandler(Some(report));

			Ok(Arc::new(Display {
				id: id,

				randr: {
					let mut event = 0;
					let mut error = 0;

					if xrandr::XRRQueryExtension(id, &mut event, &mut error) == xlib::True {
						Some(Extension { event: event, error: error })
					}
					else {
						None
					}
				},

				dpms: {
					let mut event = 0;
					let mut error = 0;

					if config.dpms &&
					   dpms::DPMSQueryExtension(id, &mut event, &mut error) == xlib::True &&
					   dpms::DPMSCapable(id) == xlib::True
					{
						// DPMS needs to be enabled for `DPMSForceLevel` to actually work,
						// so we just put maximum timeout and handle the states ourselves.
						dpms::DPMSSetTimeouts(id, 0xffff, 0xffff, 0xffff);
						dpms::DPMSEnable(id);

						Some(Extension { event: event, error: error })
					}
					else {
						None
					}
				},

				atoms: Atoms {
					saver: util::with("SCREENRUSTER_SAVER", |name| xlib::XInternAtom(id, name, xlib::False)),
				},
			}))
		}
	}

	/// Get the display name.
	pub fn name(&self) -> &str {
		unsafe {
			CStr::from_ptr(xlib::XDisplayString(self.id)).to_str().unwrap()
		}
	}

	/// Check if the monitor is powered on or not.
	pub fn is_powered(&self) -> bool {
		if self.dpms.is_some() {
			unsafe {
				let mut level = 0;
				let mut state = 0;

				dpms::DPMSInfo(self.id, &mut level, &mut state);

				if state == xlib::False as xmd::BOOL {
					return true;
				}

				match level {
					dpms::DPMSModeOn =>
						true,

					dpms::DPMSModeOff | dpms::DPMSModeStandby | dpms::DPMSModeSuspend =>
						false,

					_ =>
						unreachable!()
				}
			}
		}
		else {
			true
		}
	}

	/// Turn the monitor on or off.
	pub fn power(&self, value: bool) {
		if self.dpms.is_none() || self.is_powered() == value {
			return;
		}

		unsafe {
			dpms::DPMSForceLevel(self.id, if value { dpms::DPMSModeOn } else { dpms::DPMSModeOff });
			xlib::XSync(self.id, xlib::False);
		}
	}

	/// Sanitize the display from bad X11 things.
	pub fn sanitize(&self) {
		unsafe {
			// Reset DPMS settings to usable.
			if self.dpms.is_some() {
				dpms::DPMSSetTimeouts(self.id, 0xffff, 0xffff, 0xffff);
				dpms::DPMSEnable(self.id);
			}

			// Reset screen saver timeout.
			xlib::XSetScreenSaver(self.id, 0, 0, 0, xlib::AllowExposures);
		}
	}

	/// Observe events on the given window and all its children.
	pub fn observe(&self, window: xlib::Window) {
		unsafe {
			let old = xlib::XSetErrorHandler(Some(ignore));
			self._observe(window);
			xlib::XSetErrorHandler(old);
		}
	}

	unsafe fn _observe(&self, window: xlib::Window) {
		let mut root     = mem::zeroed();
		let mut parent   = mem::zeroed();
		let mut children = mem::zeroed();
		let mut count    = mem::zeroed();

		if xlib::XQueryTree(self.id, window, &mut root, &mut parent, &mut children, &mut count) != xlib::True {
			return;
		}

		// Return if the window is one of ours.
		{
			let mut kind   = mem::zeroed();
			let mut format = mem::zeroed();
			let mut count  = mem::zeroed();
			let mut after  = mem::zeroed();
			let mut values = mem::zeroed();

			xlib::XGetWindowProperty(self.id, window, self.atoms.saver, 0, 1, xlib::False, xlib::XA_CARDINAL,
				&mut kind, &mut format, &mut count, &mut after, &mut values);

			if kind == xlib::XA_CARDINAL {
				return;
			}
		}

		let mut attrs = mem::zeroed();
		xlib::XGetWindowAttributes(self.id, window, &mut attrs);

		// Listen to key press and release only if the window is not already listening for them, so we do not
		// steal their keys.
		//
		// Listen for pointer motion events and window changes.
		xlib::XSelectInput(self.id, window, (attrs.all_event_masks | attrs.do_not_propagate_mask) &
			(xlib::KeyPressMask | xlib::KeyReleaseMask) |
			(xlib::PointerMotionMask | xlib::SubstructureNotifyMask));

		if !children.is_null() && count > 0 {
			for i in 0 .. count {
				self._observe(*children.offset(i as isize));
			}

			xlib::XFree(children as *mut _);
		}
	}
}

unsafe impl Send for Display { }
unsafe impl Sync for Display { }

impl Drop for Display {
	fn drop(&mut self) {
		unsafe {
			xlib::XCloseDisplay(self.id);
		}
	}
}
