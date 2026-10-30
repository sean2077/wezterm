---
tags:
  - quake
---
# `quake`

*This is a self-use, Windows-only fork feature; it is not available upstream.*

When `quake.enabled = true`, the dedicated WezTerm instance starts hidden,
registers `quake.hotkey` as a global hotkey, and toggles between hidden and a
work-area anchored rectangle. A window close request hides the window instead
of exiting; use `QuitApplication` to exit the process.

```lua
config.quake = {
  enabled = true,
  hotkey = { key = 'F3', mods = 'NONE' },
  start_hidden = true,
  hide_on_focus_loss = true,
  always_on_top = true,
  edge = 'Top',
  alignment = 'Center',
  width_percent = 100,
  height_percent = 45,
  monitor = 'Cursor',
}
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `enabled` | boolean | `false` | Enable quake mode for this window |
| `hotkey` | key/mods table | `nil` | Global hotkey; required when enabled |
| `start_hidden` | boolean | `false` | Skip the initial `ShowWindow`; wait for the hotkey |
| `hide_on_focus_loss` | boolean | `false` | Hide when the foreground window leaves this window's root-owner family |
| `always_on_top` | boolean | `true` | Keep the shown window above other windows |
| `edge` | `Top`, `Bottom` | `Top` | Work-area edge to anchor to |
| `alignment` | `Start`, `Center`, `End` | `Center` | Alignment along that edge |
| `width_percent` | 10–100 | `100` | Width as a percentage of the monitor work area |
| `height_percent` | 10–100 | `45` | Height as a percentage of the monitor work area |
| `monitor` | `Cursor`, `Foreground`, `Primary` | `Cursor` | Monitor used when showing the window |

Notes:

- The hotkey is registered against the window handle via `RegisterHotKey`; it
  is global and exclusive. If registration fails the window is shown instead of
  staying hidden.
- Intended for a dedicated instance, for example:
  `wezterm-gui.exe --config-file path/to/quake.lua start --class org.wezfurlong.wezterm.quake`.
- Use `window_decorations = 'NONE'` so the outer and client rectangles
  coincide and `tab_bar_at_bottom = true` for tabs at the bottom.
- Non-character keys other than F1-F24 (for example arrows or media keys) are
  not mapped for the global hotkey; use a function key or an alphanumeric key.
- The window is created with `WS_EX_TOOLWINDOW`, so it does not appear in the
  taskbar or Alt+Tab.