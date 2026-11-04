# shellcheck shell=sh
# Sourced by release-prep.sh, changelog-lint.sh and pr-lint.sh.
#
# A PR description carries one changelog: block, each line a kind and the
# text as it reads in CHANGELOG.md:
#
#   changelog:
#   - fixed: a closed field no longer marks its siblings closed
#   - silent: ci only
#
# Kinds are added, changed, fixed, removed and silent (no CHANGELOG line).
# An indented line continues the entry above it.

changelog_kinds="added, changed, fixed, removed or silent"

# The PRs merged in git range $1, oldest first: first-parent commits whose
# subject ends in (#N) (squash) or starts with "Merge pull request #N".
# Release commits have no PR number and are skipped.
changelog_prs() {
  git log --first-parent --reverse --format=%s "$1" -- |
    sed -n -e 's/.*(#\([0-9][0-9]*\))$/\1/p' -e 's/^Merge pull request #\([0-9][0-9]*\) .*/\1/p'
}

# The changelog: block of the PR description in file $1 as `kind<TAB>text`
# lines. HTML comments are not part of it. Problems go to stderr, one per
# line; returns 1 on any.
changelog_entries() {
  tr -d '\r' < "$1" | awk -v kinds="$changelog_kinds" '
    function flush(  k, t) {
      if (!have) return
      have = 0
      n++
      k = ""
      t = cur
      if (match(cur, /^[A-Za-z]+:/)) {
        k = tolower(substr(cur, 1, RLENGTH - 1))
        t = substr(cur, RLENGTH + 1)
      }
      sub(/^[[:space:]]+/, "", t)
      sub(/[[:space:]]+$/, "", t)
      if (k !~ /^(added|changed|fixed|removed|silent)$/) {
        print "entry \"" cur "\" has no kind; start it with " kinds ":" > "/dev/stderr"
        bad = 1
      } else if (t == "" && k != "silent") {
        print "entry \"" cur "\" is empty; say what changed after " k ":" > "/dev/stderr"
        bad = 1
      } else {
        print k "\t" t
      }
    }
    {
      line = $0
      out = ""
      had = incomment
      while (line != "") {
        if (incomment) {
          i = index(line, "-->")
          if (!i) break
          line = substr(line, i + 3)
          incomment = 0
        } else {
          i = index(line, "<!--")
          if (!i) {
            out = out line
            break
          }
          had = 1
          out = out substr(line, 1, i - 1)
          line = substr(line, i + 4)
          incomment = 1
        }
      }
      if (had && out ~ /^[[:space:]]*$/) next
      $0 = out
    }
    done { next }
    !inblock {
      if (tolower($0) ~ /^changelog:[[:space:]]*$/) inblock = found = 1
      next
    }
    /^-[[:space:]]*$/ || /^- / {
      flush()
      cur = $0
      sub(/^-[[:space:]]*/, "", cur)
      have = 1
      next
    }
    have && /^[[:space:]]+[^[:space:]]/ {
      sub(/^[[:space:]]+/, "")
      cur = cur " " $0
      next
    }
    {
      flush()
      done = 1
    }
    END {
      flush()
      if (!found) {
        print "no changelog: block; end the description with one (.github/PULL_REQUEST_TEMPLATE.md)" > "/dev/stderr"
        exit 1
      }
      if (!n) {
        print "the changelog: block has no entries; add one line per change, or \"- silent: <why>\"" > "/dev/stderr"
        exit 1
      }
      exit bad
    }
  '
}

# The newest final release tag reachable from $1, other than v$2.
changelog_prev_tag() {
  git describe --tags --abbrev=0 --match 'v[0-9]*' --exclude '*-*' --exclude "v$2" "$1" 2> /dev/null
}
