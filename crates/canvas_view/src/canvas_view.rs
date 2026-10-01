use std::{
    cell::Cell,
    path::{Path, PathBuf},
    rc::Rc,
};

use anyhow::Result;
use collections::HashSet;
use editor::{
    Editor,
    actions::{Redo, Undo},
};
use gpui::{
    App, Bounds, Context, CursorStyle, Entity, EventEmitter, FocusHandle, Focusable, Hsla, Img,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ObjectFit, ParentElement, PathBuilder, PinchEvent, Pixels, Point, Render, ScrollDelta,
    ScrollWheelEvent, SharedString, Styled, StyledImage, Subscription, Task, Window, actions,
    canvas, div, img, point, px,
};
use language::{Buffer, BufferEvent};
use project::{Project, ProjectEntryId, ProjectPath};
use serde::{Deserialize, Serialize};
use ui::prelude::*;
use util::ResultExt as _;
use workspace::{
    OpenOptions, OpenVisible, Pane, SplitDirection, Workspace,
    item::{Item, ItemBufferKind, ItemEvent, ProjectItem, SaveOptions},
};

actions!(
    canvas_view,
    [
        /// Zooms in on the canvas.
        ZoomIn,
        /// Zooms out of the canvas.
        ZoomOut,
        /// Zooms and pans so that every node is visible.
        FitToContent,
        /// Opens the canvas file as JSON text in a split.
        OpenAsText,
    ]
);

const CANVAS_EXTENSION: &str = "canvas";
const MIN_ZOOM: f32 = 0.05;
const MAX_ZOOM: f32 = 8.0;
const ZOOM_STEP: f32 = 1.2;
const SCROLL_LINE_MULTIPLIER: f32 = 20.0;
const GRID_SIZE: f32 = 10.0;
const FIT_PADDING: f32 = 48.0;
const BASE_FONT_SIZE: f32 = 14.0;
const MIN_READABLE_FONT_SIZE: f32 = 3.0;
const NODE_CORNER_RADIUS: f32 = 10.0;
const NODE_PADDING: f32 = 8.0;
// Themes often make `elevated_surface_background` nearly identical to the editor
// background, so nodes are tinted from the canvas toward the text color instead.
const NODE_SURFACE_TINT: f32 = 0.03;
const EDGE_WIDTH: f32 = 1.5;
const EDGE_BEND_RADIUS: f32 = 8.0;
const EDGE_DASH_LENGTH: f32 = 4.0;
// Matches the "tree" look where an edge leaves its parent, turns early, and runs
// down into the child, instead of bending halfway between the two nodes.
const MAX_FIRST_BEND_OFFSET: f32 = 28.0;
const ARROW_LENGTH: f32 = 9.0;
const ARROW_HALF_WIDTH: f32 = 4.5;

pub fn init(cx: &mut App) {
    workspace::register_project_item::<CanvasView>(cx);
}

/// A [JSON Canvas](https://jsoncanvas.org/spec/1.0/) document, plus two extension
/// fields: `shape` on nodes and `style` on edges.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
struct CanvasDocument {
    #[serde(default)]
    nodes: Vec<CanvasNode>,
    #[serde(default)]
    edges: Vec<CanvasEdge>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
struct CanvasNode {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(serialize_with = "serialize_integer")]
    x: f32,
    #[serde(serialize_with = "serialize_integer")]
    y: f32,
    #[serde(serialize_with = "serialize_integer")]
    width: f32,
    #[serde(serialize_with = "serialize_integer")]
    height: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shape: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

impl CanvasNode {
    fn is_group(&self) -> bool {
        self.kind == "group"
    }

    fn is_circle(&self) -> bool {
        self.shape.as_deref() == Some("circle")
    }

    fn display_text(&self) -> Option<&str> {
        match self.kind.as_str() {
            "file" => self.file.as_deref(),
            "link" => self.url.as_deref(),
            "group" => self.label.as_deref(),
            _ => self.text.as_deref(),
        }
    }

    fn center(&self) -> Point<f32> {
        point(self.x + self.width / 2., self.y + self.height / 2.)
    }

    fn anchor(&self, side: Side) -> Point<f32> {
        match side {
            Side::Top => point(self.x + self.width / 2., self.y),
            Side::Bottom => point(self.x + self.width / 2., self.y + self.height),
            Side::Left => point(self.x, self.y + self.height / 2.),
            Side::Right => point(self.x + self.width, self.y + self.height / 2.),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CanvasEdge {
    id: String,
    from_node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from_side: Option<Side>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from_end: Option<EdgeEnd>,
    to_node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    to_side: Option<Side>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    to_end: Option<EdgeEnd>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    style: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Side {
    Top,
    Right,
    Bottom,
    Left,
}

impl Side {
    fn is_vertical(self) -> bool {
        matches!(self, Side::Top | Side::Bottom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum EdgeEnd {
    None,
    Arrow,
}

// The spec defines positions and sizes as integers, while dragging produces
// fractional values in between snaps.
fn serialize_integer<S: serde::Serializer>(value: &f32, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_i64(value.round() as i64)
}

fn parse_canvas_color(color: &str) -> Option<Hsla> {
    let hex = match color {
        "1" => 0xfb464c,
        "2" => 0xe9973f,
        "3" => 0xe0de71,
        "4" => 0x44cf6e,
        "5" => 0x53dfdd,
        "6" => 0xa882ff,
        _ => {
            let digits = color.strip_prefix('#')?;
            if digits.len() != 6 {
                return None;
            }
            u32::from_str_radix(digits, 16).ok()?
        }
    };
    Some(gpui::rgb(hex).into())
}

fn snap_to_grid(value: f32) -> f32 {
    (value / GRID_SIZE).round() * GRID_SIZE
}

fn default_sides(from: &CanvasNode, to: &CanvasNode) -> (Side, Side) {
    let delta = to.center() - from.center();
    if delta.y.abs() >= delta.x.abs() {
        if delta.y >= 0. {
            (Side::Bottom, Side::Top)
        } else {
            (Side::Top, Side::Bottom)
        }
    } else if delta.x >= 0. {
        (Side::Right, Side::Left)
    } else {
        (Side::Left, Side::Right)
    }
}

/// Returns the corner points of an orthogonal route between two anchors, in canvas coordinates.
fn route_edge(
    start: Point<f32>,
    start_side: Side,
    end: Point<f32>,
    end_side: Side,
) -> Vec<Point<f32>> {
    let mut points = vec![start];
    match (start_side.is_vertical(), end_side.is_vertical()) {
        (true, true) => {
            let bend_y = if start_side == Side::Bottom && end.y > start.y {
                start.y + ((end.y - start.y) / 2.).min(MAX_FIRST_BEND_OFFSET)
            } else {
                (start.y + end.y) / 2.
            };
            points.push(point(start.x, bend_y));
            points.push(point(end.x, bend_y));
        }
        (false, false) => {
            let bend_x = if start_side == Side::Right && end.x > start.x {
                start.x + ((end.x - start.x) / 2.).min(MAX_FIRST_BEND_OFFSET)
            } else {
                (start.x + end.x) / 2.
            };
            points.push(point(bend_x, start.y));
            points.push(point(bend_x, end.y));
        }
        (true, false) => points.push(point(start.x, end.y)),
        (false, true) => points.push(point(end.x, start.y)),
    }
    points.push(end);
    simplify_route(points)
}

fn distance(a: Point<f32>, b: Point<f32>) -> f32 {
    let delta = b - a;
    (delta.x * delta.x + delta.y * delta.y).sqrt()
}

fn simplify_route(points: Vec<Point<f32>>) -> Vec<Point<f32>> {
    let mut simplified: Vec<Point<f32>> = Vec::with_capacity(points.len());
    for current in points {
        if simplified
            .last()
            .is_some_and(|last| distance(*last, current) < 0.01)
        {
            continue;
        }
        if let [.., before, last] = simplified.as_slice() {
            let first_leg = *last - *before;
            let second_leg = current - *last;
            let cross = first_leg.x * second_leg.y - first_leg.y * second_leg.x;
            if cross.abs() < 0.01 {
                simplified.pop();
            }
        }
        simplified.push(current);
    }
    simplified
}

/// Moves `point` toward `toward` by `amount`, without passing it.
fn step_toward(point: Point<f32>, toward: Point<f32>, amount: f32) -> Point<f32> {
    let length = distance(point, toward);
    if length <= f32::EPSILON {
        return point;
    }
    let amount = amount.min(length);
    let delta = toward - point;
    point + gpui::point(delta.x / length * amount, delta.y / length * amount)
}

fn to_pixels(point: Point<f32>) -> Point<Pixels> {
    gpui::point(px(point.x), px(point.y))
}

struct EdgeGeometry {
    /// Corner points relative to the canvas container, in screen pixels.
    points: Vec<Point<f32>>,
    color: Hsla,
    dashed: bool,
    arrow_at_start: bool,
    arrow_at_end: bool,
}

fn paint_edge(edge: &EdgeGeometry, origin: Point<Pixels>, zoom: f32, window: &mut Window) {
    let origin = point(f32::from(origin.x), f32::from(origin.y));
    let mut points: Vec<Point<f32>> = edge.points.iter().map(|point| *point + origin).collect();
    let arrow_length = (ARROW_LENGTH * zoom).max(4.);
    let arrow_half_width = (ARROW_HALF_WIDTH * zoom).max(2.);

    let mut arrows = Vec::new();
    if edge.arrow_at_end
        && let [.., before, tip] = points.as_mut_slice()
    {
        arrows.push((*tip, *before));
        *tip = step_toward(*tip, *before, arrow_length);
    }
    if edge.arrow_at_start
        && let [tip, after, ..] = points.as_mut_slice()
    {
        arrows.push((*tip, *after));
        *tip = step_toward(*tip, *after, arrow_length);
    }

    let [first, .., last] = points.as_slice() else {
        return;
    };

    let mut builder = PathBuilder::stroke(px((EDGE_WIDTH * zoom).max(1.)));
    if edge.dashed {
        let dash = px((EDGE_DASH_LENGTH * zoom).max(2.));
        builder = builder.dash_array(&[dash, dash]);
    }
    builder.move_to(to_pixels(*first));
    for corner_points in points.windows(3) {
        let [previous, corner, next] = corner_points else {
            continue;
        };
        let radius = (EDGE_BEND_RADIUS * zoom)
            .min(distance(*previous, *corner) / 2.)
            .min(distance(*corner, *next) / 2.);
        builder.line_to(to_pixels(step_toward(*corner, *previous, radius)));
        builder.curve_to(
            to_pixels(step_toward(*corner, *next, radius)),
            to_pixels(*corner),
        );
    }
    builder.line_to(to_pixels(*last));
    if let Some(path) = builder.build().log_err() {
        window.paint_path(path, edge.color);
    }

    for (tip, from) in arrows {
        let length = distance(tip, from);
        if length <= f32::EPSILON {
            continue;
        }
        let direction = point((tip.x - from.x) / length, (tip.y - from.y) / length);
        let base = tip - point(direction.x * arrow_length, direction.y * arrow_length);
        let perpendicular = point(
            -direction.y * arrow_half_width,
            direction.x * arrow_half_width,
        );
        let mut arrow = PathBuilder::fill();
        arrow.move_to(to_pixels(tip));
        arrow.line_to(to_pixels(base + perpendicular));
        arrow.line_to(to_pixels(base - perpendicular));
        arrow.close();
        if let Some(path) = arrow.build().log_err() {
            window.paint_path(path, edge.color);
        }
    }
}

pub struct CanvasItem {
    buffer: Entity<Buffer>,
    project_path: ProjectPath,
    entry_id: Option<ProjectEntryId>,
}

impl project::ProjectItem for CanvasItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        // For single-file worktrees the relative path is empty, so fall back
        // to the absolute path to detect canvases opened directly.
        let abs_path = project.read(cx).absolute_path(path, cx);
        let is_canvas = path.path.extension() == Some(CANVAS_EXTENSION)
            || abs_path
                .as_ref()
                .and_then(|abs_path| abs_path.extension())
                .is_some_and(|extension| extension == CANVAS_EXTENSION);
        if !is_canvas {
            return None;
        }

        let project = project.clone();
        let path = path.clone();
        Some(cx.spawn(async move |cx| {
            let buffer = project
                .update(cx, |project, cx| project.open_buffer(path.clone(), cx))
                .await?;
            let entry_id = project.read_with(cx, |project, cx| {
                project.entry_for_path(&path, cx).map(|entry| entry.id)
            });
            Ok(cx.new(|_| CanvasItem {
                buffer,
                project_path: path,
                entry_id,
            }))
        }))
    }

    fn entry_id(&self, _: &App) -> Option<ProjectEntryId> {
        self.entry_id
    }

    fn project_path(&self, _: &App) -> Option<ProjectPath> {
        Some(self.project_path.clone())
    }

    fn is_dirty(&self) -> bool {
        // The view reports dirtiness from the buffer, which needs an `App` to read.
        false
    }
}

enum Interaction {
    Panning {
        last_position: Point<Pixels>,
    },
    DraggingNode {
        node_index: usize,
        start_mouse_position: Point<Pixels>,
        start_node_position: Point<f32>,
        moved: bool,
    },
}

pub enum CanvasViewEvent {
    TitleChanged,
}

pub struct CanvasView {
    canvas_item: Entity<CanvasItem>,
    buffer: Entity<Buffer>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    document: CanvasDocument,
    parse_error: Option<SharedString>,
    zoom_level: f32,
    pan_offset: Point<Pixels>,
    container_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    needs_fit: bool,
    interaction: Option<Interaction>,
    _buffer_subscription: Subscription,
}

impl CanvasView {
    fn new(
        canvas_item: Entity<CanvasItem>,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Self {
        let buffer = canvas_item.read(cx).buffer.clone();
        let buffer_subscription = cx.subscribe(&buffer, Self::on_buffer_event);
        let mut this = Self {
            canvas_item,
            buffer,
            project,
            focus_handle: cx.focus_handle(),
            document: CanvasDocument::default(),
            parse_error: None,
            zoom_level: 1.0,
            pan_offset: Point::default(),
            container_bounds: Rc::new(Cell::new(None)),
            needs_fit: true,
            interaction: None,
            _buffer_subscription: buffer_subscription,
        };
        this.reload_document(cx);
        this
    }

    fn on_buffer_event(&mut self, _: Entity<Buffer>, event: &BufferEvent, cx: &mut Context<Self>) {
        match event {
            // Our own writes come back through here too. Re-parsing them is harmless
            // because they produce the document we already have, and it's what makes
            // undo and edits from a split text editor show up on the canvas.
            BufferEvent::Edited { .. } | BufferEvent::Reloaded => {
                self.reload_document(cx);
                cx.notify();
            }
            BufferEvent::DirtyChanged | BufferEvent::Saved | BufferEvent::FileHandleChanged => {
                cx.emit(CanvasViewEvent::TitleChanged);
            }
            _ => {}
        }
    }

    fn reload_document(&mut self, cx: &App) {
        let text = self.buffer.read(cx).text();
        if text.trim().is_empty() {
            self.document = CanvasDocument::default();
            self.parse_error = None;
            return;
        }
        // On error, keep showing the last good document so that typing in a split
        // text editor doesn't blank the canvas on every keystroke.
        match serde_json::from_str::<CanvasDocument>(&text) {
            Ok(document) => {
                self.document = document;
                self.parse_error = None;
            }
            Err(error) => {
                self.parse_error = Some(format!("Invalid canvas file: {error}").into());
            }
        }
    }

    fn write_document(&mut self, cx: &mut Context<Self>) {
        let text = match serde_json::to_string_pretty(&self.document) {
            Ok(text) => text + "\n",
            Err(error) => {
                log::error!("failed to serialize canvas: {error}");
                return;
            }
        };
        self.buffer.update(cx, |buffer, cx| {
            let end = buffer.len();
            buffer.edit([(0..end, text)], None, cx);
        });
    }

    fn canvas_to_local(&self, canvas_point: Point<f32>) -> Point<Pixels> {
        point(
            self.pan_offset.x + px(canvas_point.x * self.zoom_level),
            self.pan_offset.y + px(canvas_point.y * self.zoom_level),
        )
    }

    fn canvas_to_local_f32(&self, canvas_point: Point<f32>) -> Point<f32> {
        let local = self.canvas_to_local(canvas_point);
        point(f32::from(local.x), f32::from(local.y))
    }

    fn set_zoom(&mut self, new_zoom: f32, anchor: Option<Point<Pixels>>, cx: &mut Context<Self>) {
        let old_zoom = self.zoom_level;
        self.zoom_level = new_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        if let Some(bounds) = self.container_bounds.get() {
            let anchor = anchor.unwrap_or_else(|| bounds.center()) - bounds.origin;
            let zoom_ratio = self.zoom_level / old_zoom;
            self.pan_offset = anchor - (anchor - self.pan_offset) * zoom_ratio;
        }
        cx.notify();
    }

    fn fit_to_content_now(&mut self, bounds: Bounds<Pixels>) {
        self.needs_fit = false;
        let content_bounds = self.document.nodes.iter().fold(None, |bounds, node| {
            let (min_x, min_y, max_x, max_y) =
                bounds.unwrap_or((f32::MAX, f32::MAX, f32::MIN, f32::MIN));
            Some((
                min_x.min(node.x),
                min_y.min(node.y),
                max_x.max(node.x + node.width),
                max_y.max(node.y + node.height),
            ))
        });
        let Some((min_x, min_y, max_x, max_y)) = content_bounds else {
            self.zoom_level = 1.0;
            self.pan_offset = Point::default();
            return;
        };

        let container_width = f32::from(bounds.size.width);
        let container_height = f32::from(bounds.size.height);
        let available_width = (container_width - 2. * FIT_PADDING).max(1.);
        let available_height = (container_height - 2. * FIT_PADDING).max(1.);
        let content_width = (max_x - min_x).max(1.);
        let content_height = (max_y - min_y).max(1.);
        self.zoom_level = (available_width / content_width)
            .min(available_height / content_height)
            .clamp(MIN_ZOOM, 1.0);
        self.pan_offset = point(
            px(container_width / 2. - (min_x + max_x) / 2. * self.zoom_level),
            px(container_height / 2. - (min_y + max_y) / 2. * self.zoom_level),
        );
    }

    fn zoom_in(&mut self, _: &ZoomIn, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom_level * ZOOM_STEP, None, cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom_level / ZOOM_STEP, None, cx);
    }

    fn fit_to_content(&mut self, _: &FitToContent, _window: &mut Window, cx: &mut Context<Self>) {
        match self.container_bounds.get() {
            Some(bounds) => self.fit_to_content_now(bounds),
            None => self.needs_fit = true,
        }
        cx.notify();
    }

    fn undo(&mut self, _: &Undo, _window: &mut Window, cx: &mut Context<Self>) {
        self.buffer.update(cx, |buffer, cx| {
            buffer.undo(cx);
        });
    }

    fn redo(&mut self, _: &Redo, _window: &mut Window, cx: &mut Context<Self>) {
        self.buffer.update(cx, |buffer, cx| {
            buffer.redo(cx);
        });
    }

    fn open_as_text(&mut self, _: &OpenAsText, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = Workspace::for_window(window, cx) else {
            return;
        };
        let buffer = self.buffer.clone();
        let project = self.project.clone();
        workspace.update(cx, |workspace, cx| {
            let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project), window, cx));
            workspace.split_item(SplitDirection::Right, Box::new(editor), window, cx);
        });
    }

    fn open_file(&self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = Workspace::for_window(window, cx) else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            workspace
                .open_abs_path(
                    path,
                    OpenOptions {
                        visible: Some(OpenVisible::None),
                        ..OpenOptions::default()
                    },
                    window,
                    cx,
                )
                .detach_and_log_err(cx);
        });
    }

    fn handle_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.modifiers.control || event.modifiers.platform {
            let delta: f32 = match event.delta {
                ScrollDelta::Pixels(pixels) => pixels.y.into(),
                ScrollDelta::Lines(lines) => lines.y * SCROLL_LINE_MULTIPLIER,
            };
            let zoom_factor = if delta > 0.0 {
                1.0 + delta.abs() * 0.01
            } else {
                1.0 / (1.0 + delta.abs() * 0.01)
            };
            self.set_zoom(self.zoom_level * zoom_factor, Some(event.position), cx);
        } else {
            let delta = match event.delta {
                ScrollDelta::Pixels(pixels) => pixels,
                ScrollDelta::Lines(lines) => lines.map(|line| px(line * SCROLL_LINE_MULTIPLIER)),
            };
            self.pan_offset += delta;
            cx.notify();
        }
    }

    fn handle_pinch(&mut self, event: &PinchEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(
            self.zoom_level * (1.0 + event.delta),
            Some(event.position),
            cx,
        );
    }

    fn start_panning(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        self.interaction = Some(Interaction::Panning {
            last_position: event.position,
        });
        cx.notify();
    }

    fn start_node_drag(
        &mut self,
        node_index: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        window.focus(&self.focus_handle, cx);
        let Some(node) = self.document.nodes.get(node_index) else {
            return;
        };
        if event.click_count == 2 {
            if let Some(path) = self.file_node_path(node, cx) {
                self.open_file(path, window, cx);
            }
            return;
        }
        self.interaction = Some(Interaction::DraggingNode {
            node_index,
            start_mouse_position: event.position,
            start_node_position: point(node.x, node.y),
            moved: false,
        });
        cx.notify();
    }

    fn handle_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The button can be released outside the canvas, where we get no mouse-up.
        if event.pressed_button.is_none() {
            if self.interaction.is_some() {
                self.finish_interaction(cx);
            }
            return;
        }

        match &mut self.interaction {
            Some(Interaction::Panning { last_position }) => {
                self.pan_offset += event.position - *last_position;
                *last_position = event.position;
                cx.notify();
            }
            Some(Interaction::DraggingNode {
                node_index,
                start_mouse_position,
                start_node_position,
                moved,
            }) => {
                let Some(node) = self.document.nodes.get_mut(*node_index) else {
                    return;
                };
                let mouse_delta = event.position - *start_mouse_position;
                let x = snap_to_grid(
                    start_node_position.x + f32::from(mouse_delta.x) / self.zoom_level,
                );
                let y = snap_to_grid(
                    start_node_position.y + f32::from(mouse_delta.y) / self.zoom_level,
                );
                if x != node.x || y != node.y {
                    node.x = x;
                    node.y = y;
                    *moved = true;
                    cx.notify();
                }
            }
            None => {}
        }
    }

    fn handle_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.finish_interaction(cx);
    }

    fn finish_interaction(&mut self, cx: &mut Context<Self>) {
        // Writing once per drag, rather than per mouse move, keeps one drag as one undo step.
        if let Some(Interaction::DraggingNode { moved: true, .. }) = self.interaction.take() {
            self.write_document(cx);
        }
        cx.notify();
    }

    fn dragged_node_index(&self) -> Option<usize> {
        match self.interaction {
            Some(Interaction::DraggingNode { node_index, .. }) => Some(node_index),
            _ => None,
        }
    }

    fn edge_geometry(&self, cx: &App) -> Vec<(EdgeGeometry, Option<(SharedString, Point<f32>)>)> {
        let default_color = cx.theme().colors().text_muted;
        self.document
            .edges
            .iter()
            .filter_map(|edge| {
                let from = self
                    .document
                    .nodes
                    .iter()
                    .find(|node| node.id == edge.from_node)?;
                let to = self
                    .document
                    .nodes
                    .iter()
                    .find(|node| node.id == edge.to_node)?;
                let (default_from_side, default_to_side) = default_sides(from, to);
                let from_side = edge.from_side.unwrap_or(default_from_side);
                let to_side = edge.to_side.unwrap_or(default_to_side);
                let points: Vec<Point<f32>> = route_edge(
                    from.anchor(from_side),
                    from_side,
                    to.anchor(to_side),
                    to_side,
                )
                .into_iter()
                .map(|canvas_point| self.canvas_to_local_f32(canvas_point))
                .collect();

                let label = edge.label.as_ref().and_then(|label| {
                    let (start, end) = points
                        .windows(2)
                        .filter_map(|segment| match segment {
                            [start, end] => Some((*start, *end)),
                            _ => None,
                        })
                        .max_by(|(a_start, a_end), (b_start, b_end)| {
                            distance(*a_start, *a_end).total_cmp(&distance(*b_start, *b_end))
                        })?;
                    let midpoint = point((start.x + end.x) / 2., (start.y + end.y) / 2.);
                    Some((SharedString::from(label.clone()), midpoint))
                });

                let geometry = EdgeGeometry {
                    points,
                    color: edge
                        .color
                        .as_deref()
                        .and_then(parse_canvas_color)
                        .unwrap_or(default_color),
                    dashed: edge.style.as_deref() == Some("dashed"),
                    arrow_at_start: edge.from_end == Some(EdgeEnd::Arrow),
                    arrow_at_end: edge.to_end != Some(EdgeEnd::None),
                };
                Some((geometry, label))
            })
            .collect()
    }

    fn image_path(&self, node: &CanvasNode, cx: &App) -> Option<PathBuf> {
        let path = self.file_node_path(node, cx)?;
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        Img::extensions()
            .contains(&extension.as_str())
            .then_some(path)
    }

    /// Resolves a `file` node's path. JSON Canvas paths are relative to the vault
    /// root, which for us is the worktree root.
    fn file_node_path(&self, node: &CanvasNode, cx: &App) -> Option<PathBuf> {
        if node.kind != "file" {
            return None;
        }
        let path = Path::new(node.file.as_deref()?);
        if path.is_absolute() {
            return Some(path.to_path_buf());
        }
        let project = self.project.read(cx);
        let project_path = &self.canvas_item.read(cx).project_path;
        let root = project
            .worktree_for_id(project_path.worktree_id, cx)
            .and_then(|worktree| worktree.read(cx).root_dir())
            .map(|root| root.to_path_buf())
            // A canvas opened on its own has a single-file worktree with no root directory.
            .or_else(|| {
                project
                    .absolute_path(project_path, cx)?
                    .parent()
                    .map(Path::to_path_buf)
            })?;
        Some(root.join(path))
    }

    fn render_node(
        &self,
        node_index: usize,
        node: &CanvasNode,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let zoom = self.zoom_level;
        let origin = self.canvas_to_local(point(node.x, node.y));
        let accent = node.color.as_deref().and_then(parse_canvas_color);
        let is_dragged = self.dragged_node_index() == Some(node_index);
        let border_color = accent.unwrap_or(colors.border);
        let background = if node.is_group() {
            accent.map_or(gpui::transparent_black(), |accent| accent.opacity(0.05))
        } else {
            accent.map_or_else(
                || {
                    colors
                        .editor_background
                        .blend(colors.text.opacity(NODE_SURFACE_TINT))
                },
                |accent| accent.opacity(0.15),
            )
        };
        let font_size = BASE_FONT_SIZE * zoom;
        let image_path = self.image_path(node, cx);
        let is_image = image_path.is_some();
        let text = node
            .display_text()
            .filter(|_| image_path.is_none() && font_size >= MIN_READABLE_FONT_SIZE)
            .map(|text| {
                let (title, details) = text.split_once('\n').unwrap_or((text, ""));
                v_flex()
                    .items_center()
                    .child(SharedString::from(title.to_string()))
                    .when(!details.is_empty(), |this| {
                        this.child(
                            div()
                                .text_color(colors.text_muted)
                                .child(SharedString::from(details.to_string())),
                        )
                    })
            });
        let image = image_path.map(|path| {
            let fallback_label = SharedString::from(node.file.clone().unwrap_or_default());
            img(path)
                .size_full()
                .object_fit(ObjectFit::Contain)
                .with_fallback(move || {
                    Label::new(fallback_label.clone())
                        .color(Color::Muted)
                        .into_any_element()
                })
        });

        div()
            .id(("canvas-node", node_index))
            .absolute()
            .left(origin.x)
            .top(origin.y)
            .w(px(node.width * zoom))
            .h(px(node.height * zoom))
            .flex()
            .when(node.is_group(), |this| this.items_start().justify_start())
            .when(!node.is_group(), |this| {
                this.items_center().justify_center()
            })
            .when(image.is_none(), |this| this.p(px(NODE_PADDING * zoom)))
            .overflow_hidden()
            // Images draw bare, so transparent ones don't show a box behind them.
            .when(!is_image, |this| {
                this.border_1()
                    .border_color(border_color)
                    .bg(background)
                    .when(is_dragged, |this| this.shadow_lg())
            })
            .map(|this| {
                if node.is_circle() {
                    this.rounded_full()
                } else {
                    this.rounded(px(NODE_CORNER_RADIUS * zoom))
                }
            })
            .text_size(px(font_size))
            .text_color(colors.text)
            .text_center()
            .cursor(if is_dragged {
                CursorStyle::ClosedHand
            } else {
                CursorStyle::OpenHand
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event, window, cx| {
                    this.start_node_drag(node_index, event, window, cx)
                }),
            )
            .children(text)
            .children(image)
    }
}

impl EventEmitter<CanvasViewEvent> for CanvasView {}

impl Focusable for CanvasView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CanvasView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.needs_fit {
            match self.container_bounds.get() {
                Some(bounds) => self.fit_to_content_now(bounds),
                // The container's size is only known after the first layout.
                None => cx.on_next_frame(window, |_, _, cx| cx.notify()),
            }
        }

        let zoom = self.zoom_level;
        let (edges, edge_labels): (Vec<_>, Vec<_>) = self.edge_geometry(cx).into_iter().unzip();
        let container_bounds = self.container_bounds.clone();

        let mut node_elements = Vec::with_capacity(self.document.nodes.len());
        // Groups go underneath the nodes they contain, and a dragged node goes above
        // everything so it doesn't slide under its neighbors.
        let dragged_node_index = self.dragged_node_index().filter(|index| {
            self.document
                .nodes
                .get(*index)
                .is_some_and(|node| !node.is_group())
        });
        for draw_groups in [true, false] {
            for (node_index, node) in self.document.nodes.iter().enumerate() {
                if node.is_group() == draw_groups && Some(node_index) != dragged_node_index {
                    node_elements.push(self.render_node(node_index, node, cx).into_any_element());
                }
            }
        }
        if let Some(node_index) = dragged_node_index
            && let Some(node) = self.document.nodes.get(node_index)
        {
            node_elements.push(self.render_node(node_index, node, cx).into_any_element());
        }

        let colors = cx.theme().colors();
        let label_font_size = px(BASE_FONT_SIZE * zoom * 0.9);
        let label_elements = edge_labels
            .into_iter()
            .flatten()
            .filter(|_| f32::from(label_font_size) >= MIN_READABLE_FONT_SIZE)
            .map(|(label, position)| {
                div()
                    .absolute()
                    .left(px(position.x))
                    .top(px(position.y))
                    .size_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .flex_none()
                            .whitespace_nowrap()
                            .px(px(4. * zoom))
                            .bg(colors.editor_background)
                            .text_size(label_font_size)
                            .text_color(colors.text_muted)
                            .child(label),
                    )
            });

        let is_empty = self.document.nodes.is_empty() && self.parse_error.is_none();

        div()
            .id("canvas-view")
            .key_context("CanvasView")
            .track_focus(&self.focus_handle(cx))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::fit_to_content))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::open_as_text))
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(colors.editor_background)
            .cursor(match self.interaction {
                Some(Interaction::Panning { .. }) => CursorStyle::ClosedHand,
                _ => CursorStyle::Arrow,
            })
            .on_scroll_wheel(cx.listener(Self::handle_scroll_wheel))
            .on_pinch(cx.listener(Self::handle_pinch))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::start_panning))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::start_panning))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::handle_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::handle_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::handle_mouse_up))
            .on_mouse_move(cx.listener(Self::handle_mouse_move))
            .child(
                canvas(
                    move |bounds, _, _| {
                        container_bounds.set(Some(bounds));
                    },
                    move |bounds, _, window, _| {
                        for edge in &edges {
                            paint_edge(edge, bounds.origin, zoom, window);
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .children(node_elements)
            .children(label_elements)
            .when(is_empty, |this| {
                this.child(
                    div()
                        .absolute()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(Label::new("Empty canvas").color(Color::Muted)),
                )
            })
            .when_some(self.parse_error.clone(), |this, error| {
                this.child(
                    div()
                        .absolute()
                        .bottom_2()
                        .left_2()
                        .right_2()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(colors.border)
                        .bg(colors.elevated_surface_background)
                        .child(Label::new(error).color(Color::Error).size(LabelSize::Small)),
                )
            })
    }
}

impl Item for CanvasView {
    type Event = CanvasViewEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            CanvasViewEvent::TitleChanged => f(ItemEvent::UpdateTab),
        }
    }

    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        self.buffer
            .read(cx)
            .file()
            .map(|file| SharedString::from(file.file_name(cx).to_string()))
            .unwrap_or_else(|| "untitled.canvas".into())
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        f(self.canvas_item.entity_id(), self.canvas_item.read(cx))
    }

    fn buffer_kind(&self, _: &App) -> ItemBufferKind {
        ItemBufferKind::Singleton
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.buffer.read(cx).is_dirty()
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.buffer.read(cx).has_conflict()
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        self.buffer
            .read(cx)
            .file()
            .is_some_and(|file| file.disk_state().is_deleted())
    }

    fn can_save(&self, _: &App) -> bool {
        true
    }

    fn can_save_as(&self, _: &App) -> bool {
        true
    }

    fn save(
        &mut self,
        _options: SaveOptions,
        project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let buffer = self.buffer.clone();
        project.update(cx, |project, cx| project.save_buffer(buffer, cx))
    }

    fn save_as(
        &mut self,
        project: Entity<Project>,
        path: ProjectPath,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let buffer = self.buffer.clone();
        project.update(cx, |project, cx| project.save_buffer_as(buffer, path, cx))
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let buffers = HashSet::from_iter([self.buffer.clone()]);
        let reload = project.update(cx, |project, cx| project.reload_buffers(buffers, true, cx));
        cx.spawn(async move |_, _| {
            reload.await?;
            Ok(())
        })
    }
}

impl ProjectItem for CanvasView {
    type Item = CanvasItem;

    fn for_project_item(
        project: Entity<Project>,
        _: Option<&Pane>,
        item: Entity<Self::Item>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(item, project, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_unknown_fields_and_writes_integer_coordinates() {
        let input = r#"{
            "nodes": [
                { "id": "a", "type": "text", "x": 10.4, "y": 20, "width": 90, "height": 90,
                  "text": "a", "shape": "circle", "customNodeField": 1 }
            ],
            "edges": [],
            "customDocumentField": { "kept": true }
        }"#;
        let document: CanvasDocument = serde_json::from_str(input).expect("valid canvas");
        let output: serde_json::Value =
            serde_json::to_value(&document).expect("serializable canvas");

        assert_eq!(output["customDocumentField"]["kept"], true);
        assert_eq!(output["nodes"][0]["customNodeField"], 1);
        assert_eq!(output["nodes"][0]["x"], 10);
        assert_eq!(output["nodes"][0]["shape"], "circle");
        assert!(output["nodes"][0].get("file").is_none());
    }

    #[test]
    fn parses_edge_sides_and_ends() {
        let input = r#"{
            "nodes": [],
            "edges": [
                { "id": "e", "fromNode": "a", "fromSide": "bottom", "toNode": "b",
                  "toSide": "top", "toEnd": "none", "style": "dashed" }
            ]
        }"#;
        let document: CanvasDocument = serde_json::from_str(input).expect("valid canvas");
        let edge = document.edges.first().expect("one edge");
        assert_eq!(edge.from_side, Some(Side::Bottom));
        assert_eq!(edge.to_side, Some(Side::Top));
        assert_eq!(edge.to_end, Some(EdgeEnd::None));
        assert_eq!(edge.from_end, None);
    }

    #[test]
    fn routes_aligned_nodes_with_a_straight_line() {
        let route = route_edge(
            point(100., 100.),
            Side::Bottom,
            point(100., 300.),
            Side::Top,
        );
        assert_eq!(route, vec![point(100., 100.), point(100., 300.)]);
    }

    #[test]
    fn routes_offset_nodes_with_an_early_bend() {
        let route = route_edge(
            point(100., 100.),
            Side::Bottom,
            point(300., 300.),
            Side::Top,
        );
        assert_eq!(
            route,
            vec![
                point(100., 100.),
                point(100., 100. + MAX_FIRST_BEND_OFFSET),
                point(300., 100. + MAX_FIRST_BEND_OFFSET),
                point(300., 300.),
            ]
        );
    }
}
