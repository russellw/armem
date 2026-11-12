#!/bin/bash
# Run gRPC Ruby's active_call_spec.rb over and over, looking for the server
# shutdown hang seen on linux arm64 (grpc/grpc PRs #40770, #41223, #41510).
# When a run hangs, dump every thread's stack with gdb, then kill it.
#
# armem showed that f01a138e8 (#41510) fixes no ARM-only bug, so the cause
# is still unknown; a backtrace from a hung run says where to look next.
# .github/workflows/grpc-ruby-hang.yml runs this on GitHub's arm64 runners.
#
# usage: grpc_ruby_hang.sh RUBY_DIR VERSION RUNS COPIES OUT_DIR [TIMEOUT]
#
# RUBY_DIR is a gRPC checkout's src/ruby (only spec/ is needed). VERSION is
# the grpc gem to load. COPIES loops run in parallel, RUNS runs each. A run
# still going after TIMEOUT seconds (default 60; a normal run takes under
# 1 s) counts as a hang. Exits 1 if any run hangs or fails.

set -u
ruby_dir=$1 version=$2 runs=$3 copies=$4 out=$5 limit=${6:-60}
mkdir -p "$out"
out=$(cd "$out" && pwd)

gdb_cmd=(gdb)
[ "$(id -u)" != 0 ] && sudo -n true 2>/dev/null && gdb_cmd=(sudo -n gdb)

one_copy() {
	local c=$1 i pid wd rc log
	for i in $(seq "$runs"); do
		log=$out/run-$c.txt
		rm -f "$out/gdb-$c.txt"
		ruby -e "gem 'grpc', '$version'; load Gem.bin_path('rspec-core', 'rspec')" \
			-- spec/generic/active_call_spec.rb >"$log" 2>&1 &
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
			echo "copy $c run $i: HANG"
			mv "$out/gdb-$c.txt" "$out/hang-$c-$i-gdb.txt"
			mv "$log" "$out/hang-$c-$i.txt"
		elif [ "$rc" != 0 ]; then
			echo "copy $c run $i: FAIL rc=$rc"
			mv "$log" "$out/fail-$c-$i.txt"
		fi
	done
	rm -f "$out/run-$c.txt"
	echo "copy $c: done $runs runs"
}

cd "$ruby_dir" || exit 2
for c in $(seq "$copies"); do
	one_copy "$c" &
done
wait

n=$(find "$out" -name 'hang-*-gdb.txt' -o -name 'fail-*' | wc -l)
echo "$n hung or failed runs out of $((runs * copies))"
[ "$n" = 0 ]
