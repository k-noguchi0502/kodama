# kodama shell integration for bash: marks prompts (OSC 133) so
# alt+shift+pageup/pagedown jump between them and alt+shift+y copies the
# last command's output, and reports the working directory (OSC 7) for new
# panes, tabs and the file tree.
#
#   source ~/.config/kodama/shell-integration.bash   # in ~/.bashrc

[[ $TERM_PROGRAM == kodama ]] || return 0

_kodama_prompt() {
  local ret=$?
  local dir=${PWD//\%/%25}
  printf '\e]133;D;%s\e\\\e]7;file://%s%s\e\\\e]133;A\e\\' "$ret" "$HOSTNAME" "${dir// /%20}"
  return $ret
}

PROMPT_COMMAND="_kodama_prompt${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
PS0='\e]133;C\e\\'"$PS0"
