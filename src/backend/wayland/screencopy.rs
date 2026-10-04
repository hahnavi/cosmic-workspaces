use cosmic::cctk::screencopy::{
    CaptureFrame, CaptureOptions, CaptureSession, CaptureSource, FailureReason, Formats, Frame,
    ScreencopyFrameData, ScreencopyFrameDataExt, ScreencopyHandler, ScreencopySessionData,
    ScreencopySessionDataExt, ScreencopyState,
};
use cosmic::cctk::wayland_client::{Connection, QueueHandle, WEnum};
use cosmic::cctk::{self};
use cosmic::iced::platform_specific::shell::subsurface_widget::{
    SubsurfaceBuffer, SubsurfaceBufferRelease,
};
use std::array;
use std::sync::{Arc, Weak};

use super::{AppData, Buffer, Capture, CaptureImage, Event};

// Number of buffers to swap between
const BUFFER_COUNT: usize = 2;

pub struct ScreencopySession {
    formats: Option<Formats>,
    // swapchain buffers
    buffers: Option<[Buffer; BUFFER_COUNT]>,
    // Index of the buffer being captured into; the other one is displayed.
    capture_idx: usize,
    session: CaptureSession,
    // Future signaled when the displayed buffer is released by the app.
    // if triple buffer is used, will need more than one.
    release: Option<SubsurfaceBufferRelease>,
}

impl ScreencopySession {
    pub fn new(
        capture: &Arc<Capture>,
        screencopy_state: &ScreencopyState,
        qh: &QueueHandle<AppData>,
    ) -> Self {
        let udata = SessionData {
            session_data: Default::default(),
            capture: Arc::downgrade(capture),
        };

        let session = screencopy_state
            .capturer()
            .create_session(&capture.source, CaptureOptions::empty(), qh, udata)
            .unwrap();

        Self {
            formats: None,
            buffers: None,
            capture_idx: 0,
            session,
            release: None,
        }
    }

    pub fn attach_buffer_and_commit(
        &mut self,
        capture: &Arc<Capture>,
        conn: &Connection,
        qh: &QueueHandle<AppData>,
    ) {
        let Some(back) = self.buffers.as_ref().map(|x| &x[self.capture_idx]) else {
            return;
        };

        // TODO
        // let node = back.node().and_then(|x| x.to_str().map(|x| x.to_string()));

        self.session.capture(
            &back.buffer,
            &back.buffer_damage,
            qh,
            FrameData {
                frame_data: Default::default(),
                capture: Arc::downgrade(capture),
            },
        );
        conn.flush().unwrap();
    }
}

pub struct SessionData {
    session_data: ScreencopySessionData,
    // Weak reference so session can be destroyed when all strong references
    // are dropped.
    pub capture: Weak<Capture>,
}

impl ScreencopySessionDataExt for SessionData {
    fn screencopy_session_data(&self) -> &ScreencopySessionData {
        &self.session_data
    }
}

struct FrameData {
    frame_data: ScreencopyFrameData,
    capture: Weak<Capture>,
}

impl ScreencopyFrameDataExt for FrameData {
    fn screencopy_frame_data(&self) -> &ScreencopyFrameData {
        &self.frame_data
    }
}

impl ScreencopyHandler for AppData {
    fn screencopy_state(&mut self) -> &mut ScreencopyState {
        &mut self.screencopy_state
    }

    fn init_done(
        &mut self,
        conn: &Connection,
        _qh: &QueueHandle<Self>,
        session: &CaptureSession,
        formats: &Formats,
    ) {
        let Some(capture) = Capture::for_session(session) else {
            return;
        };
        let mut session = capture.session.lock().unwrap();
        let Some(session) = session.as_mut() else {
            return;
        };

        session.formats = Some(formats.clone());

        // Create new buffer if none, then start capturing
        if session.buffers.is_none() {
            session.buffers = Some(array::from_fn(|_| self.create_buffer(formats)));
            session.attach_buffer_and_commit(&capture, conn, &self.qh);
        }
    }

    fn ready(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        capture_frame: &CaptureFrame,
        frame: Frame,
    ) {
        let capture = &capture_frame.data::<FrameData>().unwrap().capture;
        let Some(capture) = capture.upgrade() else {
            return;
        };
        let mut session = capture.session.lock().unwrap();
        let Some(session) = session.as_mut() else {
            return;
        };

        let Some(buffers) = session.buffers.as_ref() else {
            log::error!("No capture buffers?");
            return;
        };

        self.clear_idle_timer(&capture.source);

        let idx = session.capture_idx;
        debug_assert!(idx < buffers.len());
        let idx = idx.min(buffers.len() - 1);

        let refreshed = !buffers[idx].buffer_damage.is_empty();
        let content_changed = refreshed || !frame.damage.is_empty();

        session.buffers.as_mut().unwrap()[idx].buffer_damage.clear();
        session.buffers.as_mut().unwrap()[idx ^ 1]
            .buffer_damage
            .extend_from_slice(&frame.damage);

        if !content_changed {
            self.schedule_idle_recapture(&capture);
            return;
        }

        session.capture_idx = idx ^ 1;
        let release = session.release.take();

        let (backing, size) = {
            let front = &session.buffers.as_ref().unwrap()[idx];
            (front.backing.clone(), front.size)
        };
        let (buffer, display_release) = SubsurfaceBuffer::new(backing);
        session.release = Some(display_release);
        let image = CaptureImage {
            wl_buffer: buffer,
            width: size.0,
            height: size.1,
            transform: match frame.transform {
                WEnum::Value(value) => value,
                WEnum::Unknown(value) => panic!("invalid capture transform: {}", value),
            },
            #[cfg(feature = "no-subsurfaces")]
            image: cosmic::widget::image::Handle::from_rgba(size.0, size.1, {
                let front = &session.buffers.as_ref().unwrap()[idx];
                front.mmap.to_vec()
            }),
        };

        let capture_clone = capture.clone();
        let conn = conn.clone();
        let qh = qh.clone();
        self.thread_pool.spawn_ok(async move {
            if let Some(release) = release {
                // Wait for the previously displayed buffer to be released by the app before capturing into it again
                release.await;
            }
            let mut session = capture_clone.session.lock().unwrap();
            let Some(session) = session.as_mut() else {
                return;
            };
            session.attach_buffer_and_commit(&capture_clone, &conn, &qh);
        });

        match &capture.source {
            CaptureSource::Toplevel(toplevel) => {
                let info = self
                    .toplevel_info_state
                    .toplevels()
                    .find(|info| info.foreign_toplevel == *toplevel);
                if let Some(info) = info {
                    self.send_event(Event::ToplevelCapture(info.foreign_toplevel.clone(), image))
                }
            }
            CaptureSource::Workspace(workspace) => {
                self.send_event(Event::WorkspaceCapture(workspace.clone(), image));
            }
            CaptureSource::Output(_) => {
                unreachable!()
            }
        };
    }

    fn failed(
        &mut self,
        conn: &Connection,
        _qh: &QueueHandle<Self>,
        capture_frame: &CaptureFrame,
        reason: WEnum<FailureReason>,
    ) {
        let capture = &capture_frame.data::<FrameData>().unwrap().capture;
        let Some(capture) = capture.upgrade() else {
            return;
        };
        if reason == WEnum::Value(FailureReason::BufferConstraints) {
            // Re-allocate buffers, then trigger another capture
            log::info!("buffer constraint failure; re-allocating");
            let mut session = capture.session.lock().unwrap();
            let Some(session) = session.as_mut() else {
                return;
            };
            if let Some(formats) = &session.formats {
                session.buffers = Some(array::from_fn(|_| self.create_buffer(formats)));
                session.capture_idx = 0;
                session.release = None;
            }
            session.attach_buffer_and_commit(&capture, conn, &self.qh);
        } else {
            // TODO
            if reason == WEnum::Value(FailureReason::Stopped) {
                log::info!("Screencopy frame capture stopped");
            } else {
                log::error!("Screencopy failed: {:?}", reason);
            }
            self.stop_capture(&capture);
        }
    }

    fn stopped(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, session: &CaptureSession) {
        // TODO
        if let Some(capture) = Capture::for_session(session) {
            self.stop_capture(&capture);
        }
    }
}

cctk::delegate_screencopy!(AppData);
