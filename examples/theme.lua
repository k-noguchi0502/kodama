-- kodama colors: copy to ~/.config/kodama/theme.lua
--
-- Colors live here, apart from config.lua (which may still set
-- `color_scheme` and `colors`; this file's go on top). It is plain Lua like
-- config.lua and reloads when saved. Picking a scheme in the command
-- palette (alt+shift+p) switches to it and writes its name into the
-- `color_scheme` line below, leaving the rest of this file as it is.
--
-- Return a table like the one below, or just a scheme name:
--   return "Kanagawa"

return {
  -- A built-in scheme, or one from `schemes` below. Names ignore case,
  -- spaces and dashes ("tokyonight", "rose-pine-moon"); kodama.color_schemes
  -- lists the built-in ones:
  --   Tokyo Night, Tokyo Night Storm, Tokyo Night Moon, Tokyo Night Day,
  --   Gruvbox Dark, Gruvbox Light, Gruvbox Dark Hard, Gruvbox Light Hard,
  --   Gruvbox Material Dark, Catppuccin Mocha, Catppuccin Macchiato,
  --   Catppuccin Frappe, Catppuccin Latte, Dracula, Nord, One Dark, One Light,
  --   Solarized Dark, Solarized Light, Everforest Dark, Everforest Dark Hard,
  --   Everforest Light, Kanagawa, Kanagawa Dragon, Kanagawa Lotus, Nightfox,
  --   Carbonfox, Ayu Dark, Ayu Mirage, Ayu Light, Night Owl, Iceberg Dark,
  --   Iceberg Light, Melange Dark, Sonokai, Moonfly, Poimandres, Zenburn,
  --   Flexoki Dark, Flexoki Light, Rosé Pine, Rosé Pine Moon, Rosé Pine Dawn,
  --   Monokai, Monokai Pro, GitHub Dark, GitHub Dark Dimmed, GitHub Light
  color_scheme = "Tokyo Night",

  -- Changes on top of the scheme; any of these can be given:
  --   foreground, background, cursor, selection
  --   split              divider between panes
  --   active_border      frame around the focused pane     (split.highlight
  --   active_background  focused pane's background          in config.lua)
  --   search_match       search matches, and the current one
  --   search_current
  --   tab_bar            tab bar, search and mode bars
  --   tab_active         current tab
  --   tab_inactive       other tabs
  --   ansi, brights      lists of 8 colors (ANSI 0-7 and 8-15)
  colors = {
    -- active_border = "#ff9e64",
  },

  -- Your own schemes, selectable by name above and in the palette.
  -- foreground, background, ansi and brights are required; the other colors
  -- are derived from them unless given.
  schemes = {
    ["Gruvbox Amber"] = {
      foreground = "#ebdbb2",
      background = "#282828",
      selection = "#504945",
      split = "#504945",
      active_border = "#fabd2f",
      active_background = "#3c3836",
      search_current = "#fe8019",
      tab_bar = "#1d2021",
      tab_active = "#fabd2f",
      tab_inactive = "#3c3836",
      ansi = { "#282828", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a", "#a89984" },
      brights = { "#928374", "#fb4934", "#b8bb26", "#fabd2f", "#83a598", "#d3869b", "#8ec07c", "#ebdbb2" },
    },
  },
}
