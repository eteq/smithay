//!
//! Support to register an open [`AtomicDrmDevice`](AtomicDrmDevice)
//! to an open [`Session`](::backend::session::Session).
//!

use drm::control::{atomic::AtomicModeReq, crtc, property, AtomicCommitFlags, Device as ControlDevice};
use drm::Device as BasicDevice;
use failure::ResultExt;
use nix::libc::dev_t;
use nix::sys::stat;
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak as WeakArc};

use super::{surface::CursorState, AtomicDrmDevice, AtomicDrmSurfaceInternal, Dev};
use crate::backend::drm::{common::Error, DevPath, Surface};
use crate::{
    backend::session::Signal as SessionSignal,
    signaling::{Linkable, Signaler},
};

/// [`SessionObserver`](SessionObserver)
/// linked to the [`AtomicDrmDevice`](AtomicDrmDevice)
/// it was created from.
pub struct AtomicDrmDeviceObserver<A: AsRawFd + 'static> {
    dev: WeakArc<Dev<A>>,
    dev_id: dev_t,
    privileged: bool,
    active: Arc<AtomicBool>,
    backends: Weak<RefCell<HashMap<crtc::Handle, WeakArc<AtomicDrmSurfaceInternal<A>>>>>,
    logger: ::slog::Logger,
}

impl<A: AsRawFd + 'static> Linkable<SessionSignal> for AtomicDrmDevice<A> {
    fn link(&mut self, signaler: Signaler<SessionSignal>) {
        let mut observer = AtomicDrmDeviceObserver {
            dev: Arc::downgrade(&self.dev),
            dev_id: self.dev_id,
            active: self.active.clone(),
            privileged: self.dev.privileged,
            backends: Rc::downgrade(&self.backends),
            logger: self.logger.clone(),
        };

        let token = signaler.register(move |signal| observer.signal(*signal));
        self.links.push(token);
    }
}

impl<A: AsRawFd + 'static> AtomicDrmDeviceObserver<A> {
    fn signal(&mut self, signal: SessionSignal) {
        match signal {
            SessionSignal::PauseSession => self.pause(None),
            SessionSignal::PauseDevice { major, minor } => self.pause(Some((major, minor))),
            SessionSignal::ActivateSession => self.activate(None),
            SessionSignal::ActivateDevice { major, minor, new_fd } => {
                self.activate(Some((major, minor, new_fd)))
            }
        }
    }

    fn pause(&mut self, devnum: Option<(u32, u32)>) {
        if let Some((major, minor)) = devnum {
            if major as u64 != stat::major(self.dev_id) || minor as u64 != stat::minor(self.dev_id) {
                return;
            }
        }

        // TODO: Clear overlay planes (if we ever use them)

        if let Some(backends) = self.backends.upgrade() {
            for surface in backends.borrow().values().filter_map(WeakArc::upgrade) {
                // other ttys that use no cursor, might not clear it themselves.
                // This makes sure our cursor won't stay visible.
                //
                // This usually happens with getty, and a cursor on top of a kernel console looks very weird.
                if let Err(err) = surface.clear_plane(surface.planes.cursor) {
                    warn!(
                        self.logger,
                        "Failed to clear cursor on {:?}: {}", surface.planes.cursor, err
                    );
                }
            }
        }

        self.active.store(false, Ordering::SeqCst);
        if self.privileged {
            if let Some(device) = self.dev.upgrade() {
                if let Err(err) = device.release_master_lock() {
                    error!(self.logger, "Failed to drop drm master state Error: {}", err);
                }
            }
        }
    }

    fn activate(&mut self, devnum: Option<(u32, u32, Option<RawFd>)>) {
        if let Some((major, minor, fd)) = devnum {
            if major as u64 != stat::major(self.dev_id) || minor as u64 != stat::minor(self.dev_id) {
                return;
            } else if let Some(fd) = fd {
                info!(self.logger, "Replacing fd");
                if let Some(device) = self.dev.upgrade() {
                    ::nix::unistd::dup2(device.as_raw_fd(), fd)
                        .expect("Failed to replace file descriptor of drm device");
                }
            }
        }
        if self.privileged {
            if let Some(device) = self.dev.upgrade() {
                if let Err(err) = device.acquire_master_lock() {
                    crit!(self.logger, "Failed to acquire drm master again. Error: {}", err);
                }
            }
        }
        self.active.store(true, Ordering::SeqCst);
        // okay, the previous session/whatever might left the drm devices in any state...
        // lets fix that
        if let Err(err) = self.reset_state() {
            warn!(self.logger, "Unable to reset state after tty switch: {}", err);
            // TODO call drm-handler::error
        }
    }

    fn reset_state(&mut self) -> Result<(), Error> {
        // reset state sets the connectors into a known state (all disabled),
        // for the same reasons we do this on device creation.
        //
        // We might end up with conflicting commit requirements, if we want to restore our state,
        // on top of the state the previous compositor left the device in.
        // This is because we do commits per surface and not per device, so we do a global
        // commit here, to fix any conflicts.
        if let Some(dev) = self.dev.upgrade() {
            let res_handles = ControlDevice::resource_handles(&*dev)
                .compat()
                .map_err(|source| Error::Access {
                    errmsg: "Error loading drm resources",
                    dev: dev.dev_path(),
                    source,
                })?;

            // Disable all connectors (otherwise we might run into conflicting commits when restarting the rendering loop)
            let mut req = AtomicModeReq::new();
            for conn in res_handles.connectors() {
                let prop = dev
                    .prop_mapping
                    .0
                    .get(&conn)
                    .expect("Unknown handle")
                    .get("CRTC_ID")
                    .expect("Unknown property CRTC_ID");
                req.add_property(*conn, *prop, property::Value::CRTC(None));
            }
            // A crtc without a connector has no mode, we also need to reset that.
            // Otherwise the commit will not be accepted.
            for crtc in res_handles.crtcs() {
                let mode_prop = dev
                    .prop_mapping
                    .1
                    .get(&crtc)
                    .expect("Unknown handle")
                    .get("MODE_ID")
                    .expect("Unknown property MODE_ID");
                let active_prop = dev
                    .prop_mapping
                    .1
                    .get(&crtc)
                    .expect("Unknown handle")
                    .get("ACTIVE")
                    .expect("Unknown property ACTIVE");
                req.add_property(*crtc, *active_prop, property::Value::Boolean(false));
                req.add_property(*crtc, *mode_prop, property::Value::Unknown(0));
            }
            dev.atomic_commit(&[AtomicCommitFlags::AllowModeset], req)
                .compat()
                .map_err(|source| Error::Access {
                    errmsg: "Failed to disable connectors",
                    dev: dev.dev_path(),
                    source,
                })?;

            // because we change the state and disabled everything,
            // we want to force a commit (instead of a page-flip) on all used surfaces
            // for the next rendering step to re-activate the connectors.
            //
            // Lets do that, by creating a garbage/non-matching current-state.
            if let Some(backends) = self.backends.upgrade() {
                for surface in backends.borrow().values().filter_map(WeakArc::upgrade) {
                    let mut current = surface.state.write().unwrap();

                    // lets force a non matching state
                    current.connectors.clear();
                    current.mode = unsafe { std::mem::zeroed() };

                    // recreate property blob
                    let mode = {
                        let pending = surface.pending.read().unwrap();
                        pending.mode
                    };
                    surface.use_mode(mode)?;

                    // drop cursor state to force setting the cursor again.
                    *surface.cursor.lock().unwrap() = CursorState {
                        position: None,
                        hotspot: (0, 0),
                        framebuffer: None,
                    };
                }
            }
        }
        Ok(())
    }
}
