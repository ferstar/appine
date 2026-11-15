# Written by Appine into new systems, and replaced by Appine's later versions of it as long as it is
# left unchanged: a copy you edit is kept as it is.
#
# Shows the motd, sets the terminal and language, and gives bash, busybox ash and zsh a coloured
# prompt with shell integration: OSC 133 marks telling Appine's terminal where each prompt, the
# command typed at it and the command's output are. With them a tab waiting at its prompt closes
# without asking, Ctrl+Shift+Up and Down scroll to the previous and the next prompt, a long press
# in a command's output offers to select all of it, and a tap on the command being typed moves the
# cursor there (cl=line: the terminal sends left and right arrows). bash and zsh put the marks back
# around whatever PS1 is before each prompt: bash from /etc/profile.d/appine.bashrc, which this file
# reads, and on Alpine every interactive bash too (/etc/bash/appine.sh), so nested ones are marked;
# ash gets them from /etc/profile.d/appine.ashrc, which ENV names so that ash reads it again after
# ~/.profile and in nested shells.
# Before each primary prompt the title returns to the current directory. bash and zsh report the
# submitted command before execution; ash has no preexec hook. A program can replace the title
# with OSC 0/2 until the next prompt. Control characters in paths and commands are replaced by ?.
cat /etc/motd 2>/dev/null
export TERM=xterm-256color
export LANG=C.UTF-8
export NMAP_UNPRIVILEGED=1
# Interactive ash (and dash, which the file ignores) reads the file ENV names; one already set is
# the user's
if [ -z "${ENV-}" ] && [ -r /etc/profile.d/appine.ashrc ]; then
    export ENV=/etc/profile.d/appine.ashrc
fi
if [ -n "${BASH_VERSION-}" ]; then
    # Its marks come from appine.bashrc, which puts them around whatever PS1 is before each prompt
    PS1='\[\e[1;32m\]\u@\h\[\e[0m\]:\[\e[1;34m\]\w\[\e[0m\]\$ '
    if [ -r /etc/profile.d/appine.bashrc ]; then . /etc/profile.d/appine.bashrc; fi
elif [ -n "${BB_ASH_VERSION-}" ]; then
    # ash runs nothing before a command or a prompt, so its marks are in PS1 and PS2 themselves:
    # appine.ashrc puts them there, now and again when ash reads ENV
    PS1='\[\e[1;32m\]\u@\h\[\e[0m\]:\[\e[1;34m\]\w\[\e[0m\]\$ '
    if [ -r /etc/profile.d/appine.ashrc ]; then . /etc/profile.d/appine.ashrc; fi
elif [ -n "${ZSH_VERSION-}" ]; then
    # zsh reads this file where its /etc/zsh/zprofile sources /etc/profile (Alpine's does, in sh
    # emulation; Debian's does not). The other shells parse this branch too, so what only zsh
    # understands (arrays) is inside eval
    PS1='%B%F{green}%n@%m%f%b:%B%F{blue}%~%f%b%(!.#.$) '
    # Before each prompt: the end of the last command with its status (D); the marks around PS1
    # (A: the prompt starts, its cursor moved with the arrows; B: the input starts) and PS2 are
    # added again whenever something replaced them (~/.zshrc, a theme), and are sent again at
    # each redraw, as zle redraws the whole prompt. The function moves itself last, to run after
    # the hooks of themes that set PS1 before each prompt
    __appine_precmd() {
        local ret=$?
        emulate -L zsh
        printf '\033]133;D;%s\007' "$ret"
        printf '\033]2;%s\007' "${PWD//[[:cntrl:]]/?}"
        case $PS1 in *'133;B'*) ;; *) PS1=$'%{\e]133;A;cl=line\a%}'$PS1$'%{\e]133;B\a%}' ;; esac
        case $PS2 in *'133;B'*) ;; *) PS2=$'%{\e]133;P;k=s\a%}'$PS2$'%{\e]133;B\a%}' ;; esac
        eval '[[ $precmd_functions[-1] == __appine_precmd ]] ||
            precmd_functions=(${precmd_functions:#__appine_precmd} __appine_precmd)'
    }
    # Before each command: the start of its output (C), then the literal submitted command.
    __appine_preexec() {
        emulate -L zsh
        printf '\033]133;C\007'
        printf '\033]2;%s\007' "${1//[[:cntrl:]]/?}"
    }
    eval 'precmd_functions=(${precmd_functions:#__appine_precmd} __appine_precmd)
        preexec_functions=(${preexec_functions:#__appine_preexec} __appine_preexec)'
fi
