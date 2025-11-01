#!/bin/sh
file="$1"
target="$2"       # block name to process
replace="$3"      # replacement string
tmp="$(mktemp)"

awk -v target="$target" -v repl="$replace" '
/<!-- START DUPLICATE/ {
    split($0,a," ")
    current=a[4]
    inblock=(current==target)
    if (inblock) block=""
    print
    next
}
/<!-- END DUPLICATE/ {
    inblock=0
    print
    next
}
inblock {
    block = block $0 "\n"
    print
    next
}
/<!-- DUPLICATE HERE/ {
    split($0,a," ")
    name=a[4]
    if (name==target) {
        copy=block
        gsub("a1", repl, copy)
        printf "%s", copy
    }
}
{ print }
' "$file" > "$tmp" && mv "$tmp" "$file"
