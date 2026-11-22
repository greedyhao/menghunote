//! 笔记开头 YAML frontmatter 的属性面板（Obsidian 风格）。
//!
//! 源码里的 `---\nkey: value\n---` 由 `menghunote-editor` 隐藏，不再铺成正文；
//! 本视图在编辑器上方渲染「笔记属性」行，并把编辑写回同一份源文（一条整篇替换，
//! 沿用现有 `EditorView::replace_source_text`，因此只有一个撤销组）。
//!
//! 输入框复用侧栏重命名的 [`RenameInput`]（单行 + IME + 选区），不新建第二套输入法。
//! 首版只做“常用 YAML 子集”：值按原字符串保存，数组/日期/引号不解析成结构化类型。

use gpui::prelude::*;
use gpui::{
    div, px, AnyElement, App, Context, Entity, IntoElement, MouseButton, MouseDownEvent, Render,
    SharedString, Subscription, WeakEntity, Window,
};

use menghunote_editor::model::frontmatter::{self, Frontmatter, FrontmatterProperty};

use crate::editor::theme;
use crate::editor::view::EditorView;
use crate::sidebar_rename::{RenameEvent, RenameInput};
use crate::ui_text;

const HEADER_HEIGHT: f32 = 28.0;
const ROW_HEIGHT: f32 = 28.0;
const ICON_WIDTH: f32 = 22.0;
const KEY_WIDTH: f32 = 108.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PropertyField {
    Key,
    Value,
}

struct PropertyEdit {
    index: usize,
    field: PropertyField,
    input: Entity<RenameInput>,
    /// 订阅必须活着，否则输入框的 Commit/Cancel 到不了面板。
    _events: Subscription,
}

/// 一篇笔记的属性面板；每篇笔记一个实体，跟随 `EditorView` 重建。
pub struct NotePropertiesPanel {
    view: WeakEntity<EditorView>,
    edit: Option<PropertyEdit>,
    /// 面板是否折叠；折叠后只保留标题行，正文随之上移。
    collapsed: bool,
    _view_observation: Subscription,
}

impl NotePropertiesPanel {
    pub fn new(view: WeakEntity<EditorView>, cx: &mut Context<Self>) -> Self {
        let view_entity = view.upgrade().expect("属性面板只会在编辑器视图存活时创建");
        let observation = cx.observe(&view_entity, move |panel, view, cx| {
            // 外部改动（同步/撤销/画布保存）后索引可能过期；只清编辑态，不动选区。
            if panel.edit.is_some() {
                let source = view.read(cx).source_text();
                if frontmatter::parse(&source).is_none() {
                    panel.edit = None;
                }
            }
            cx.notify();
        });
        Self {
            view,
            edit: None,
            collapsed: false,
            _view_observation: observation,
        }
    }

    fn source(&self, cx: &App) -> Option<String> {
        self.view.upgrade().map(|view| view.read(cx).source_text())
    }

    /// 面板是否折叠（父视图布局读取；不读 `EditorView`，避免重入借用）。
    #[must_use]
    pub fn is_collapsed(&self) -> bool {
        self.collapsed
    }

    /// 折叠后的固定高度（只保留标题行）。
    #[must_use]
    pub fn collapsed_height() -> f32 {
        8.0 + HEADER_HEIGHT + 4.0 + 1.0
    }

    /// 面板占用高度（0 = 没有 frontmatter 或视图已释放）。
    #[must_use]
    pub fn height(&self, cx: &App) -> f32 {
        let Some(source) = self.source(cx) else {
            return 0.0;
        };
        let Some(frontmatter) = frontmatter::parse(&source) else {
            return 0.0;
        };
        if self.collapsed {
            Self::collapsed_height()
        } else {
            panel_height(&frontmatter)
        }
    }

    fn toggle_collapsed(&mut self, cx: &mut Context<Self>) {
        self.collapsed = !self.collapsed;
        self.edit = None;
        // 父视图按高度给正文留位，折叠后必须让 EditorView 重新布局。
        if let Some(view) = self.view.upgrade() {
            view.update(cx, |_, cx| cx.notify());
        }
        cx.notify();
    }

    /// 「清除笔记属性」：只清空属性行，保留 frontmatter 分隔符（面板仍可继续添加）。
    fn clear_properties(&mut self, cx: &mut Context<Self>) {
        self.edit = None;
        let Some(source) = self.source(cx) else {
            return;
        };
        let Some(frontmatter) = frontmatter::parse(&source) else {
            return;
        };
        if frontmatter.properties.is_empty() {
            return;
        }
        self.write_properties(&frontmatter, &[], cx);
    }

    fn begin_edit(
        &mut self,
        index: usize,
        field: PropertyField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = self.source(cx) else {
            return;
        };
        let Some(frontmatter) = frontmatter::parse(&source) else {
            return;
        };
        let Some(property) = frontmatter.properties.get(index) else {
            return;
        };
        let initial = match field {
            PropertyField::Key => property.key.clone(),
            PropertyField::Value => property.value.clone(),
        };
        let input = cx.new(|cx| RenameInput::new_cell_selected(&initial, cx));
        let events = cx.subscribe(
            &input,
            move |panel, _input, event: &RenameEvent, cx| match event {
                RenameEvent::Commit(text) => panel.commit(index, field, text.clone(), cx),
                RenameEvent::Cancel | RenameEvent::Blank => {
                    panel.edit = None;
                    cx.notify();
                }
                RenameEvent::Changed | RenameEvent::Advance { .. } => {}
            },
        );
        input.update(cx, |input, cx| input.focus(window, cx));
        self.edit = Some(PropertyEdit {
            index,
            field,
            input,
            _events: events,
        });
        cx.notify();
    }

    fn commit(&mut self, index: usize, field: PropertyField, text: String, cx: &mut Context<Self>) {
        self.edit = None;
        let Some(source) = self.source(cx) else {
            cx.notify();
            return;
        };
        let Some(frontmatter) = frontmatter::parse(&source) else {
            cx.notify();
            return;
        };
        let mut properties = frontmatter.properties.clone();
        let Some(property) = properties.get_mut(index) else {
            cx.notify();
            return;
        };
        match field {
            PropertyField::Key => {
                let key = text.trim();
                if key.is_empty() {
                    cx.notify();
                    return;
                }
                property.key = key.to_owned();
            }
            PropertyField::Value => property.value = text.trim().to_owned(),
        }
        self.write_properties(&frontmatter, &properties, cx);
    }

    fn add_property(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.source(cx) else {
            return;
        };
        let Some(frontmatter) = frontmatter::parse(&source) else {
            return;
        };
        let mut properties = frontmatter.properties.clone();
        let key = unique_key(&properties, "property");
        properties.push(FrontmatterProperty {
            key,
            value: String::new(),
            line: 0..0,
            key_range: 0..0,
            value_range: 0..0,
        });
        let index = properties.len() - 1;
        self.write_properties(&frontmatter, &properties, cx);
        self.begin_edit(index, PropertyField::Key, window, cx);
    }

    fn delete_property(&mut self, index: usize, cx: &mut Context<Self>) {
        self.edit = None;
        let Some(source) = self.source(cx) else {
            return;
        };
        let Some(frontmatter) = frontmatter::parse(&source) else {
            return;
        };
        let mut properties = frontmatter.properties.clone();
        if index >= properties.len() {
            return;
        }
        properties.remove(index);
        self.write_properties(&frontmatter, &properties, cx);
    }

    fn write_properties(
        &mut self,
        frontmatter: &Frontmatter,
        properties: &[FrontmatterProperty],
        cx: &mut Context<Self>,
    ) {
        let Some(source) = self.source(cx) else {
            return;
        };
        let next = frontmatter.with_properties(&source, properties);
        let Some(view) = self.view.upgrade() else {
            return;
        };
        let applied = view.update(cx, |view, cx| {
            let applied = view.replace_source_text(&next, cx);
            if applied {
                view.move_caret_to_body_start(cx);
            }
            applied
        });
        if !applied {
            eprintln!("[menghunote] 属性面板写回被编辑器拒绝");
        }
        cx.notify();
    }

    fn render_header(&self, property_count: usize, cx: &mut Context<Self>) -> AnyElement {
        let collapsed = self.collapsed;
        div()
            .h(px(HEADER_HEIGHT))
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .child(
                div()
                    .id("properties-collapse")
                    .debug_selector(|| "properties-collapse".to_string())
                    .w(px(18.0))
                    .h(px(18.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::sidebar_selected_bg()))
                    .text_size(px(12.0))
                    .text_color(theme::status_bar_text())
                    .child(if collapsed { "▸" } else { "▾" })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|panel, _event: &MouseDownEvent, _window, cx| {
                            panel.toggle_collapsed(cx);
                            cx.stop_propagation();
                        }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(13.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme::text_color())
                    .child(SharedString::from(ui_text::properties_title())),
            )
            .when(property_count > 0, |this| {
                this.child(
                    div()
                        .id("properties-clear")
                        .debug_selector(|| "properties-clear".to_string())
                        .h(px(20.0))
                        .px_2()
                        .flex()
                        .items_center()
                        .rounded_sm()
                        .text_size(px(11.0))
                        .text_color(theme::status_bar_text())
                        .cursor_pointer()
                        .hover(|style| style.text_color(theme::danger_text()))
                        .child("清除")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|panel, _event: &MouseDownEvent, _window, cx| {
                                panel.clear_properties(cx);
                                cx.stop_propagation();
                            }),
                        ),
                )
            })
            .into_any_element()
    }

    fn render_row(
        &self,
        index: usize,
        property: &FrontmatterProperty,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let value = property.value.trim();
        let empty = Frontmatter::is_empty_value(value);
        let icon = match property.key.as_str() {
            "tags" => "#",
            "date_created" | "date_modified" => "◷",
            "published" => "☑",
            _ => "≡",
        };
        div()
            .id(SharedString::from(format!("property-row-{index}")))
            .debug_selector(move || format!("property-row-{index}"))
            .flex()
            .flex_row()
            .items_center()
            .h(px(ROW_HEIGHT))
            .gap_2()
            .px_2()
            .rounded_sm()
            .hover(|style| style.bg(theme::sidebar_selected_bg()))
            .child(
                div()
                    .w(px(ICON_WIDTH))
                    .flex_shrink_0()
                    .text_center()
                    .text_size(px(12.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(icon)),
            )
            .child(
                div()
                    .id(SharedString::from(format!("property-key-{index}")))
                    .debug_selector(move || format!("property-key-{index}"))
                    .w(px(KEY_WIDTH))
                    .flex_shrink_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(12.0))
                    .text_color(theme::text_color())
                    .cursor_pointer()
                    .child(SharedString::from(property.key.clone()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |panel, _event: &MouseDownEvent, window, cx| {
                            panel.begin_edit(index, PropertyField::Key, window, cx);
                            cx.stop_propagation();
                        }),
                    ),
            )
            .child(
                div()
                    .id(SharedString::from(format!("property-value-{index}")))
                    .debug_selector(move || format!("property-value-{index}"))
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(12.0))
                    .text_color(if empty {
                        theme::status_bar_text()
                    } else {
                        theme::text_color()
                    })
                    .cursor_pointer()
                    .child(SharedString::from(if empty {
                        ui_text::properties_no_value()
                    } else {
                        value.to_owned()
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |panel, _event: &MouseDownEvent, window, cx| {
                            panel.begin_edit(index, PropertyField::Value, window, cx);
                            cx.stop_propagation();
                        }),
                    ),
            )
            .child(
                div()
                    .id(SharedString::from(format!("property-delete-{index}")))
                    .debug_selector(move || format!("property-delete-{index}"))
                    .w(px(20.0))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .text_size(px(13.0))
                    .text_color(theme::status_bar_text())
                    .cursor_pointer()
                    .hover(|style| style.text_color(theme::danger_text()))
                    .child("×")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |panel, _event: &MouseDownEvent, _window, cx| {
                            panel.delete_property(index, cx);
                            cx.stop_propagation();
                        }),
                    ),
            )
            .into_any_element()
    }
}

impl Render for NotePropertiesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(source) = self.source(cx) else {
            return div().into_any_element();
        };
        let Some(frontmatter) = frontmatter::parse(&source) else {
            return div().into_any_element();
        };
        let mut panel = div()
            .id("note-properties")
            .debug_selector(|| "note-properties".to_string())
            .relative()
            .flex()
            .flex_col()
            .w_full()
            .flex_shrink_0()
            .px_2()
            .pt_2()
            .pb_1()
            .border_b(px(1.0))
            .border_color(theme::sidebar_border())
            .bg(theme::window_bg())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_panel, _event: &MouseDownEvent, _window, cx| {
                    cx.stop_propagation();
                }),
            )
            .child(self.render_header(frontmatter.properties.len(), cx));
        if !self.collapsed {
            for (index, property) in frontmatter.properties.iter().enumerate() {
                panel = panel.child(self.render_row(index, property, cx));
            }
            panel = panel.child(
                div()
                    .id("properties-add")
                    .debug_selector(|| "properties-add".to_string())
                    .h(px(26.0))
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .rounded_sm()
                    .text_size(px(12.0))
                    .text_color(theme::status_bar_text())
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::sidebar_selected_bg()))
                    .child("+")
                    .child(SharedString::from(ui_text::properties_add()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|panel, _event: &MouseDownEvent, window, cx| {
                            panel.add_property(window, cx);
                            cx.stop_propagation();
                        }),
                    ),
            );
        }
        if let Some(edit) = &self.edit {
            let (left, width) = match edit.field {
                PropertyField::Key => (px(ICON_WIDTH + 8.0), px(KEY_WIDTH + 8.0)),
                PropertyField::Value => (px(ICON_WIDTH + KEY_WIDTH + 12.0), px(300.0)),
            };
            let top = px(8.0 + HEADER_HEIGHT + edit.index as f32 * ROW_HEIGHT);
            let input = edit.input.clone();
            panel = panel.child(
                div()
                    .id("property-edit")
                    .debug_selector(|| "property-edit".to_string())
                    .absolute()
                    .left(left)
                    .top(top)
                    .w(width)
                    .h(px(24.0))
                    .rounded_sm()
                    .border_1()
                    .border_color(theme::selection_bg())
                    .bg(theme::window_bg())
                    .child(input),
            );
        }
        panel.into_any_element()
    }
}

/// 属性面板的自然高度（像素）：与 `render` 的 `pt/header/rows/add/pb/border` 一致。
///
/// `EditorView` 用它把正文整体下移，面板因此像正文里的一个块一样随滚动移动。
#[must_use]
pub fn panel_height(frontmatter: &Frontmatter) -> f32 {
    let rows = frontmatter.properties.len() as f32 * ROW_HEIGHT;
    8.0 + HEADER_HEIGHT + rows + 26.0 + 4.0 + 1.0
}

fn unique_key(properties: &[FrontmatterProperty], base: &str) -> String {
    if !properties.iter().any(|property| property.key == base) {
        return base.to_owned();
    }
    for suffix in 2.. {
        let key = format!("{base}_{suffix}");
        if !properties.iter().any(|property| property.key == key) {
            return key;
        }
    }
    unreachable!("unbounded suffix loop")
}
