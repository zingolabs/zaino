  #!/usr/bin/env bash
  set -u

  # How many runs, where to put failure logs, and what to run.
  # Override with env vars: N_RUNS=10 CARGO_NEXTEST_ARGS="-p zaino-state" ./thousand_tests.sh
  N_RUNS=${N_RUNS:-1000}
  OUTDIR=${OUTDIR:-thousand_tests}
  CARGO_NEXTEST_ARGS=${CARGO_NEXTEST_ARGS:-"--no-fail-fast"}

  mkdir -p "$OUTDIR"

  pass=0
  fail=0
  trap 'printf "\nInterrupted at run %d. Summary: %d passed, %d failed.\n" "$run" "$pass" "$fail"; exit 130' INT TERM

  for run in $(seq 1 "$N_RUNS"); do
      ts=$(date +%Y%m%d-%H%M%S-%N)
      log=$(mktemp --tmpdir nextest.XXXXXX.log)

      if cargo nextest run $CARGO_NEXTEST_ARGS >"$log" 2>&1; then
          pass=$((pass + 1))
          printf '[run %4d/%d] PASS  (pass=%d fail=%d)\n' "$run" "$N_RUNS" "$pass" "$fail"
      else
          fail=$((fail + 1))
          printf '[run %4d/%d] FAIL  (pass=%d fail=%d) -> extracting\n' "$run" "$N_RUNS" "$pass" "$fail"

          # Split the run log into per-test-failure files.
          # Each FAIL section is delimited by the next PASS/FAIL line or the
          # summary "────" divider.
          awk -v ts="$ts" -v dir="$OUTDIR" '
              function close_current() {
                  if (current_file != "") { close(current_file); current_file = "" }
              }
              /^[[:space:]]+FAIL \[/ {
                  close_current()
                  name = $NF
                  gsub(/::/, "__", name)
                  gsub(/[^A-Za-z0-9_-]/, "_", name)
                  current_file = dir "/" name "-" ts ".log"
                  print > current_file
                  next
              }
              /^[[:space:]]+PASS \[/ { close_current(); next }
              /^────+$/            { close_current(); next }
              current_file != ""   { print >> current_file }
              END { close_current() }
          ' "$log"
      fi

      rm -f "$log"
  done

  printf '\nSummary: %d passed, %d failed out of %d runs\n' "$pass" "$fail" "$N_RUNS"
