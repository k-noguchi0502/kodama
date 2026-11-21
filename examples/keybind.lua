-- kodama key bindings: copy to ~/.config/kodama/keybind.lua
--
-- Read from the same directory as config.lua (which is optional). Like the
-- config it is plain Lua, re-read when saved, and errors show up in the
-- terminal. Return a list of bindings; they take precedence over `keys` in
-- config.lua, which take precedence over the built-in bindings.
--
-- Each binding:
--   key:    a character ("c", "=", "[") or a key name ("Enter", "PageUp",
--           "F5", "Up", "Tab", "Space", "Escape", ...)
--   mods:   "ctrl|shift|alt|super" in any combination (optional). "super"
--           is the modifier chosen with `super` below (alt here);
--           "logo" (or "win") is the physical Super/Windows key.
--   action: a built-in name, kodama.send(text), or a function(win)
--
-- Built-in actions: copy, paste, paste_primary, copy_or_pass, paste_or_pass,
-- copy_last_command_output, show_scrollback (opens it in scrollback_pager),
-- increase_font_size, decrease_font_size, reset_font_size, scroll_page_up,
-- scroll_page_down, scroll_line_up, scroll_line_down, scroll_to_top,
-- scroll_to_bottom, clear_scrollback, reload_config, new_pane (split the
-- focused pane along its longer side, like Hyprland's dwindle layout),
-- split_right, split_down, close_pane, focus_pane_left/right/up/down,
-- move_pane_left/right/up/down (swap with the neighbour), focus_next_pane,
-- focus_prev_pane, resize_pane_left/right/up/down, toggle_pane_zoom,
-- toggle_broadcast (typing goes to every pane of the tab),
-- toggle_file_tree, search, scroll_to_prev_prompt, scroll_to_next_prompt,
-- new_tab, next_tab, prev_tab, close_tab, go_to_tab_1 ... go_to_tab_9,
-- move_tab_left, move_tab_right, rename_tab,
-- copy_mode, quick_select, command_palette, toggle_fullscreen, new_window,
-- and none (passes the key to the program, unbinding a default).
--
-- Defaults (Hyprland-style panes, with super = alt unless changed):
--   super+enter         new pane
--   super+q             close the focused pane
--   super+arrows        move focus to the pane in that direction
--   super+shift+arrows  move (swap) the focused pane in that direction
--   super+ctrl+arrows   move the divider (resize). super is alt, so
--                       super+alt would be the same chord as focus.
--   super+shift+b       file tree: show and focus it, hide it when focused.
--                       super+left from the leftmost pane also focuses it.
--                       In the tree: j/k or arrows move, l/enter open (enter on
--                       a file runs $EDITOR in the pane), h closes the
--                       folder, space toggles, g/G top/bottom, "." dotfiles,
--                       r reload, c cd the shell there, i type the path,
--                       y copy the path, esc/q back to the pane.
--                       / filter by name (enter keeps the result, esc clears),
--                       a new file (end in / for a folder), R rename, m move,
--                       C copy, d move to trash, D delete permanently.
--                       Git changes are coloured with M/A/?/D/R/U marks.
--                       While the tree is focused, super+ctrl+left/right
--                       resizes it (5% of the window per press; 8 columns
--                       up to half the window). The same keys resize panes
--                       when a pane is focused.
--
--   alt+shift+f  search the history: type to search, enter/up older match,
--                 shift+enter/down newer, esc closes
--   alt+click     open a link (URLs in the text and OSC 8 hyperlinks;
--                 holding alt underlines them)
--   alt+shift+pageup/pagedown   previous / next shell prompt (needs the
--                 shell integration: examples/shell-integration.zsh/.bash)
--   alt+shift+t  new tab         alt+pageup/pagedown  previous / next tab
--   alt+1 ... alt+9  go to tab n  (closing a tab's last pane closes the tab;
--                 click a tab to switch, middle-click to close)
--   alt+shift+p  command palette: type to filter every action and color
--                 scheme, up/down (alt+p/n) pick, enter runs, esc closes
--   alt+shift+x  copy mode: move with vi keys (h/j/k/l, w/b/e, 0/^/$,
--                 gg/G, H/M/L, alt+u/d, alt+b/f), v select, V select lines,
--                 y (or enter) copy and leave (the line if nothing is
--                 selected), / search (esc continues from the match),
--                 esc/q leave
--   alt+shift+space  quick select: URLs, paths, git hashes, IPs and long
--                 numbers on screen get labels; type one to copy it, with
--                 shift to paste it too; esc cancels
--
--   alt+c         copy the selection; with nothing selected the key goes to
--                 the program. Ctrl+C is not bound, so it still interrupts.
--   alt+v         paste at the shell; full-screen programs (vim, nano) get
--                 the key. Bind "paste" to always paste.
--   alt+shift+c / alt+shift+v   always copy / paste
--   shift+insert  paste the primary selection
--   alt+= / alt+- / alt+0      font size bigger / smaller / reset
--   shift+pageup/pagedown, shift+home/end   scroll the history
--   alt+shift+e / alt+shift+o   split right / down
--   alt+shift+w  close pane      alt+shift+z  zoom the focused pane
--   alt+arrows    move focus
--   alt+tab / alt+shift+tab     next / previous pane
--   alt+shift+r  reload the config
--
-- A function receives a handle for the focused pane with fields cols, rows,
-- font_size, title, selection (nil when nothing is selected), alt_screen,
-- pane_count, and methods win:send(text), win:action(name), win:copy(),
-- win:paste(), win:set_font_size(points), win:scroll(lines; positive scrolls
-- back).
--
-- To drop every built-in binding, return
--   { disable_default_keys = true, keys = { ... } }
-- instead of a plain list.
--
-- `super` picks the modifier(s) "super" stands for: "alt" (as set below), "ctrl",
-- "ctrl|alt", "logo" for the real Super key (only if the desktop doesn't
-- take it, which Hyprland usually does), ... It can also be set in
-- config.lua.

return {
  super = "alt",

  { key = "q", mods = "alt|shift", action = "close_tab" },

  { key = "Enter", mods = "shift", action = kodama.send("\x1b\r") },
  { key = "k", mods = "alt|shift", action = "clear_scrollback" },
  { key = "Up", mods = "shift", action = "scroll_line_up" },
  { key = "Down", mods = "shift", action = "scroll_line_down" },

  -- tmux-style splits; the built-in alt+shift+e / alt+shift+o keep working.
  -- alt+- is the font size, so these use shift as well.
  { key = "\\", mods = "alt|shift", action = "split_right" },
  { key = "-", mods = "alt|shift", action = "split_down" },

  -- Split along the longer side; zoom instead once there are four panes.
  {
    key = "Enter",
    mods = "alt|shift",
    action = function(win)
      if win.pane_count >= 4 then
        win:action("toggle_pane_zoom")
      elseif win.cols > 2 * win.rows then
        win:action("split_right")
      else
        win:action("split_down")
      end
    end,
  },

  -- Presentation mode: toggle between the normal size and a large one.
  {
    key = "m",
    mods = "alt|shift",
    action = function(win)
      win:set_font_size(win.font_size < 20 and 24 or 12)
    end,
  },

  -- Search the selection with ripgrep in the current shell.
  {
    key = "g",
    mods = "alt|shift",
    action = function(win)
      if win.selection and not win.alt_screen then
        win:send(string.format("rg -F %q\r", win.selection))
      end
    end,
  },
}
