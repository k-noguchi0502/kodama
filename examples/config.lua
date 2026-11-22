-- kodama configuration: copy to ~/.config/kodama/config.lua
--
-- Plain Lua 5.4. The file must return a table; anything left out keeps its
-- default. Saving it reloads the config (alt+shift+r does the same), and
-- errors are printed in the terminal.
--
-- `kodama` is provided while this file loads:
--   kodama.version, kodama.os, kodama.home, kodama.config_dir
--   kodama.hostname()     -> string
--   kodama.log(...)       -> logs when RUST_LOG=info
--   kodama.send(text)     -> key action that writes text to the shell
--   kodama.color_schemes  -> names of the built-in color schemes
-- require("foo") loads foo.lua from the config directory.
--
-- Colors live in theme.lua. Key bindings live in keybind.lua.

local is_laptop = kodama.hostname():match("laptop") ~= nil

return {
  font = {
    family = "monospace", -- any fontconfig pattern, e.g. "JetBrains Mono"
    size = is_laptop and 11 or 12, -- points
    ligatures = true,
    -- Tried in order when the font lacks a character (CJK, symbols, ...),
    -- before fontconfig chooses one. Emoji, including sequences, come from
    -- the system emoji font.
    -- fallback = { "Noto Sans CJK JP", "Symbols Nerd Font" },
  },

  window = {
    -- Logical pixels between the window edge and the content, on every side.
    -- Kept equal to split.padding so the tree and each pane have the same
    -- margin on the window and beside a divider. 0 meets the edge.
    -- padding_top, padding_right, padding_bottom and padding_left each
    -- replace one side; an omitted side keeps `padding`.
    padding = 6,
    columns = 100, -- initial size; floating window managers honour it
    rows = 30,
    opacity = 1.0, -- 0 to 1; text stays opaque
    -- A PNG behind the text (~/ or relative to this file). Its opacity is
    -- how strongly it shows through the background color; fit is "cover"
    -- (fill, cropping), "contain" (whole image) or "stretch".
    -- background_image = "~/Pictures/wallpaper.png",
    background_image_opacity = 0.25,
    background_image_fit = "cover",
  },

  -- The line between panes. Separate from the window margin and from the
  -- padding inside each pane.
  split = {
    padding = 6, -- space on each side of the line
    width = 1, -- line thickness; 0 hides it. Colour is colors.split
    -- "border", "background", "both" or "none".
    -- border uses colors.active_border, background uses colors.active_background.
    highlight = "background",
    -- true paints a background highlight through the padding, out to the
    -- window edge and up to the divider, for the focused pane and the
    -- focused tree. The cells stay where they are.
    highlight_ignores_padding = true,
    -- Border thickness in logical pixels, for "border" and "both".
    -- Only this stroke is drawn; 0 hides it. With highlight_ignores_padding
    -- the stroke sits on the outer edge and the fill stays inside it.
    active_border = 2,
  },

  -- Extra inset inside the margin above, for every pane. 0 keeps the
  -- grid on that margin. The same four side keys as window replace one
  -- side. active_padding is added around the focused pane only.
  pane = {
    padding = 0,
    active_padding = 0,
  },

  -- File tree of the focused pane's directory (super+shift+b).
  tree = {
    width = 30, -- columns; drag the right edge to resize
    show_hidden = false, -- "." toggles this while the tree has focus
    -- Extra inset inside the sidebar. 0 keeps the grid on the same margin
    -- as the panes. The same four side keys as window replace one side.
    padding = 0,
    -- Space on each side of the divider between the tree and the panes.
    -- Omit gap to use split.padding.
    -- gap = 6,
  },

  -- Tabs across the top of the panes (the file tree keeps the full
  -- height). Colours are colors.tab_bar, tab_active and tab_inactive in
  -- theme.lua.
  tab_bar = {
    -- "plain" (rectangles), "powerline" (arrows), "slant" or "round" (pills).
    style = "plain",
    -- "auto" shows it while there is more than one tab; "always" or "never".
    show = "auto",
    -- Widest a tab gets, in columns. 0 shares the whole bar between the
    -- tabs, side by side.
    max_width = 0,
    -- What a tab is labelled with until it is renamed: "directory" (the name
    -- of its working directory, ~ at home) or "title" (the title its program
    -- sets, e.g. user@host:~/path).
    title = "directory",
  },

  cursor = {
    style = "block", -- "block", "bar" or "underline"
  },

  -- What a bell (BEL, e.g. `printf '\a'`) does. A bell in another tab puts
  -- a bell mark on that tab until you switch to it.
  bell = {
    visual = true, -- flash the pane
    urgent = true, -- ask the window manager for attention when unfocused
    -- command = "paplay /usr/share/sounds/freedesktop/stereo/bell.oga",
  },

  -- Desktop notifications that programs send with OSC 9 or OSC 777
  -- (shown with notify-send): "always", "unfocused" (only when the window
  -- or the pane isn't focused) or "never".
  notifications = "always",

  scrollback = 10000,
  -- What show_scrollback (alt+shift+h) opens the history with, in a new
  -- tab; the file name is appended. Quitting it closes the tab.
  scrollback_pager = "less +G",

  -- Drop blank lines printed while the screen is still empty, so a prompt
  -- that starts with a newline begins on the top row.
  trim_leading_blank_lines = true,

  -- Ask before closing the window or its last pane (y or Enter closes,
  -- anything else cancels; closing the window twice also closes).
  confirm_close = true,

  -- Each of these is on unless set to false.
  features = {
    restore_layout = true, -- save tabs, splits, directories, pane text and the open tree
    file_tree = true, -- the sidebar; also restored when it was open
    tree_open_split = true, -- s in the file tree opens the selection in a new pane
    quick_select = true, -- label URLs and paths on screen
    quick_select_open = true, -- alt+label opens a URL, file or directory
    copy_mode = true, -- vi keys over the scrollback
    search = true, -- search the scrollback
    command_palette = true, -- the list of actions, schemes and languages
    drag_reorder = true, -- drag tabs to reorder; alt-drag a pane onto another to swap
    notification_focus = true, -- clicking a notification focuses that pane
  },

  -- Language of kodama's own text. Built in: en, ja. Only that file is loaded.
  -- ~/.config/kodama/lang/<id>.lua overrides a built-in file of the same id.
  -- The command palette can change this and writes the choice back here.
  language = "en",

  shell = {
    -- program = "/usr/bin/fish", -- default: $SHELL
    -- args = { "--login" },
    -- working_directory = "~/Projects",
  },

  env = {
    -- EDITOR = "nvim",
  },

  -- Extra bindings, applied after keybind.lua and before the built-ins.
  -- keys = {
  --   { key = "Enter", mods = "shift", action = kodama.send("\x1b\r") },
  -- },

  -- true drops the built-in bindings and keeps only your own.
  disable_default_keys = false,
}
