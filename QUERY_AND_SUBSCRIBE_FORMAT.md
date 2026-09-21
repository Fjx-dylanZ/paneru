# Query and Subscribe Format

Paneru exposes structured state over the same IPC channel used by `send-cmd`:
a Mach service named `com.github.karinushka.paneru`. The CLI commands below
require a running Paneru daemon.

**The JSON below is what the CLI prints, not what crosses between processes.**
Requests and responses travel as typed values in a compact binary encoding
(`postcard`); `paneru query` and `paneru subscribe` render them as JSON because
a terminal — and `jq`, and a status bar's shell script — needs text. Anything
consuming these commands' output sees exactly the shapes documented here.

A client written in Rust can skip the JSON entirely by using the
`paneru-shared-types` crate: its `wire::Request` and `wire::Response` are the
protocol, and `async-mach-ports` is the transport.

All query responses are a single JSON document. `subscribe` emits
line-delimited JSON, with one complete event object per line.

## Query Commands

```shell
paneru query state --json
paneru query virtual-workspaces --json
paneru query active --json
paneru query native-spaces --json
```

`--json` is accepted for clarity and is the only output format, so it may be
omitted; callers should include it anyway, in case another format is ever added.

### `paneru query state --json`

Returns the complete state document.

```json
{
  "version": 1,
  "timestamp": 1777740000,
  "active": {
    "display_id": 1,
    "native_workspace_id": 4,
    "virtual_workspace_number": 3,
    "focused_window_id": 321,
    "focused_bundle_id": "com.apple.Terminal",
    "focused_app_name": "Terminal",
    "focused_window_title": "paneru"
  },
  "virtual_workspaces": [
    {
      "number": 1,
      "native_workspace_id": 4,
      "active": false,
      "windows": []
    },
    {
      "number": 2,
      "native_workspace_id": 4,
      "active": false,
      "windows": []
    },
    {
      "number": 3,
      "native_workspace_id": 4,
      "active": true,
      "windows": [
        {
          "window_id": 321,
          "bundle_id": "com.apple.Terminal",
          "app_name": "Terminal",
          "title": "paneru",
          "focused": true,
          "floating": false
        }
      ]
    }
  ]
}
```

### `paneru query virtual-workspaces --json`

Returns only the `virtual_workspaces` array from the complete state document.

```json
[
  {
    "number": 1,
    "native_workspace_id": 4,
    "active": false,
    "windows": []
  },
  {
    "number": 2,
    "native_workspace_id": 4,
    "active": false,
    "windows": []
  },
  {
    "number": 3,
    "native_workspace_id": 4,
    "active": true,
    "windows": [
      {
        "window_id": 321,
        "bundle_id": "com.apple.Terminal",
        "app_name": "Terminal",
        "title": "paneru",
        "focused": true,
        "floating": false
      }
    ]
  }
]
```

### `paneru query active --json`

Returns only the active display, workspace, and focused-window state.

```json
{
  "display_id": 1,
  "native_workspace_id": 4,
  "virtual_workspace_number": 3,
  "focused_window_id": 321,
  "focused_bundle_id": "com.apple.Terminal",
  "focused_app_name": "Terminal",
  "focused_window_title": "paneru"
}
```

### `paneru query native-spaces --json`

Returns a fresh census of every native macOS Space, read from the OS at query
time. It is a separate request (`wire::Request::NativeSpaces`, answered with
`QueryPayload::NativeSpaces`), not a slice of the state document above: the
state document is Paneru's virtual layout, while this census is the window
server's own list, independent of Paneru's layout and of whatever
reconciliation is in flight, so it always lists empty and fullscreen Spaces
whether or not Paneru holds a row for them. The `state`,
`virtual-workspaces` and `active` shapes are unchanged by it. The embedded Lua
`paneru.query(kind)` / `paneru.query_*` functions serve the state document
only; this census is not available through them.

```json
[
  {
    "id": 4,
    "index": 1,
    "display": "display-a",
    "display_index": 1,
    "type": 0,
    "active": true
  },
  {
    "id": 4294967303,
    "index": 2,
    "display": "display-a",
    "display_index": 2,
    "type": 0,
    "active": false
  },
  {
    "id": 4294967310,
    "index": 3,
    "display": "display-b",
    "display_index": 1,
    "type": 0,
    "active": true
  }
]
```

| Field | Type | Description |
| :--- | :--- | :--- |
| `id` | number | Native Space id, the same value `native_workspace_id` carries in the state document. Stable for the Space's lifetime; not a selector. |
| `index` | number | One-based position in global Mission Control order across all displays. This is the number `space focus <n>`, `space destroy <n>`, `window spacemove <n>` and `window spacesend <n>` select by. It shifts whenever a Space is created, destroyed or reordered. |
| `display` | string | Opaque identifier of the display that owns the Space (shown here as `display-a`/`display-b` for illustration; the real value is whatever the window server reports). Only useful for grouping and equality; it is not the numeric CoreGraphics `display_id`. |
| `display_index` | number | One-based position in the owning display's native Space list, non-Desktop entries (fullscreen, system) included. Not necessarily the Desktop label Mission Control shows on that display. Shifts like `index`. |
| `type` | number | Native Space type as reported by macOS. `0` is an ordinary Desktop; only Desktops can be destroyed or receive moved windows. Other values are fullscreen or system Spaces. |
| `active` | boolean | Whether this Space is the one currently shown on **its own** display. With several displays, one entry per display is `true`. |

The array is ordered by `index`. Because native requests complete
asynchronously (see the
[native Spaces section](./CONFIGURATION.md#native-macos-spaces-experimental)
of the Configuration Guide), a census read immediately after
`send-cmd space create` or `space destroy` may still show the previous state
or fail transiently while macOS converges; query again once it has settled.

## Fields

| Field | Type | Description |
| :--- | :--- | :--- |
| `version` | number | State document format version. Currently `1`. |
| `timestamp` | number | Unix timestamp in seconds when the response was built. |
| `active` | object | Current active display/native workspace/virtual workspace/focused window. |
| `display_id` | number or null | CoreGraphics display id for the active display, when known. |
| `native_workspace_id` | number or null | macOS Space id for the active native workspace, when known. |
| `virtual_workspace_number` | number or null | One-based Paneru virtual workspace number, when known. |
| `focused_window_id` | number or null | Focused window id, when known. |
| `focused_bundle_id` | string or null | Bundle id of the focused window's app, when known. |
| `focused_app_name` | string or null | Display name of the focused window's app, when known. |
| `focused_window_title` | string or null | Title of the focused window, when known. |
| `virtual_workspaces` | array | Virtual workspace rows known to Paneru. |
| `number` | number | One-based virtual workspace number. |
| `active` | boolean | Whether this virtual workspace is currently selected. |
| `windows` | array | Managed windows in this virtual workspace row. |
| `window_id` | number | Window id. |
| `bundle_id` | string | Bundle id for the owning application, or an empty string if unknown. |
| `app_name` | string | Display name for the owning application, or an empty string if unknown. |
| `title` | string | Window title, or an empty string if unknown. |
| `focused` | boolean | Whether this window is focused. |
| `floating` | boolean | Persistent floating mode; remains true while a floating window is minimized or its application is hidden. |

Paneru may include empty `windows` arrays for missing virtual workspace numbers
inside a native workspace so integrations can render stable numbered slots.

Minimized and application-hidden windows are not reported visible or focused.
Unhiding an application does not imply that its minimized windows were restored.
Inactive members of an identified native tab group remain in the logical row,
but are also reported invisible and unfocused, even when macOS still lists
their backing windows as ordered in.

A confirmed-destroyed native Space can retain a last-known virtual row while
Paneru cannot establish where one of its windows went. Queries preserve that
row and its window metadata, but report it inactive and its windows invisible
and unfocused. This does not mean the native Space still exists; use the native
census for topology. A native enumeration failure for a present Space remains
an error.

## Subscribe Command

```shell
paneru subscribe --json
```

`subscribe` keeps its channel open and writes one JSON event per line. The stream
is intended for integrations such as SketchyBar, so it emits changes that are
useful for keeping a bar in sync: focus changes, native or virtual workspace
changes, managed window-list changes, window title changes, and display changes.
Paneru coalesces duplicate internal events from the same ECS tick and skips
events whose relevant state has not changed since the last emitted event.
Consumers should parse each line independently and then call
`paneru query state --json` when they need a full refresh.

### Event Types

```json
{"event":"virtual_workspace_changed","active":{"display_id":1,"native_workspace_id":4,"virtual_workspace_number":3,"focused_window_id":321,"focused_bundle_id":"com.apple.Terminal","focused_app_name":"Terminal","focused_window_title":"paneru"}}
```

Emitted after native Space changes and Paneru virtual workspace switches. Paneru
derives this from both incoming workspace events and ECS active-workspace marker
changes, so integrations receive the event when the visible workspace state
changes.

```json
{"event":"windows_changed","virtual_workspace_number":3,"active":{"display_id":1,"native_workspace_id":4,"virtual_workspace_number":3,"focused_window_id":321,"focused_bundle_id":"com.apple.Terminal","focused_app_name":"Terminal","focused_window_title":"paneru"}}
```

Emitted after managed window creation/destruction/minimize/deminimize events and
after Paneru moves or sends a window between virtual workspaces. The event is
emitted only when Paneru's virtual workspace/window state differs from the last
emitted `windows_changed` event.

```json
{"event":"window_focused","window_id":321,"bundle_id":"com.apple.Terminal","title":"paneru","virtual_workspace_number":3}
```

Emitted when focus changes. Paneru derives this from both incoming focus events
and ECS focused-window marker changes, so internally handled focus transitions
are visible to subscribers. The `window_id`, `bundle_id`, `title`, and
`virtual_workspace_number` fields are taken from the final active state for the
tick, so stale lower-level focus notifications are not forwarded with mismatched
window metadata.

```json
{"event":"window_title_changed","window_id":321,"title":"paneru"}
```

Emitted when a window title changes.

```json
{"event":"display_changed","display_id":1}
```

Emitted when display configuration changes. `display_id` can be `null` when the
event is a global display-change notification and Paneru cannot resolve an
active display id.

## Virtual Workspace Commands

Absolute virtual workspace selection is addressed as a window command:

```shell
paneru send-cmd window virtualnum 3
paneru send-cmd window virtualmovenum 3
paneru send-cmd window virtualsendnum 3
```

The matching config binding names are:

```toml
[bindings]
window_virtualnum_3 = "cmd + alt - 3"
window_virtualmovenum_3 = "cmd + alt + ctrl - 3"
window_virtualsendnum_3 = "cmd + alt + shift - 3"
```
