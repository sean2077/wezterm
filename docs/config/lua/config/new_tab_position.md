---
tags:
  - tab
---

# `new_tab_position = "End"`

Controls where a tab that is spawned into an existing window is placed in
that window's tab bar.

* `"End"` (the default): append the new tab at the end of the tab bar.
* `"AfterCurrent"`: insert the new tab immediately after the tab that is
  active when the tab is spawned.

This applies to tabs spawned by the GUI (the new tab button, `SpawnTab`,
`SpawnCommandInNewTab` and friends) and by `wezterm cli spawn`. Tabs
restored by `wezterm cli restore-workspace` keep their saved order.