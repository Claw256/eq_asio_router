#!/bin/bash
# usage: run1.sh NAME MODE SAFETY [--bypass]
name=$1; mode=$2; safety=$3; shift 3
cd /c/Projects/eq_asio_router/measure
rm -rf dump_$name; 
../target/release/capture-probe.exe tone 14 > tone_$name.log 2>&1 &
sleep 1.2
../target/release/router-cli.exe run --config "$APPDATA/eq_asio_router/config.toml" --mode $mode --safety-ms $safety --preamp-db -20 --seconds 12 --dump-dir dump_$name "$@" > run_$name.log 2>&1
wait
tail -12 run_$name.log
for s in capture_in after_resample after_eq; do ../target/release/router-cli.exe analyze dump_$name/$s.wav --skip 1.5 --tail 0.5 --channel 0; done
