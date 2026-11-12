#!/bin/bash
# Run gRPC Ruby's specs over and over, looking for the server shutdown hang
# seen on linux arm64 in active_call_spec.rb (grpc/grpc PRs #40770, #41223,
# #41510). When a run hangs, dump every thread's stack with gdb, then kill it.
#
# armem showed that f01a138e8 (#41510) fixes no ARM-only bug, so the cause
# is still unknown; a backtrace from a hung run says where to look next.
# .github/workflows/grpc-ruby-hang.yml runs this on GitHub's arm64 runners.
#
# usage: grpc_ruby_hang.sh RUBY_DIR VERSION ROUNDS COPIES OUT_DIR [TIMEOUT] [SPEC...]
#
# RUBY_DIR is a gRPC checkout's src/ruby (spec/ and pb/ are needed). VERSION
# is the grpc gem to load. Each spec file runs in a process of its own, as
# in gRPC's CI (tools/run_tests/run_tests.py). COPIES loops run in parallel,
# each going through the specs ROUNDS times in its own shuffled order, so
# that each spec runs alongside the others, as with CI's --inner_jobs. The
# specs default to spec/generic/active_call_spec.rb. A run still going after
# TIMEOUT seconds (default 120; the slowest spec takes about 4 s) counts as
# a hang. Exits 1 if any run hangs or fails.

set -u
ruby_dir=$1 version=$2 rounds=$3 copies=$4 out=$5 limit=${6:-120}
shift 5
[ $# -gt 0 ] && shift
specs=("$@")
[ ${#specs[@]} = 0 ] && specs=(spec/generic/active_call_spec.rb)
mkdir -p "$out"
out=$(cd "$out" && pwd)

gdb_cmd=(gdb)
[ "$(id -u)" != 0 ] && sudo -n true 2>/dev/null && gdb_cmd=(sudo -n gdb)

one_copy() {
	local c=$1 r spec name pid wd rc log n=0
	for r in $(seq "$rounds"); do
		for spec in $(printf '%s\n' "${specs[@]}" | shuf); do
			n=$((n + 1))
			name=$(basename "$spec" .rb)-$c-$n
			log=$out/run-$c.txt
			rm -f "$out/gdb-$c.txt"
			ruby -e "gem 'grpc', '$version'; load Gem.bin_path('rspec-core', 'rspec')" \
				-- "$spec" >"$log" 2>&1 </dev/null &
			pid=$!
			(
				sleep "$limit"
				kill -0 "$pid" 2>/dev/null || exit
				"${gdb_cmd[@]}" -p "$pid" -batch -ex 'info threads' \
					-ex 'thread apply all bt' >"$out/gdb-$c.txt" 2>&1
				kill -9 "$pid"
			) 2>/dev/null &
			wd=$!
			{ wait "$pid"; } 2>/dev/null
			rc=$?
			pkill -P "$wd" 2>/dev/null
			kill "$wd" 2>/dev/null
			wait "$wd" 2>/dev/null
			if [ -e "$out/gdb-$c.txt" ]; then
				echo "HANG $spec (copy $c run $n)"
				mv "$out/gdb-$c.txt" "$out/hang-$name-gdb.txt"
				mv "$log" "$out/hang-$name.txt"
			elif [ "$rc" != 0 ]; then
				echo "FAIL rc=$rc $spec (copy $c run $n)"
				mv "$log" "$out/fail-$name.txt"
			fi
		done
	done
	rm -f "$out/run-$c.txt"
	echo "copy $c: done $n runs"
}

cd "$ruby_dir" || exit 2
for c in $(seq "$copies"); do
	one_copy "$c" &
done
wait

hangs=$(find "$out" -name 'hang-*-gdb.txt' | wc -l)
fails=$(find "$out" -name 'fail-*' | wc -l)
echo "$hangs hung and $fails failed out of $((rounds * copies * ${#specs[@]})) runs"
[ "$hangs" = 0 ] && [ "$fails" = 0 ]
