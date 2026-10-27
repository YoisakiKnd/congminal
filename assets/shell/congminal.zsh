# 可选：在 ~/.zshrc 最后 source 本文件。只支持交互 shell，不安装、不执行历史命令。
[[ -o interactive && -z ${CONGMINAL_ZSH_INTEGRATION-} ]] || return 0
typeset -g CONGMINAL_ZSH_INTEGRATION=1
autoload -Uz add-zsh-hook

_congminal_zsh_cwd() {
    # 字节百分号编码，不调用外部进程；不把路径中的控制序列送到终端。
    local LC_ALL=C value=$PWD out='' byte ch i
    for (( i=1; i<=${#value}; i++ )); do
        ch=${value[i]}
        case "$ch" in
            [a-zA-Z0-9/._~-]) out+=$ch ;;
            *) printf -v byte '%%%02X' "'$ch"; out+=$byte ;;
        esac
    done
    printf '\033]7;file://%s%s\007' "${HOST//[^a-zA-Z0-9._-]/}" "$out"
}
_congminal_zsh_preexec() {
    typeset -g _congminal_zsh_running=1
    printf '\033]133;C\007'
}
# 可选的美化提示符：`CONGMINAL_PROMPT=pretty` 才替换 PROMPT，默认保留用户自己的提示符。
# 分支名每 2 秒最多查一次 git；`CONGMINAL_PROMPT_GIT=0` 彻底关掉。
_congminal_zsh_branch() {
    [[ ${CONGMINAL_PROMPT_GIT-1} == 0 ]] && return 0
    local now=$EPOCHSECONDS
    if [[ ${_congminal_zsh_branch_at-} == "$now" ]]; then return 0; fi
    typeset -g _congminal_zsh_branch_at=$now
    typeset -g _congminal_zsh_branch_name=''
    (( $+commands[git] )) || return 0
    local name
    name=$(git symbolic-ref --short HEAD 2>/dev/null) || name=$(git rev-parse --short HEAD 2>/dev/null) || name=''
    typeset -g _congminal_zsh_branch_name=$name
}
_congminal_zsh_pretty() {
    _congminal_zsh_branch
    local branch='' status_mark=''
    [[ -n ${_congminal_zsh_branch_name-} ]] && branch=" [${_congminal_zsh_branch_name}]"
    [[ ${_congminal_zsh_status-0} != 0 ]] && status_mark=" (!${_congminal_zsh_status})"
    typeset -g _congminal_zsh_pretty_line="%F{242}%n@%m%f %~${branch}${status_mark}"$'\n'"%# "
}
_congminal_zsh_precmd() {
    local status_code=$?
    if [[ ${_congminal_zsh_running-0} == 1 ]]; then
        printf '\033]133;D;%d\007' "$status_code"
    fi
    typeset -g _congminal_zsh_running=0
    typeset -g _congminal_zsh_status=$status_code
    _congminal_zsh_cwd
    # 重复 precmd/异步重画不嵌套包装；主题变更后采用新 PS1。
    if [[ ${CONGMINAL_PROMPT-} == pretty ]]; then
        _congminal_zsh_pretty
        typeset -g _congminal_zsh_base=$_congminal_zsh_pretty_line
    elif [[ $PS1 != "${_congminal_zsh_wrapped-}" ]]; then
        typeset -g _congminal_zsh_base=$PS1
    fi
    typeset -g _congminal_zsh_wrapped=$'%{\e]133;A\a%}'"$_congminal_zsh_base"$'%{\e]133;B\a%}'
    PS1=$_congminal_zsh_wrapped
    return 0
}
# zsh 在每个 hook 中保留原始 $?；不替换用户的 precmd/preexec 函数。
add-zsh-hook precmd _congminal_zsh_precmd
add-zsh-hook preexec _congminal_zsh_preexec
