# Written by Appine into new systems, and replaced by Appine's later versions of it as long as it is
# left unchanged: a copy you edit is kept as it is.
#
# Shows the motd, sets the terminal and language, and gives bash and busybox ash a coloured prompt
# with shell integration: OSC 133 marks telling Appine's terminal where each prompt, the command
# typed at it and the command's output are. With them a tab waiting at its prompt closes without
# asking, Ctrl+Shift+Up and Down scroll to the previous and the next prompt, a long press in a
# command's output offers to select all of it, and a tap on the command being typed moves the
# cursor there (cl=line: the terminal sends left and right arrows). A prompt set later (~/.profile
# for ash) goes without the marks; bash puts them back around whatever PS1 is before each prompt.
cat /etc/motd 2>/dev/null
export TERM=xterm-256color
export LANG=C.UTF-8
export NMAP_UNPRIVILEGED=1
if [ -n "${BASH_VERSION-}" ]; then
    PS1='\[\e[1;32m\]\u@\h\[\e[0m\]:\[\e[1;34m\]\w\[\e[0m\]\$ '
    # Before each prompt: the end of the last command with its status (D), then the prompt's start
    # on a fresh line (A; readline redraws only the prompt's last line, and moves the cursor with
    # the arrows); the marks around PS1 (P, B: the input starts) and PS2, and the start of the
    # output (C, from PS0), are added again whenever something replaced them
    __appine_prompt() {
        local status=$?
        printf '\033]133;D;%s\007\033]133;A;redraw=last;cl=line\007' "$status"
        case $PS1 in *'133;B'*) ;; *) PS1='\[\e]133;P;k=i\a\]'$PS1'\[\e]133;B\a\]' ;; esac
        case ${PS2-} in *'133;B'*) ;; *) PS2='\[\e]133;P;k=s\a\]'${PS2-}'\[\e]133;B\a\]' ;; esac
        case ${PS0-} in *'133;C'*) ;; *) PS0=${PS0-}'\e]133;C\a' ;; esac
        return "$status"
    }
    case ${PROMPT_COMMAND-} in
    *__appine_prompt*) ;;
    *) PROMPT_COMMAND="__appine_prompt${PROMPT_COMMAND:+;$PROMPT_COMMAND}" ;;
    esac
elif [ -n "${BB_ASH_VERSION-}" ]; then
    # ash runs nothing before a command, so the prompt itself reports the last command's status
    # (D) and starts on a fresh line (A, its cursor moved with the arrows), and the typed command
    # ends with its line (I); a command longer than the screen's width is therefore partly taken
    # for output, as ash breaks its lines
    PS1='\[\e]133;D;$?\a\e]133;A;cl=line\a\]\[\e[1;32m\]\u@\h\[\e[0m\]:\[\e[1;34m\]\w\[\e[0m\]\$ \[\e]133;I\a\]'
    PS2='\[\e]133;P;k=s\a\]> \[\e]133;I\a\]'
fi
