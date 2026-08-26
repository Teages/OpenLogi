//! The touchpad Gestures tab: every host-classified gesture slot with its
//! bound action, for pads that classify gestures from raw `0x6100` frames
//! (Casa Touch and kin — see `openlogi-core::touchpad`).
//!
//! Mirrors the function-row remapper's shape: a gesture list to select a
//! slot, the shared action catalog beside it to pick the slot's action.
//! Unlike button bindings there is no per-app layer — touchpad gestures are
//! device-global (see `Config::set_touchpad_gesture`).

use std::rc::Rc;

use gpui::{
    App, Context, FontWeight, IntoElement, ParentElement, Render, RenderOnce, Styled, Window, div,
    px, svg,
};
use gpui_component::{Selectable as _, h_flex, v_flex};
use openlogi_core::binding::Action;
use openlogi_core::touchpad::TouchpadGestureId;

use crate::features::mouse::picker::{
    PickFn, action_icon_path, action_rows, compact_panel, divider, editor_scroll_list, title,
};
use crate::state::{AppState, DeviceRecord, StateEvent};
use crate::ui::components::MenuRow;
use crate::ui::theme::{self, Palette, Typography as _};

/// The touchpad gesture binding editor for the selected device.
pub struct TouchpadGesturesView {
    /// The gesture slot whose catalog is showing, if any.
    selected: Option<TouchpadGestureId>,
}

impl TouchpadGesturesView {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self { selected: None }
    }

    fn select(&mut self, gesture: TouchpadGestureId, cx: &mut Context<Self>) {
        // Re-selecting the open gesture collapses the catalog again.
        self.selected = (self.selected != Some(gesture)).then_some(gesture);
        cx.notify();
    }
}

impl Render for TouchpadGesturesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pal = theme::palette(cx);
        let view = cx.entity();
        let selected = self.selected;
        // The effective map carries every vocabulary slot (defaults seeded),
        // so a row never renders an unknown action.
        let gestures = AppState::try_read(cx)
            .map(AppState::effective_touchpad_gestures)
            .unwrap_or_default();

        let list =
            compact_panel(pal).child(
                v_flex().p_1().gap_1().children(
                    TouchpadGestureId::ALL
                        .into_iter()
                        .enumerate()
                        .map(|(idx, gesture)| GestureRow {
                            gesture,
                            // The vocabulary is append-only, so the ALL index
                            // is a stable domain identity for the row id.
                            idx,
                            action: gestures.get(&gesture).cloned(),
                            selected: selected == Some(gesture),
                            view: view.clone(),
                            pal,
                        }),
                ),
            );

        let catalog = match selected {
            Some(gesture) => {
                let view_for_pick = view.clone();
                let on_pick: PickFn = Rc::new(move |action, _window, cx| {
                    AppState::update(cx, |state, cx| {
                        let key = state.current_record().map(DeviceRecord::device_key);
                        state.commit_touchpad_gesture(gesture, Some(action));
                        if let Some(key) = key {
                            cx.emit(StateEvent::BindingsChanged(key));
                        }
                    });
                    view_for_pick.update(cx, |_, vcx| vcx.notify());
                });
                compact_panel(pal)
                    .w(px(340.))
                    .child(title(tr!(gesture.label()), pal))
                    .child(divider(pal))
                    .child(editor_scroll_list(
                        "gesture-actions",
                        action_rows("gesture-action", gestures.get(&gesture), &on_pick, pal),
                    ))
            }
            None => compact_panel(pal)
                .w(px(340.))
                .child(title(tr!("Select a gesture to change its action"), pal)),
        };

        h_flex()
            .w_full()
            .items_start()
            .gap_4()
            .child(div().w(px(320.)).flex_none().child(list))
            .child(div().flex_1().max_w(px(360.)).child(catalog))
    }
}

/// One gesture slot row: the localized gesture label and its current action.
#[derive(IntoElement)]
struct GestureRow {
    gesture: TouchpadGestureId,
    idx: usize,
    action: Option<Action>,
    selected: bool,
    view: gpui::Entity<TouchpadGesturesView>,
    pal: Palette,
}

impl RenderOnce for GestureRow {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let pal = self.pal;
        let action = self.action.clone().unwrap_or(Action::None);
        let action_label = tr!(action.label());
        MenuRow::new(("gesture", self.idx))
            .selected(self.selected)
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_caption()
                            .font_weight(FontWeight::MEDIUM)
                            .child(tr!(self.gesture.label())),
                    )
                    .child(div().flex_1())
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .child(
                                svg()
                                    .path(action_icon_path(&action))
                                    .size_3()
                                    .flex_none()
                                    .text_color(pal.text_muted),
                            )
                            .child(
                                div()
                                    .text_caption()
                                    .text_color(pal.text_muted)
                                    .child(action_label),
                            ),
                    ),
            )
            .on_click({
                let view = self.view;
                let gesture = self.gesture;
                move |_e, _w, cx| {
                    view.update(cx, |v, vcx| v.select(gesture, vcx));
                }
            })
    }
}
