use std::{sync::mpsc::SyncSender, time::Duration};

use anyhow::{Context, Result};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};

use crate::qemu::{self, ConnectTarget};

use super::super::{
    InputEvent, ViewerEvent, ViewerReady, audio, clipboard,
    events::EventSender,
    mouse::{self, MouseMode},
};
use super::remote::RemoteConsole;

const DISCONNECT_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Minimum time between a button release and the next press of the same button
/// as seen by the guest (see the double-tap note in `listener_session`).
const MIN_PRESS_GAP: Duration = Duration::from_millis(30);

static INPUT_DEBUG_EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn input_debug_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("QD2_INPUT_DEBUG").is_some_and(|v| v != "0"))
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum SessionOutcome {
    Shutdown,
    Disconnected,
}

pub(super) async fn listener_session(
    target: ConnectTarget,
    event_tx: &EventSender,
    ready_tx: &SyncSender<Result<ViewerReady>>,
    input_rx: &mut tokio_mpsc::UnboundedReceiver<InputEvent>,
    shutdown_rx: &mut oneshot::Receiver<()>,
) -> Result<SessionOutcome> {
    let connection = qemu::connect(target.source_address.as_deref()).await?;
    let mut console = RemoteConsole::new(
        &connection,
        &target.owner,
        target.console_id,
        &target.console_ids,
        target.head_layout.clone(),
    )
    .await
    .with_context(|| format!("failed to open console {}", target.console_id))?;
    if let Err(error) = console.refresh_head_map().await {
        eprintln!("QD2 multi-head warning: could not read the head layout: {error:#}");
    }

    console
        .register_listener(event_tx.clone())
        .await
        .context("failed to register the QEMU display listener")?;
    // QEMU accepts a single clipboard peer per VM. A second viewer (another
    // console of a multi-head guest, or a second window) must still work —
    // just without clipboard sync — so a failed registration is a warning,
    // not a fatal startup error.
    let clipboard = if !target.share_clipboard {
        eprintln!("QD2 clipboard: sharing disabled (--no-clipboard)");
        None
    } else {
        match clipboard::register_clipboard_bridge(&connection, &target.owner, event_tx.clone())
            .await
        {
            Ok(clipboard) => clipboard,
            Err(error) => {
                eprintln!("QD2 clipboard warning: clipboard sharing disabled: {error:#}");
                let _ = event_tx.send(ViewerEvent::Status(format!(
                    "Clipboard sharing unavailable in this window: {error:#}"
                )));
                None
            }
        }
    };
    let _audio =
        match audio::register_audio_output(&connection, &target.owner, &target.vm_name).await {
            Ok(audio) => audio,
            Err(error) => {
                eprintln!("QD2 audio error: {error:#}");
                None
            }
        };
    clipboard::debug(format!(
        "listener clipboard availability: {}",
        if clipboard.is_some() {
            "present"
        } else {
            "absent"
        }
    ));

    let title = format!("{} - QD2", target.vm_name);
    let keyboard_available = target
        .console_interfaces
        .iter()
        .any(|interface| interface == "org.qemu.Display1.Keyboard");
    let mouse_available = target
        .console_interfaces
        .iter()
        .any(|interface| interface == "org.qemu.Display1.Mouse");
    let mut mouse_mode = if mouse_available {
        match console.mouse_is_absolute().await {
            Ok(is_absolute) => MouseMode::from_is_absolute(is_absolute),
            Err(error) => {
                let _ = event_tx.send(ViewerEvent::Status(format!(
                    "Could not detect mouse mode: {error:#}"
                )));
                MouseMode::Relative
            }
        }
    } else {
        MouseMode::Disabled
    };
    let mut disconnect_probe = tokio::time::interval(DISCONNECT_POLL_INTERVAL);

    let ready = ViewerReady {
        title,
        width: target.width,
        height: target.height,
        keyboard_available,
        clipboard_available: clipboard.is_some(),
        mouse_mode,
    };
    let _ = ready_tx.send(Ok(ready));

    let mut last_release: std::collections::HashMap<u32, std::time::Instant> =
        std::collections::HashMap::new();
    loop {
        tokio::select! {
            _ = &mut *shutdown_rx => return Ok(SessionOutcome::Shutdown),
            _ = disconnect_probe.tick() => {
                if console.check_alive().await.is_err() {
                    break;
                }
            }
            maybe_input = input_rx.recv() => match maybe_input {
                Some(input) => {
                    if let InputEvent::ClipboardViewerFocused(focused) = &input {
                        clipboard::debug(format!("listener received ClipboardViewerFocused({focused})"));
                        if let Some(clipboard) = &clipboard {
                            clipboard.set_viewer_focused(*focused);
                        }
                        continue;
                    }

                    if let InputEvent::ClipboardHostChanged(selection, content) = &input {
                        clipboard::debug(format!(
                            "listener received ClipboardHostChanged selection={selection:?}: {}",
                            content
                                .as_ref()
                                .map(|content| content.describe())
                                .unwrap_or_else(|| "empty".to_owned())
                        ));
                        if let Some(clipboard) = &clipboard {
                            if let Err(error) = clipboard
                                .update_host_content(*selection, content.clone())
                                .await
                            {
                                super::super::clipboard::debug(format!(
                                    "update_host_content failed: {error:#}"
                                ));
                                let _ = event_tx.send(ViewerEvent::Status(format!(
                                    "Clipboard sharing failed: {error:#}"
                                )));
                            }
                        } else {
                            super::super::clipboard::debug(
                                "dropping ClipboardHostChanged because no QEMU clipboard is available",
                            );
                        }
                        continue;
                    }

                    if let InputEvent::UiInfo { width, height } = &input {
                        if let Err(error) = console.set_ui_info(*width, *height).await {
                            let _ = event_tx.send(ViewerEvent::Status(format!(
                                "Guest resize request failed: {error:#}"
                            )));
                        }
                        continue;
                    }

                    let needs_mouse_mode = mouse::input_needs_mouse_mode(&input);
                    // A trackpad double tap: libinput on the host holds the first
                    // tap's release until its tap timeout, then emits release and
                    // the second press back to back (0.5 ms apart here). Forwarded
                    // 1:1 over D-Bus they land in the guest as two frames under a
                    // millisecond apart, and the guest's libinput drops the pair as
                    // button bounce: one long press, no double click. A mouse never
                    // produces that pattern. Hold a press until MIN_PRESS_GAP after
                    // the last release of the same button; 34 ms was enough in the
                    // measurement, 0.5 ms was not.
                    if let InputEvent::MousePress(button) = &input {
                        if let Some(released_at) = last_release.get(&(*button as u32)) {
                            let gap = released_at.elapsed();
                            if gap < MIN_PRESS_GAP {
                                tokio::time::sleep(MIN_PRESS_GAP - gap).await;
                            }
                        }
                    }
                    if input_debug_enabled() {
                        // QD2_INPUT_DEBUG=1: one line per forwarded input event, with a
                        // monotonic timestamp in ms, to compare against what the host
                        // delivers and what the guest's libinput sees.
                        let t = INPUT_DEBUG_EPOCH.get_or_init(std::time::Instant::now).elapsed();
                        eprintln!("[qd2-input] {:>10.3} ms {:?}", t.as_secs_f64() * 1000.0, input);
                    }
                    let released_button = match &input {
                        InputEvent::MouseRelease(button) => Some(*button),
                        _ => None,
                    };
                    if let Err(error) = console.handle_input(input).await {
                        let recovered = if needs_mouse_mode {
                            match console.mouse_is_absolute().await {
                                Ok(is_absolute) => {
                                    let detected_mode = MouseMode::from_is_absolute(is_absolute);
                                    if detected_mode != mouse_mode {
                                        mouse_mode = detected_mode;
                                        let _ = event_tx.send(ViewerEvent::MouseModeChanged(detected_mode));
                                        true
                                    } else {
                                        false
                                    }
                                }
                                Err(mode_error) => {
                                    let _ = event_tx.send(ViewerEvent::Status(format!(
                                        "Input forwarding failed: {error:#}\n\nCould not re-check the mouse mode: {mode_error:#}"
                                    )));
                                    true
                                }
                            }
                        } else {
                            false
                        };

                        if !recovered {
                            let _ = event_tx.send(ViewerEvent::Status(format!(
                                "Input forwarding failed: {error:#}"
                            )));
                        }
                    }
                    if let Some(button) = released_button {
                        last_release.insert(button as u32, std::time::Instant::now());
                    }
                }
                None => return Ok(SessionOutcome::Shutdown),
            }
        }
    }

    drop(console);
    drop(connection);
    Ok(SessionOutcome::Disconnected)
}
