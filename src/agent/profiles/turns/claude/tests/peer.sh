# Scripted 2.1.296 writer shape, in a temporary ASCII project chosen by the tests.
set -eu
[ "$#" = 2 ] && [ "$1" = --session-id ] || exit 1
initial=$2
current=$initial
project=$(pwd -P)
root=$(cd "${CLAUDE_CONFIG_DIR:-$HOME/.claude}" && pwd -P)
slug=$(printf '%s' "$project" | LC_ALL=C tr -c '[:alnum:]' '-')
dir="$root/projects/$slug"
mkdir -p "$root/sessions" "$dir"
started=$(/usr/bin/perl -MTime::HiRes=time -e 'printf "%.0f", time()*1000')
witness="$root/sessions/$$.json"
bind_session() {
    file="$dir/$current.jsonl"
    if [ ! -e "$file" ]; then
        printf '{"type":"mode","mode":"normal","sessionId":"%s"}\n' "$current" > "$file"
    fi
    printf '{"pid":%d,"sessionId":"%s","cwd":"%s","startedAt":%s,"version":"2.1.296","kind":"interactive","entrypoint":"cli","pidDomain":"darwin"}\n' \
        "$$" "$current" "$project" "$started" > "$witness.new"
    mv "$witness.new" "$witness"
}
if [ "${HOLD_WRITER_RECORD:-}" = yes ]; then
    : > "$HOME/awaiting-bind"
    printf '{"type":"mode","mode":"normal","sessionId":"%s"}\nhistorical bytes preceding the writer record\n' \
        "$current" > "$dir/$current.jsonl"
    IFS= read -r release && [ "$release" = bind ] || exit 1
fi
bind_session
printf '%s\n' "$witness" > "$HOME/started.new"
mv "$HOME/started.new" "$HOME/started"
stty -echoctl
printf '\033[2J\033[Hpeer ready\r\n> '
n=0
while IFS= read -r line; do
    case "$line" in
        append) cat "$HOME/batch" >> "$file" ;;
        foreign) cat "$HOME/batch" >> "$dir/$(cat "$HOME/foreign").jsonl" ;;
        *'/clear'*) current=$(cat "$HOME/next"); bind_session ;;
        exit-peer) exit 0 ;;
    esac
    n=$((n+1))
    printf '%s' "$n" > "$HOME/ack.new"
    mv "$HOME/ack.new" "$HOME/ack"
    printf '\033[2J\033[Haccepted input\r\n> '
done
