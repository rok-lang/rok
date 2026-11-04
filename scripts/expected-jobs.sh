#!/bin/sh
# Print expected-jobs.json: the name of every job a ci.yml run must pass,
# as ci.yml's plan picked them, and whether the run is a full one (every
# host and job, no filter). release-ready.sh reads it from the run.
#
# Reads the plan from the environment: TIER, EVENT, JOBS (words), FILTER,
# and the JSON lists ALL (every host), PICKED, GATE, CROSS, OPTS, WASM,
# BINDGEN, DIST, PREFIXES. The names follow the jobs' `name:` in ci.yml.
set -eu

jq -n --arg tier "$TIER" --arg event "$EVENT" --arg jobs "$JOBS" --arg filter "$FILTER" \
  --argjson all "$ALL" --argjson picked "$PICKED" --argjson gate "$GATE" \
  --argjson cross "$CROSS" --argjson opts "$OPTS" --argjson wasm "$WASM" \
  --argjson bindgen "$BINDGEN" --argjson dist "$DIST" --argjson prefixes "$PREFIXES" '
  ([$jobs | splits("[\\s,]+") | select(. != "")]) as $j
  | {
      contract: "expected-jobs/1",
      tier: $tier,
      event: $event,
      hosts: [$picked[].name],
      jobs: $j,
      opts: $opts,
      filter: $filter,
      full: ($tier == "nightly" and $filter == ""
        and ([$picked[].name] | sort) == ([$all[].name] | sort)
        and ($j | sort) == (["gate", "cross", "wasm", "bindgen", "dist"] | sort)),
      names: (
        ["lint", "plan"]
        + [$prefixes[] | "llvm-prefix (\(.))"]
        + [$gate[] as $h
            | "check (\($h))",
              (range(1; 4) | "test (\($h) \(.)/3)"),
              ($opts[] as $o | range(1; 5) | "corpus (\($h) \($o) \(.)/4)")]
        + (if any($cross[]; .host == "linux-x64") then ["cross-link"] else [] end)
        + [$cross[] as $c | $opts[] | "cross (\($c.target) \(.))"]
        + [$wasm[] | "wasm (\(.))"]
        + [$bindgen[] | "bindgen (\(.))"]
        + [$dist[] | "dist (\(.))"]
        + (if $event == "merge_group" then ["queue-head"] else [] end)
        + ["ci-gate"])
    }'
