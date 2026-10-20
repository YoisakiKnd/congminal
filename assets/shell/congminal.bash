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
_congminal_bash_capture() {
    _congminal_bash_status=$?
    # 初次 source 之前没有本次命令的 C，解析器会忽略这个 D。
    printf '\033]133;D;%d\007' "$_congminal_bash_status"
    return "$_congminal_bash_status"
}
_congminal_bash_prompt() {
    _congminal_bash_cwd
    if [[ $PS1 != "${_congminal_bash_wrapped-}" ]]; then _congminal_bash_base=$PS1; fi
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
