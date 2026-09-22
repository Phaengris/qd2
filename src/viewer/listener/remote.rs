use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[cfg(not(unix))]
use anyhow::bail;
use anyhow::{Context, Result};
use qemu_display::{
    ConsoleProxy, Cursor, KeyboardProxy, MouseProxy, MouseSet, Scanout, Update, UpdateDMABUF,
};
#[cfg(unix)]
use qemu_display::{ScanoutDMABUF, ScanoutMap, UpdateMap};
use zbus::{
    Connection,
    proxy::CacheProperties,
    zvariant::{Fd, OwnedObjectPath},
};

#[cfg(unix)]
use std::os::fd::{AsFd, IntoRawFd};

use super::super::{InputEvent, events::EventSender, framebuffer::FrameStreamHandler};

const LISTENER_PATH: &str = "/org/qemu/Display1/Listener";

pub(super) struct RemoteConsole {
    proxy: ConsoleProxy<'static>,
    keyboard: KeyboardProxy<'static>,
    mouse: MouseProxy<'static>,
    listener_connection: Option<Connection>,
    console_id: u32,
    /// Every console of the VM (including ours), for reading head sizes.
    siblings: Vec<(u32, ConsoleProxy<'static>)>,
    explicit_layout: Vec<(u32, i32, i32)>,
    head_map: std::sync::Mutex<Option<HeadMap>>,
    /// This console's own size, for clamping single-head positions.
    self_size: std::sync::Mutex<Option<(u32, u32)>>,
    head_map_refreshed: std::sync::Mutex<Option<std::time::Instant>>,
}

/// Where this console sits inside the guest's whole desktop, in guest pixels.
///
/// QEMU scales an absolute position by the *console's* own size, but the
/// guest's single absolute pointing device spans the *whole* desktop. For a
/// multi-head guest we therefore translate a console-local position into the
/// global one and pre-scale it so QEMU's per-console scaling lands exactly:
/// `x' = (offset_x + x) * console_w / total_w`. Single head = identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HeadMap {
    offset_x: i64,
    offset_y: i64,
    width: i64,
    height: i64,
    total_width: i64,
    total_height: i64,
}

/// Build the head map from `(id, width, height)` of every console. Explicit
/// `(id, x, y)` offsets win; otherwise heads are laid out left-to-right in id
/// order (the default hot-plug arrangement of KDE and GNOME).
pub(super) fn compute_head_map(
    sizes: &[(u32, u32, u32)],
    explicit: &[(u32, i32, i32)],
    self_id: u32,
) -> Option<HeadMap> {
    if sizes.len() < 2 {
        return None;
    }
    let mut sizes = sizes.to_vec();
    sizes.sort_by_key(|(id, _, _)| *id);
    let mut placed = Vec::with_capacity(sizes.len());
    let mut cursor_x: i64 = 0;
    for (id, w, h) in &sizes {
        let (x, y) = match explicit.iter().find(|(eid, _, _)| eid == id) {
            Some((_, x, y)) => (i64::from(*x), i64::from(*y)),
            None => (cursor_x, 0),
        };
        cursor_x = cursor_x.max(x + i64::from(*w));
        placed.push((*id, x, y, i64::from(*w), i64::from(*h)));
    }
    let total_width = placed.iter().map(|(_, x, _, w, _)| x + w).max()?;
    let total_height = placed.iter().map(|(_, _, y, _, h)| y + h).max()?;
    let (_, offset_x, offset_y, width, height) = *placed.iter().find(|(id, ..)| *id == self_id)?;
    Some(HeadMap {
        offset_x,
        offset_y,
        width,
        height,
        total_width,
        total_height,
    })
}

/// Translate a console-local absolute position for QEMU (see [`HeadMap`]).
/// The position may lie outside this head (negative, or past its edge): it
/// is clamped to the guest's WHOLE desktop, so a pointer dragged past the
/// window edge keeps moving onto the neighboring head.
pub(super) fn map_abs_position(map: &HeadMap, x: i64, y: i64) -> (u32, u32) {
    if map.width <= 0 || map.height <= 0 || map.total_width <= 0 || map.total_height <= 0 {
        return (x.max(0) as u32, y.max(0) as u32);
    }
    let global_x = (map.offset_x + x).clamp(0, map.total_width - 1);
    let global_y = (map.offset_y + y).clamp(0, map.total_height - 1);
    let sx = global_x * map.width / map.total_width;
    let sy = global_y * map.height / map.total_height;
    (
        sx.clamp(0, map.width - 1) as u32,
        sy.clamp(0, map.height - 1) as u32,
    )
}

/// Single-head clamp: keep the position inside the console.
pub(super) fn clamp_to_console(size: Option<(u32, u32)>, x: i64, y: i64) -> (u32, u32) {
    match size {
        Some((w, h)) if w > 0 && h > 0 => (
            x.clamp(0, i64::from(w) - 1) as u32,
            y.clamp(0, i64::from(h) - 1) as u32,
        ),
        _ => (x.max(0) as u32, y.max(0) as u32),
    }
}

impl RemoteConsole {
    pub(super) async fn new(
        connection: &Connection,
        owner: &str,
        console_id: u32,
        console_ids: &[u32],
        explicit_layout: Vec<(u32, i32, i32)>,
    ) -> Result<Self> {
        let mut siblings = Vec::new();
        for id in console_ids {
            let path = OwnedObjectPath::try_from(format!("/org/qemu/Display1/Console_{id}"))?;
            let sibling = ConsoleProxy::builder(connection)
                .cache_properties(CacheProperties::No)
                .destination(owner.to_owned())?
                .path(path)?
                .build()
                .await
                .with_context(|| format!("failed to build the proxy for console {id}"))?;
            siblings.push((*id, sibling));
        }
        let object_path =
            OwnedObjectPath::try_from(format!("/org/qemu/Display1/Console_{console_id}"))?;
        let proxy = ConsoleProxy::builder(connection)
            .cache_properties(CacheProperties::No)
            .destination(owner.to_owned())?
            .path(object_path.clone())?
            .build()
            .await
            .with_context(|| format!("failed to build the console proxy for owner `{owner}`"))?;
        let keyboard = KeyboardProxy::builder(connection)
            .destination(owner.to_owned())?
            .path(object_path.clone())?
            .build()
            .await
            .with_context(|| format!("failed to build the keyboard proxy for owner `{owner}`"))?;
        let mouse = MouseProxy::builder(connection)
            .cache_properties(CacheProperties::No)
            .destination(owner.to_owned())?
            .path(object_path)?
            .build()
            .await
            .with_context(|| format!("failed to build the mouse proxy for owner `{owner}`"))?;

        Ok(Self {
            proxy,
            keyboard,
            mouse,
            listener_connection: None,
            console_id,
            siblings,
            explicit_layout,
            head_map: std::sync::Mutex::new(None),
            self_size: std::sync::Mutex::new(None),
            head_map_refreshed: std::sync::Mutex::new(None),
        })
    }

    pub(super) async fn mouse_is_absolute(&self) -> Result<bool> {
        self.mouse
            .is_absolute()
            .await
            .context("failed to query the mouse mode")
    }

    pub(super) async fn check_alive(&self) -> Result<()> {
        self.proxy
            .label()
            .await
            .context("failed to reach the remote console")
            .map(|_| ())
    }

    /// Re-read every head's size and recompute where this console sits in
    /// the guest desktop. Cheap (one property read per head); called at
    /// startup, on every button press, and at most once per second while the
    /// pointer moves, so other windows resizing their heads is picked up.
    pub(super) async fn refresh_head_map(&self) -> Result<()> {
        let mut sizes = Vec::with_capacity(self.siblings.len());
        for (id, proxy) in &self.siblings {
            let width = proxy
                .width()
                .await
                .with_context(|| format!("console {id} width"))?;
            let height = proxy
                .height()
                .await
                .with_context(|| format!("console {id} height"))?;
            sizes.push((*id, width, height));
        }
        if let Some((_, w, h)) = sizes.iter().find(|(id, _, _)| *id == self.console_id) {
            *self.self_size.lock().unwrap() = Some((*w, *h));
        }
        let map = compute_head_map(&sizes, &self.explicit_layout, self.console_id);
        *self.head_map.lock().unwrap() = map;
        *self.head_map_refreshed.lock().unwrap() = Some(std::time::Instant::now());
        Ok(())
    }

    async fn refresh_head_map_if_stale(&self) {
        let stale = self
            .head_map_refreshed
            .lock()
            .unwrap()
            .is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(1));
        if stale && !self.siblings.is_empty() {
            let _ = self.refresh_head_map().await;
        }
    }

    fn map_abs(&self, x: i32, y: i32) -> (u32, u32) {
        let (x, y) = (i64::from(x), i64::from(y));
        match *self.head_map.lock().unwrap() {
            Some(map) => map_abs_position(&map, x, y),
            None => clamp_to_console(*self.self_size.lock().unwrap(), x, y),
        }
    }

    pub(super) async fn handle_input(&self, input: InputEvent) -> Result<()> {
        match input {
            InputEvent::KeyPress(keycode) => self
                .keyboard
                .press(keycode)
                .await
                .with_context(|| format!("failed to send key press for qnum {keycode}")),
            InputEvent::KeyRelease(keycode) => self
                .keyboard
                .release(keycode)
                .await
                .with_context(|| format!("failed to send key release for qnum {keycode}")),
            InputEvent::ClipboardViewerFocused(_) | InputEvent::ClipboardHostChanged(_, _) => {
                Ok(())
            }
            InputEvent::MousePress(button) => {
                if self.siblings.len() >= 2 {
                    let _ = self.refresh_head_map().await;
                }
                self.mouse
                    .press(button)
                    .await
                    .with_context(|| format!("failed to send mouse press for {button:?}"))
            }
            InputEvent::MouseRelease(button) => self
                .mouse
                .release(button)
                .await
                .with_context(|| format!("failed to send mouse release for {button:?}")),
            InputEvent::MouseAbs { x, y } => {
                self.refresh_head_map_if_stale().await;
                let (mx, my) = self.map_abs(x, y);
                self.mouse
                    .set_abs_position(mx, my)
                    .await
                    .with_context(|| format!("failed to move the absolute mouse to {x},{y}"))
            }
            InputEvent::MouseRel { dx, dy } => self
                .mouse
                .rel_motion(dx, dy)
                .await
                .with_context(|| format!("failed to move the relative mouse by {dx},{dy}")),
            InputEvent::MouseWheel(button) => {
                self.mouse
                    .press(button)
                    .await
                    .with_context(|| format!("failed to send mouse wheel press for {button:?}"))?;
                self.mouse
                    .release(button)
                    .await
                    .with_context(|| format!("failed to send mouse wheel release for {button:?}"))
            }
        }
    }

    /// Register the local peer-to-peer listener object that QEMU pushes scanout
    /// updates into for this console.
    pub(super) async fn register_listener(&mut self, event_tx: EventSender) -> Result<()> {
        #[cfg(not(unix))]
        {
            let _ = event_tx;
            bail!("`qd2 connect` currently requires a Unix platform");
        }

        #[cfg(unix)]
        {
            use std::os::unix::net::UnixStream;

            let (socket0, socket1) =
                UnixStream::pair().context("failed to allocate the listener socket pair")?;
            let listener_fd: Fd<'_> = (&socket0).into();
            let shared = Arc::new(SharedListenerState::new(event_tx));

            self.proxy
                .register_listener(listener_fd)
                .await
                .context("QEMU rejected the display listener registration")?;

            let listener_connection = zbus::connection::Builder::unix_stream(socket1)
                .p2p()
                .serve_at(LISTENER_PATH, LocalConsoleListener::new(shared.clone()))?
                .build()
                .await
                .context("failed to publish the local QEMU display listener")?;

            listener_connection
                .object_server()
                .at(LISTENER_PATH, LocalConsoleListenerMap::new(shared.clone()))
                .await
                .context("failed to publish the shared-memory listener interface")?;
            listener_connection
                .object_server()
                .at(LISTENER_PATH, LocalConsoleListenerDmabuf2::new(shared))
                .await
                .context("failed to publish the DMABUF2 listener interface")?;

            self.listener_connection = Some(listener_connection);
            Ok(())
        }
    }
}

struct SharedListenerState {
    handler: Mutex<FrameStreamHandler>,
    disconnected: AtomicBool,
}

impl SharedListenerState {
    fn new(event_tx: EventSender) -> Self {
        Self {
            handler: Mutex::new(FrameStreamHandler::new(event_tx)),
            disconnected: AtomicBool::new(false),
        }
    }

    fn with_handler<T>(&self, f: impl FnOnce(&mut FrameStreamHandler) -> T) -> T {
        let mut handler = self.handler.lock().expect("listener mutex was poisoned");
        f(&mut handler)
    }

    fn disconnected(&self) {
        if !self.disconnected.swap(true, Ordering::SeqCst) {
            self.with_handler(|handler| handler.disconnected());
        }
    }

    fn interfaces(&self) -> Vec<String> {
        self.with_handler(|handler| handler.interfaces())
    }
}

#[derive(Clone)]
struct LocalConsoleListener {
    shared: Arc<SharedListenerState>,
}

impl LocalConsoleListener {
    fn new(shared: Arc<SharedListenerState>) -> Self {
        Self { shared }
    }
}

impl Drop for LocalConsoleListener {
    fn drop(&mut self) {
        self.shared.disconnected();
    }
}

#[zbus::interface(name = "org.qemu.Display1.Listener", spawn = false)]
impl LocalConsoleListener {
    async fn scanout(
        &mut self,
        width: u32,
        height: u32,
        stride: u32,
        format: u32,
        data: serde_bytes::ByteBuf,
    ) {
        self.shared.with_handler(|handler| {
            handler.scanout(Scanout {
                width,
                height,
                stride,
                format,
                data: data.into_vec(),
            });
        });
    }

    async fn update(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        stride: u32,
        format: u32,
        data: serde_bytes::ByteBuf,
    ) {
        self.shared.with_handler(|handler| {
            handler.update(Update {
                x,
                y,
                w,
                h,
                stride,
                format,
                data: data.into_vec(),
            });
        });
    }

    #[cfg(unix)]
    #[zbus(name = "ScanoutDMABUF")]
    async fn scanout_dmabuf(
        &mut self,
        fd: Fd<'_>,
        width: u32,
        height: u32,
        stride: u32,
        fourcc: u32,
        modifier: u64,
        y0_top: bool,
    ) -> zbus::fdo::Result<()> {
        let fd = fd
            .as_fd()
            .try_clone_to_owned()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;

        self.shared.with_handler(|handler| {
            handler.scanout_dmabuf(ScanoutDMABUF {
                fd: [fd.into_raw_fd(), -1, -1, -1],
                width,
                height,
                offset: [0; 4],
                stride: [stride, 0, 0, 0],
                fourcc,
                modifier,
                y0_top,
                num_planes: 1,
            });
        });

        Ok(())
    }

    #[cfg(unix)]
    #[zbus(name = "UpdateDMABUF")]
    async fn update_dmabuf(&mut self, x: i32, y: i32, w: i32, h: i32) -> zbus::fdo::Result<()> {
        self.shared.with_handler(|handler| {
            handler.update_dmabuf(UpdateDMABUF { x, y, w, h });
        });

        Ok(())
    }

    async fn disable(&mut self) {
        self.shared.with_handler(|handler| handler.disable());
    }

    async fn mouse_set(&mut self, x: i32, y: i32, on: i32) {
        self.shared
            .with_handler(|handler| handler.mouse_set(MouseSet { x, y, on }));
    }

    async fn cursor_define(
        &mut self,
        width: i32,
        height: i32,
        hot_x: i32,
        hot_y: i32,
        data: Vec<u8>,
    ) {
        self.shared.with_handler(|handler| {
            handler.cursor_define(Cursor {
                width,
                height,
                hot_x,
                hot_y,
                data,
            });
        });
    }

    #[zbus(property)]
    fn interfaces(&self) -> Vec<String> {
        self.shared.interfaces()
    }
}

#[cfg(unix)]
#[derive(Clone)]
struct LocalConsoleListenerMap {
    shared: Arc<SharedListenerState>,
}

#[cfg(unix)]
impl LocalConsoleListenerMap {
    fn new(shared: Arc<SharedListenerState>) -> Self {
        Self { shared }
    }
}

#[cfg(unix)]
impl Drop for LocalConsoleListenerMap {
    fn drop(&mut self) {
        self.shared.disconnected();
    }
}

#[cfg(unix)]
#[zbus::interface(name = "org.qemu.Display1.Listener.Unix.Map", spawn = false)]
impl LocalConsoleListenerMap {
    async fn scanout_map(
        &mut self,
        fd: Fd<'_>,
        offset: u32,
        width: u32,
        height: u32,
        stride: u32,
        format: u32,
    ) -> zbus::fdo::Result<()> {
        let fd = fd
            .as_fd()
            .try_clone_to_owned()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;

        self.shared.with_handler(|handler| {
            handler.scanout_map(ScanoutMap {
                fd,
                offset,
                width,
                height,
                stride,
                format,
            });
        });

        Ok(())
    }

    async fn update_map(&mut self, x: i32, y: i32, w: i32, h: i32) -> zbus::fdo::Result<()> {
        self.shared
            .with_handler(|handler| handler.update_map(UpdateMap { x, y, w, h }));
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Clone)]
struct LocalConsoleListenerDmabuf2 {
    shared: Arc<SharedListenerState>,
}

#[cfg(unix)]
impl LocalConsoleListenerDmabuf2 {
    fn new(shared: Arc<SharedListenerState>) -> Self {
        Self { shared }
    }
}

#[cfg(unix)]
impl Drop for LocalConsoleListenerDmabuf2 {
    fn drop(&mut self) {
        self.shared.disconnected();
    }
}

#[cfg(unix)]
#[zbus::interface(name = "org.qemu.Display1.Listener.Unix.ScanoutDMABUF2", spawn = false)]
impl LocalConsoleListenerDmabuf2 {
    #[zbus(name = "ScanoutDMABUF2")]
    async fn scanout_dmabuf(
        &mut self,
        fd: Vec<Fd<'_>>,
        _x: u32,
        _y: u32,
        width: u32,
        height: u32,
        offset: Vec<u32>,
        stride: Vec<u32>,
        num_planes: u32,
        fourcc: u32,
        _backing_width: u32,
        _backing_height: u32,
        modifier: u64,
        y0_top: bool,
    ) -> zbus::fdo::Result<()> {
        let mut fds = [-1; 4];
        for (index, fd) in fd.into_iter().take(4).enumerate() {
            let owned = fd
                .as_fd()
                .try_clone_to_owned()
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            fds[index] = owned.into_raw_fd();
        }

        let mut offsets = [0; 4];
        for (index, value) in offset.into_iter().take(4).enumerate() {
            offsets[index] = value;
        }

        let mut strides = [0; 4];
        for (index, value) in stride.into_iter().take(4).enumerate() {
            strides[index] = value;
        }

        self.shared.with_handler(|handler| {
            match super::super::dmabuf::DmabufFrame::try_from_raw_parts(
                fds, width, height, offsets, strides, fourcc, modifier, y0_top, num_planes,
            ) {
                Ok(scanout) => handler.emit_dmabuf_scanout(scanout),
                Err(error) => handler.send_status(format!("Unsupported DMABUF scanout: {error:#}")),
            }
        });

        Ok(())
    }
}

#[cfg(test)]
mod head_map_tests {
    use super::{clamp_to_console, compute_head_map, map_abs_position};

    #[test]
    fn single_head_has_no_map() {
        assert!(compute_head_map(&[(0, 1920, 1080)], &[], 0).is_none());
    }

    #[test]
    fn auto_layout_places_heads_left_to_right() {
        let sizes = [(1, 1280, 1024), (0, 2560, 1440)];
        let head0 = compute_head_map(&sizes, &[], 0).unwrap();
        let head1 = compute_head_map(&sizes, &[], 1).unwrap();
        // head 0 at origin, head 1 to its right; total spans both
        assert_eq!((head0.offset_x, head0.offset_y), (0, 0));
        assert_eq!((head1.offset_x, head1.offset_y), (2560, 0));
        assert_eq!((head0.total_width, head0.total_height), (3840, 1440));
        // clicking the right edge of head 1 lands on the right edge of the desktop
        let (x, _) = map_abs_position(&head1, 1279, 0);
        assert_eq!((2560 + 1279) * 1280 / 3840, i64::from(x));
        // clicking the middle of head 0 stays in head 0's half
        let (x, y) = map_abs_position(&head0, 1280, 720);
        assert_eq!((x, y), (1280 * 2560 / 3840, 720 * 1440 / 1440));
    }

    #[test]
    fn explicit_layout_overrides_auto_placement() {
        let sizes = [(0, 1920, 1080), (1, 1920, 1080)];
        let head1 = compute_head_map(&sizes, &[(1, 0, 1080)], 1).unwrap(); // stacked below
        assert_eq!((head1.offset_x, head1.offset_y), (0, 1080));
        assert_eq!((head1.total_width, head1.total_height), (1920, 2160));
        let (x, y) = map_abs_position(&head1, 100, 100);
        assert_eq!((x, y), (100, (1080 + 100) * 1080 / 2160));
    }

    #[test]
    fn positions_beyond_a_head_continue_onto_the_neighbor() {
        let sizes = [(0, 2560, 1440), (1, 1280, 1024)];
        let head0 = compute_head_map(&sizes, &[], 0).unwrap();
        // 100px past head 0's right edge = 100px into head 1 (global 2660 of 3840)
        let (x, _) = map_abs_position(&head0, 2660, 100);
        assert_eq!(i64::from(x), 2660 * 2560 / 3840);
        assert!(
            x < 2560,
            "pre-scaled position must satisfy QEMU's range check"
        );
        // far left of head 1 = clamped to the desktop's left edge
        let head1 = compute_head_map(&sizes, &[], 1).unwrap();
        assert_eq!(map_abs_position(&head1, -5000, 10).0, 0);
    }

    #[test]
    fn single_head_positions_are_clamped_to_the_console() {
        assert_eq!(clamp_to_console(Some((640, 480)), -7, 500), (0, 479));
        assert_eq!(clamp_to_console(Some((640, 480)), 100, 100), (100, 100));
        assert_eq!(clamp_to_console(None, -3, 9), (0, 9));
    }

    #[test]
    fn mapped_positions_stay_inside_the_console() {
        let sizes = [(0, 1000, 500), (1, 1000, 500)];
        let head1 = compute_head_map(&sizes, &[], 1).unwrap();
        let (x, y) = map_abs_position(&head1, 999, 499);
        assert!(x < 1000 && y < 500);
    }
}
