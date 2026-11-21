# kodama shell integration for zsh: marks prompts (OSC 133) so
# alt+shift+pageup/pagedown jump between them and alt+shift+y copies the
# last command's output, and reports the working directory (OSC 7) for new
# panes, tabs and the file tree.
#
#   source ~/.config/kodama/shell-integration.zsh   # in ~/.zshrc

[[ $TERM_PROGRAM == kodama ]] || return 0

_kodama_precmd() {
  local ret=$?
  local dir=${PWD//\%/%25}
  printf '\e]133;D;%s\e\\\e]7;file://%s%s\e\\\e]133;A\e\\' "$ret" "$HOST" "${dir// /%20}"
}

_kodama_preexec() {
  printf '\e]133;C\e\\'
}

autoload -Uz add-zsh-hook
add-zsh-hook precmd _kodama_precmd
add-zsh-hook preexec _kodama_preexec
