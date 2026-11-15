# Written by Appine beside /etc/profile.d/appine.sh, which reads this file, and replaced together
# with it.
#
# bash's shell integration marks (OSC 133, see appine.sh). A login bash reads this file through the
# profile, and on Alpine every interactive bash also through /etc/bash/appine.sh, which Alpine's
# /etc/bash/bashrc reads, so a nested bash (bash, su without -l) is marked too, with its own
# prompt. Reading it again changes nothing.
if [ -n "${BASH_VERSION-}" ]; then
    # Before each prompt: the end of the last command with its status (D), then the prompt's start
    # on a fresh line (A; readline redraws only the prompt's last line, and moves the cursor with
    # the arrows); the marks around PS1 (P, B: the input starts) and PS2, and the start of the
    # output (C, from PS0), are added again whenever something replaced them, so a PS1 that
    # ~/.bashrc or anything later sets is marked from the next prompt on
    __appine_prompt() {
        local status=$?
        printf '\033]133;D;%s\007\033]133;A;redraw=last;cl=line\007' "$status"
        # Restore the directory after a program's title, without letting path controls end OSC.
        printf '\033]2;%s\007' "${PWD//[[:cntrl:]]/?}"
        case $PS1 in *'133;B'*) ;; *) PS1='\[\e]133;P;k=i\a\]'$PS1'\[\e]133;B\a\]' ;; esac
        case ${PS2-} in *'133;B'*) ;; *) PS2='\[\e]133;P;k=s\a\]'${PS2-}'\[\e]133;B\a\]' ;; esac
        case ${PS0-} in *'133;C'*) ;; *) PS0=${PS0-}'\e]133;C\a' ;; esac
        return "$status"
    }
    case ${PROMPT_COMMAND-} in
    *__appine_prompt*) ;;
    *) PROMPT_COMMAND="__appine_prompt${PROMPT_COMMAND:+;$PROMPT_COMMAND}" ;;
    esac
fi
