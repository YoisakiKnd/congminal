# 可选：在 ~/.bashrc 最后 source 本文件。PS0 需要 Bash >= 4.4。
[[ $- == *i* && -z ${CONGMINAL_BASH_INTEGRATION-} ]] || return 0
if (( BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 4) )); then
    printf 'congminal: Shell 集成需要 Bash 4.4+；当前 shell 仍可正常使用。\n' >&2
    return 0
fi
CONGMINAL_BASH_INTEGRATION=1
_congminal_bash_cwd() {
    local LC_ALL=C value=$PWD out='' byte ch i
    for (( i=0; i<${#value}; i++ )); do
        ch=${value:i:1}
        case "$ch" in
            [a-zA-Z0-9/._~-]) out+=$ch ;;
            *) printf -v byte '%%%02X' "'$ch"; out+=$byte ;;
        esac
    done
    printf '\033]7;file://%s%s\007' "${HOSTNAME//[^a-zA-Z0-9._-]/}" "$out"
}
# 可选的美化提示符：`CONGMINAL_PROMPT=pretty` 才替换 PS1，默认保留用户自己的提示符。
# 分支名每 2 秒最多查一次 git（同一个提示符反复重画不重复起子进程）；`CONGMINAL_PROMPT_GIT=0` 彻底关掉。
_congminal_bash_branch() {
    [[ ${CONGMINAL_PROMPT_GIT-1} == 0 ]] && return 0
    local now=$SECONDS
    if [[ ${_congminal_bash_branch_at-} == "$now" ]]; then return 0; fi
    _congminal_bash_branch_at=$now
    _congminal_bash_branch_name=''
    command -v git >/dev/null 2>&1 || return 0
    local name
    name=$(git symbolic-ref --short HEAD 2>/dev/null) || name=$(git rev-parse --short HEAD 2>/dev/null) || name=''
    _congminal_bash_branch_name=$name
}
_congminal_bash_pretty() {
    _congminal_bash_branch
    local where=${PWD/#$HOME/~} branch='' status_mark=''
    [[ -n ${_congminal_bash_branch_name-} ]] && branch=" [${_congminal_bash_branch_name}]"
    [[ ${_congminal_bash_status-0} != 0 ]] && status_mark=" (!${_congminal_bash_status})"
    _congminal_bash_pretty_line="\[\e[2m\]\u@\h\[\e[0m\] ${where}${branch}${status_mark}\n\$ "
}
_congminal_bash_capture() {
    _congminal_bash_status=$?
    # 初次 source 之前没有本次命令的 C，解析器会忽略这个 D。
    printf '\033]133;D;%d\007' "$_congminal_bash_status"
    return "$_congminal_bash_status"
}
_congminal_bash_prompt() {
    _congminal_bash_cwd
    if [[ ${CONGMINAL_PROMPT-} == pretty ]]; then
        _congminal_bash_pretty
        _congminal_bash_base=$_congminal_bash_pretty_line
    elif [[ $PS1 != "${_congminal_bash_wrapped-}" ]]; then
        _congminal_bash_base=$PS1
    fi
    _congminal_bash_wrapped='\[\e]133;A\a\]'"$_congminal_bash_base"'\[\e]133;B\a\]'
    PS1=$_congminal_bash_wrapped
    return "$_congminal_bash_status"
}
# 数组 hook (Bash 5.1+) 和字符串 hook 均保留，不安装 DEBUG trap。
if [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == 'declare -a '* ]]; then
    PROMPT_COMMAND=(_congminal_bash_capture "${PROMPT_COMMAND[@]}" _congminal_bash_prompt)
else
    PROMPT_COMMAND="_congminal_bash_capture; ${PROMPT_COMMAND:+$PROMPT_COMMAND; }_congminal_bash_prompt"
fi
PS0='\e]133;C\a'"${PS0-}"
