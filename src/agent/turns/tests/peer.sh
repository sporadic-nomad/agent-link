# Synthetic writer in a private test home. A per-launch witness path is passed to
# exactly one peer, independently of the shared project and transcript directory.
set -eu
[ "$1" = --session-id ]
initial=$2
current=$initial
revision=0
root=$(cd "$TURN_ROOT" && pwd -P)
witness() {
    printf '%s\n' "$initial" "$current" "$revision" "$root" "$(pwd -P)" > "$WRITER.new"
    mv "$WRITER.new" "$WRITER"
}
witness
stty -echoctl
printf '\033[2J\033[Hpeer ready\r\n> '
while IFS= read -r line; do
    notice='accepted input'
    case "$line" in
        *'/clear'*)
            if [ -f "$WRITER.refuse" ]; then
                notice='reset refused'
            else
                revision=$((revision + 1))
                current=$initial-$revision
                witness
            fi ;;
        *'/resume'*)
            revision=$((revision + 1))
            current=$initial
            witness ;;
        *'record-turn'*)
            printf '"user"\n{"end":"%s"}\n' "$(cat "$WRITER.reply")" >> "$root/$current.jsonl" ;;
        *'show-dialog'*) notice='choose an option' ;;
        *'park-monitor'*) notice='monitor barrier' ;;
        *'exit-peer'*) exit 0 ;;
    esac
    printf '\033[2J\033[H%s\r\n> ' "$notice"
done
