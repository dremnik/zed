# Canvas view

Goal for tonight: open a `.canvas` file in Zed and get an interactive canvas in the main pane, where you can pan, zoom, drag nodes, undo/redo, and save. The test case is the git branch graph (circles for commits, rounded rects for branch labels, solid and dashed elbow arrows).

The canvas is backed by the file's buffer. The buffer provides undo/redo, the dirty indicator, and saving; the canvas only parses JSON out of it and writes JSON back into it.

## 1. Format

Use the open [JSON Canvas](https://jsoncanvas.org/spec/1.0/) format (`.canvas`, as used by Obsidian), plus two optional extension fields:

```json
{
  "nodes": [
    { "id": "d84375b", "type": "text", "shape": "circle", "x": 692, "y": 30, "width": 96, "height": 96, "text": "d84375b" },
    { "id": "pc", "type": "text", "x": 340, "y": 433, "width": 310, "height": 98, "text": "proposal/planning-criteria\nlocal + origin · PR #32 open" }
  ],
  "edges": [
    { "id": "e1", "fromNode": "d84375b", "fromSide": "bottom", "toNode": "00f1fe9", "toSide": "top" },
    { "id": "e2", "fromNode": "d84375b", "toNode": "baaf008", "style": "dashed", "label": "4 commits omitted" }
  ]
}
```

- Spec fields: nodes have `id`, `type` (`text` | `file` | `link` | `group`), `x`/`y` (top-left), `width`/`height`, optional `color`, and `text` / `file` / `url` / `label` by type. Edges have `id`, `fromNode`, `toNode`, optional `fromSide`/`toSide`, `fromEnd`/`toEnd` (`none` | `arrow`, `toEnd` defaults to `arrow`), `color`, `label`.
- Extensions: `"shape": "circle"` on nodes and `"style": "dashed"` on edges. Other tools should ignore them (Obsidian would draw rectangles and solid edges).
- Tonight: render `text` nodes fully; draw `file` / `link` / `group` nodes as labeled boxes. Unknown fields are kept in a `#[serde(flatten)]` map so saving doesn't drop them.
- Positions are explicit, so no layout algorithm is needed.

## 2. Crate skeleton

- `crates/canvas_view/Cargo.toml` with `[lib] path = "src/canvas_view.rs"`.
- Add the crate to the workspace members and `[workspace.dependencies]` in the root `Cargo.toml`, and as a dependency of `crates/zed`.
- `pub fn init(cx)` calls `workspace::register_project_item::<CanvasView>(cx)`, invoked next to `image_viewer::init(cx)` in `crates/zed/src/main.rs`.

## 3. Opening the file

- `CanvasItem` implements `project::ProjectItem`, modeled on `NotebookItem::try_open` in `crates/repl/src/notebook/notebook_ui.rs`: match the extension, open the file as a buffer, and keep the buffer handle.
- `CanvasView` implements `Item`, `ProjectItem`, `Render`, and `Focusable`.
- `CanvasView` subscribes to buffer edits and re-parses each time. That gives live reload: edit the raw JSON in a split and the canvas updates.
- On a parse error, keep the last good document and show the error in a corner.
- Opening a `.canvas` file always opens the canvas, so a `canvas_view: open as text` action splits a plain editor onto the same buffer.

Milestone: opening a `.canvas` file shows a canvas tab.

## 4. Pan and zoom

Port from `crates/image_viewer/src/image_viewer.rs`: `zoom_level`, `pan_offset`, cursor-anchored `set_zoom`, scroll wheel (pan; zoom with ctrl/cmd), pinch, and background drag-to-pan.

```
screen = container_origin + pan_offset + canvas_point * zoom_level
```

## 5. Rendering

All inside one relative container, with edges first so they sit under the nodes.

- Edges: one `canvas(...)` element over the whole view. For each edge, build a `PathBuilder` path from `fromSide` through rounded elbows into `toSide`. Missing sides default from the nodes' relative position (mostly vertical → bottom/top, mostly horizontal → right/left). Vertical routes bend early, at most 28px below the source, for the tree look. `dash_array` for dashed edges, plus a filled triangle arrowhead. Colors come from `cx.theme().colors()`.
- Nodes: an absolutely positioned `div` per node at its transformed bounds (`x`/`y` is the top-left). `rounded_full()` for circles, `rounded_lg()` for rects, theme border/fill, centered label with font size scaled by `zoom_level`.
- Edge labels: centered on the midpoint of the edge's longest segment, on an editor-background chip that masks the line.

Milestone: the branch graph renders and you can pan and zoom it.

## 5b. Dragging nodes

1. Hit-testing: `on_mouse_down` on each node div with `cx.stop_propagation()`. Drags that start on a node move it; drags that start on the background pan.
2. During the drag, only memory changes: `Interaction::DraggingNode` holds the node index, the mouse-down position, and the node's starting position. On each move the node is set to `start + delta / zoom_level`, snapped to a 10px grid, then `cx.notify()`. Edges follow because they're drawn from node positions each frame. No buffer writes per mouse move, so there's no undo step per pixel.
3. On mouse up (or the first mouse move with no button pressed, for releases outside the canvas), the document is serialized with `serde_json::to_string_pretty`, and the whole buffer text is replaced with one `buffer.edit(...)`. One drag is one undo step, and the tab shows as dirty.
4. No echo suppression: GPUI delivers the buffer's `Edited` event after the update finishes, so a flag around the edit would already be cleared. The re-parse of our own write produces the document we already have, so it's harmless.
5. Undo/redo: `CanvasView` handles `editor::actions::Undo` / `Redo` and forwards them to `buffer.undo(cx)` / `buffer.redo(cx)`. The re-parse moves the node back. Those keys are only bound in the `Editor` context, so the default keymaps get a `CanvasView` block (undo/redo, zoom in/out, `cmd-0` / `ctrl-0` fit to content).

Known cost: the first drag rewrites the file in serde's formatting and key order (unknown fields are preserved, formatting is not).

## 6. Test file and polish

- `.plan/branch-graph.canvas`: the git graph from the reference image, using its pixel coordinates (24 nodes, 27 edges).
- `FitToContent` action, from the bounding box of all nodes; also runs on first open.
- Run with `cargo run` and open the file.

## Later

- Selection outline, arrow-key nudge, shift-drag box select.
- Double-click empty space to add a node, drag from a node's edge to create an edge, delete key.
- Inline label editing with a single-line `Editor` overlay (see the zoom editor in `image_viewer.rs`).
- Click a commit node to open it in git.
- Auto-layout, so a script can generate `.canvas` from `git log --graph`.
- Viewport culling for large boards.
