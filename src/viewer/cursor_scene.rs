//! Renders the guest cursor inside the viewer scene instead of relying on
//! `gdk::Cursor`. GTK rasterizes pointer cursors at logical scale (scale=1
//! even on HiDPI outputs), so a cursor set through the GTK cursor API is
//! always blurry on scaled displays. Drawing the cursor texture as part of
//! the scene maps guest pixels 1:1 to physical pixels whenever the display
//! itself does, which is what makes it sharp.

use std::{cell::RefCell, rc::Rc};

use gtk::{gdk, glib, graphene, prelude::*, subclass::prelude::*};
use gtk4 as gtk;

use super::{UiState, mouse::MouseMode};

pub(super) struct SceneState {
    texture: Option<gdk::Texture>,
    hotspot: (i32, i32),
    pointer: Option<(f64, f64)>,
    cursor_visible: bool,
    picture: Option<gtk::Picture>,
    ui_state: Option<Rc<RefCell<UiState>>>,
    mouse_mode: Option<Rc<RefCell<MouseMode>>>,
}

impl Default for SceneState {
    fn default() -> Self {
        Self {
            texture: None,
            hotspot: (0, 0),
            pointer: None,
            cursor_visible: true,
            picture: None,
            ui_state: None,
            mouse_mode: None,
        }
    }
}

mod cursor_scene_imp {
    use super::*;

    #[derive(Default)]
    pub struct CursorScene {
        pub(super) state: RefCell<SceneState>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CursorScene {
        const NAME: &'static str = "Qd2CursorScene";
        type Type = super::CursorScene;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for CursorScene {}

    impl WidgetImpl for CursorScene {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let state = self.state.borrow();

            if !state.cursor_visible {
                return;
            }
            let Some(texture) = &state.texture else {
                return;
            };
            let Some((pointer_x, pointer_y)) = state.pointer else {
                return;
            };
            let (Some(picture), Some(ui_state), Some(mouse_mode)) =
                (&state.picture, &state.ui_state, &state.mouse_mode)
            else {
                return;
            };
            // In relative mode the local pointer position says nothing about
            // where the guest keeps its cursor, so don't pretend otherwise.
            if *mouse_mode.borrow() != MouseMode::Absolute {
                return;
            }
            let Some((frame_width, frame_height)) = ui_state.borrow().frame_size else {
                return;
            };
            let Some(bounds) = picture.compute_bounds(self.obj().upcast_ref::<gtk::Widget>())
            else {
                return;
            };

            // The same guest-pixel -> logical-pixel factor ContentFit::Contain
            // applies to the framebuffer, so the cursor scales with the scene.
            let scale = (f64::from(bounds.width()) / f64::from(frame_width))
                .min(f64::from(bounds.height()) / f64::from(frame_height));
            if !scale.is_finite() || scale <= 0.0 {
                return;
            }

            let x = f64::from(bounds.x()) + pointer_x - f64::from(state.hotspot.0) * scale;
            let y = f64::from(bounds.y()) + pointer_y - f64::from(state.hotspot.1) * scale;
            snapshot.append_texture(
                texture,
                &graphene::Rect::new(
                    x as f32,
                    y as f32,
                    (f64::from(texture.width()) * scale) as f32,
                    (f64::from(texture.height()) * scale) as f32,
                ),
            );
        }
    }
}

glib::wrapper! {
    pub struct CursorScene(ObjectSubclass<cursor_scene_imp::CursorScene>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl CursorScene {
    pub(super) fn new(
        picture: &gtk::Picture,
        ui_state: Rc<RefCell<UiState>>,
        mouse_mode: Rc<RefCell<MouseMode>>,
    ) -> Self {
        let scene: Self = glib::Object::builder().build();
        {
            let mut state = scene.imp().state.borrow_mut();
            state.picture = Some(picture.clone());
            state.ui_state = Some(ui_state);
            state.mouse_mode = Some(mouse_mode);
        }
        scene.set_can_target(false);
        scene
    }

    pub(super) fn set_shape(&self, shape: Option<(gdk::Texture, i32, i32)>) {
        {
            let mut state = self.imp().state.borrow_mut();
            match shape {
                Some((texture, hotspot_x, hotspot_y)) => {
                    state.texture = Some(texture);
                    state.hotspot = (hotspot_x, hotspot_y);
                }
                None => state.texture = None,
            }
        }
        self.queue_draw();
    }

    pub(super) fn set_cursor_visible(&self, visible: bool) {
        self.imp().state.borrow_mut().cursor_visible = visible;
        self.queue_draw();
    }

    pub(super) fn set_pointer(&self, pointer: Option<(f64, f64)>) {
        self.imp().state.borrow_mut().pointer = pointer;
        self.queue_draw();
    }
}

/// Follow the local pointer over the picture. Unlike the input controllers in
/// `mouse.rs` this is not gated on the input grab: the drawn cursor replaces
/// the host cursor, which also follows the pointer unconditionally.
pub(super) fn track_pointer(picture: &gtk::Picture, scene: &CursorScene) {
    let motion = gtk::EventControllerMotion::new();
    motion.connect_enter({
        let scene = scene.clone();
        move |_, x, y| scene.set_pointer(Some((x, y)))
    });
    motion.connect_motion({
        let scene = scene.clone();
        move |_, x, y| scene.set_pointer(Some((x, y)))
    });
    motion.connect_leave({
        let scene = scene.clone();
        move |_| scene.set_pointer(None)
    });
    picture.add_controller(motion);
}
